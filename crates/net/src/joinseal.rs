//! Sealing the join request and the host's answer, so the connection server passes on only
//! ciphertext.
//!
//! - The host holds a join key pair (X25519) and gives the server its public half when it
//!   opens the code. The server hands it to each newcomer.
//! - The newcomer makes a one-time key pair, does Diffie-Hellman with the host's key, and
//!   seals its request (ChaCha20-Poly1305). The host's answer is sealed with a second key
//!   from the same exchange, so only that newcomer can read it.
//! - With a room password, a key stretched from the password (PBKDF2-SHA256, salted with
//!   the room code) goes into the key derivation. The password itself is never sent: a
//!   wrong password simply makes a request the host cannot open. And a server in the middle
//!   that swapped the host's key still cannot read the request or forge the answer without
//!   the password (it could only try guesses offline, which the stretching slows down).
//! - Without a password nothing is secret between newcomer and host to begin with; the
//!   room's safety code ([`crate::crypto::room_safety_code`]) is the check against a server
//!   in the middle there.
//!
//! Every key is used once (a fresh one-time key per request), so a zero nonce is safe.

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

/// PBKDF2 rounds for the room password (~30 ms in a release build; paid once per join).
pub const PASSWORD_ROUNDS: u32 = 100_000;

/// The room password stretched into a key, salted with the room code (so one table of
/// guesses does not fit every room). Slow on purpose.
pub fn password_key(code: &str, password: &str) -> [u8; 32] {
    password_key_rounds(code, password, PASSWORD_ROUNDS)
}

fn password_key_rounds(code: &str, password: &str, rounds: u32) -> [u8; 32] {
    // PBKDF2-HMAC-SHA256, one block (RFC 8018)
    let mac = <Hmac<Sha256> as Mac>::new_from_slice(password.as_bytes()).expect("any key length");
    let mut u = mac
        .clone()
        .chain_update(b"gachi v1 join password")
        .chain_update(code.as_bytes())
        .chain_update(1u32.to_be_bytes())
        .finalize()
        .into_bytes();
    let mut out: [u8; 32] = u.into();
    for _ in 1..rounds {
        u = mac.clone().chain_update(u).finalize().into_bytes();
        for (o, x) in out.iter_mut().zip(u.iter()) {
            *o ^= x;
        }
    }
    out
}

/// One direction's key (used once).
pub struct OneKey([u8; 32]);

impl OneKey {
    pub fn seal(&self, plain: &[u8]) -> Vec<u8> {
        ChaCha20Poly1305::new(Key::from_slice(&self.0))
            .encrypt(Nonce::from_slice(&[0; 12]), plain)
            .expect("in memory")
    }

    pub fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        ChaCha20Poly1305::new(Key::from_slice(&self.0))
            .decrypt(Nonce::from_slice(&[0; 12]), sealed)
            .ok()
    }
}

/// Request key and answer key from one exchange.
fn keys(
    shared: &[u8; 32],
    password: Option<&[u8; 32]>,
    eph: &[u8; 32],
    host: &[u8; 32],
) -> (OneKey, OneKey) {
    let mut ikm = [0u8; 64];
    ikm[..32].copy_from_slice(shared);
    if let Some(p) = password {
        ikm[32..].copy_from_slice(p);
    }
    let mut info = Vec::with_capacity(16 + 1 + 64);
    info.extend_from_slice(b"gachi v1 join");
    info.push(password.is_some() as u8);
    info.extend_from_slice(eph);
    info.extend_from_slice(host);
    let mut okm = [0u8; 64];
    Hkdf::<Sha256>::new(None, &ikm)
        .expand(&info, &mut okm)
        .expect("64 bytes is a valid length");
    let (a, b) = okm.split_at(32);
    (
        OneKey(a.try_into().expect("32")),
        OneKey(b.try_into().expect("32")),
    )
}

/// The host's join key pair. Lives in memory only, for as long as the app runs.
pub struct JoinKey {
    secret: StaticSecret,
    pub public: [u8; 32],
}

impl JoinKey {
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// Host: opens a newcomer's request. `password`: the room's [`password_key`], if the
    /// room has one. Returns the request and the key to seal the answer with; `None` if it
    /// does not open (wrong password, or tampered with).
    pub fn open(
        &self,
        eph: &[u8; 32],
        password: Option<&[u8; 32]>,
        sealed: &[u8],
    ) -> Option<(Vec<u8>, OneKey)> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*eph));
        if !shared.was_contributory() {
            return None;
        }
        let (req, answer) = keys(shared.as_bytes(), password, eph, &self.public);
        Some((req.open(sealed)?, answer))
    }
}

/// Newcomer: seals a request to the host's join key. Returns our one-time public key, the
/// sealed request, and the key the host's answer will be sealed with.
pub fn seal(
    host: &[u8; 32],
    password: Option<&[u8; 32]>,
    plain: &[u8],
) -> Option<([u8; 32], Vec<u8>, OneKey)> {
    let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let eph = PublicKey::from(&secret).to_bytes();
    let shared = secret.diffie_hellman(&PublicKey::from(*host));
    if !shared.was_contributory() {
        return None;
    }
    let (req, answer) = keys(shared.as_bytes(), password, &eph, host);
    Some((eph, req.seal(plain), answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pw(p: &str) -> [u8; 32] {
        password_key_rounds("K7MQ2X", p, 10)
    }

    #[test]
    fn request_and_answer_round_trip() {
        let host = JoinKey::generate();
        let (eph, sealed, answer_key) = seal(&host.public, None, b"hello").unwrap();
        assert!(!sealed.windows(5).any(|w| w == b"hello"), "ciphertext only");
        let (plain, reply) = host.open(&eph, None, &sealed).unwrap();
        assert_eq!(plain, b"hello");
        let welcome = reply.seal(b"welcome");
        assert_eq!(answer_key.open(&welcome).unwrap(), b"welcome");
        // the answer key is not the request key
        assert!(host.open(&eph, None, &welcome).is_none());
    }

    #[test]
    fn the_password_is_part_of_the_key() {
        let host = JoinKey::generate();
        let right = pw("secret");
        let (eph, sealed, answer_key) = seal(&host.public, Some(&right), b"req").unwrap();
        assert!(
            host.open(&eph, Some(&pw("wrong")), &sealed).is_none(),
            "wrong password"
        );
        assert!(
            host.open(&eph, None, &sealed).is_none(),
            "room without the password"
        );
        let (_, reply) = host.open(&eph, Some(&right), &sealed).unwrap();
        // someone without the password (a server in the middle) cannot forge the answer
        let (_, _, guess) = seal(&host.public, Some(&pw("guess")), b"x").unwrap();
        assert!(answer_key.open(&guess.seal(b"fake")).is_none());
        assert!(answer_key.open(&reply.seal(b"real")).is_some());
    }

    #[test]
    fn a_swapped_host_key_cannot_open_a_password_request() {
        // the server hands the newcomer its own key instead of the host's
        let host = JoinKey::generate();
        let mitm = JoinKey::generate();
        let (eph, sealed, _) = seal(&mitm.public, Some(&pw("secret")), b"req").unwrap();
        assert!(
            mitm.open(&eph, None, &sealed).is_none(),
            "the server does not know the password"
        );
        assert!(
            host.open(&eph, Some(&pw("secret")), &sealed).is_none(),
            "and it was not for the host"
        );
    }

    #[test]
    fn tampering_and_junk_are_refused() {
        let host = JoinKey::generate();
        let (eph, mut sealed, _) = seal(&host.public, None, b"req").unwrap();
        sealed[3] ^= 1;
        assert!(host.open(&eph, None, &sealed).is_none());
        assert!(
            host.open(&[0; 32], None, &sealed).is_none(),
            "low-order key"
        );
        assert!(host.open(&eph, None, &[]).is_none());
        assert!(seal(&[0; 32], None, b"x").is_none());
    }

    #[test]
    fn pbkdf2_matches_the_standard() {
        // RFC 7914 §11 test vector for PBKDF2-HMAC-SHA256 ("passwd", "salt", 1 round),
        // with our salt prefix taken out of the picture
        let mac = <Hmac<Sha256> as Mac>::new_from_slice(b"passwd").unwrap();
        let t = mac
            .chain_update(b"salt")
            .chain_update(1u32.to_be_bytes())
            .finalize()
            .into_bytes();
        assert_eq!(gachi_proto::to_hex(&t[..8]), "55ac046e56e3089f");
        // and the salt (room code) matters
        assert_ne!(
            password_key_rounds("AAAAAA", "pw", 3),
            password_key_rounds("BBBBBB", "pw", 3)
        );
    }
}
