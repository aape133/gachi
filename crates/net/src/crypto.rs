//! End-to-end encryption between peers.
//!
//! - Each session makes a fresh X25519 key pair; the public key goes to the room through
//!   signaling with our candidates.
//! - For each peer: shared secret → HKDF-SHA256 → one key per direction (so the two sides
//!   never use the same key and nonce).
//! - The cipher is AES-256-GCM when both machines have AES in hardware (AES-NI + PCLMULQDQ
//!   on x86, the crypto extensions on ARM; nearly every PC since 2011-2015), else
//!   ChaCha20-Poly1305, which is fast without it. Both are equally strong; on a recent desktop CPU
//!   a 32-frame 24-bit stereo packet (232 bytes) seals in ~0.19 µs with AES, ~0.47 µs with
//!   ChaCha20 (with poly1305's plain backend, see `.cargo/config.toml`; its AVX2 one took
//!   ~1.3 µs on packets this small). Each side announces its support with its
//!   key and both pick by the same rule ([`Cipher::pick`]); the pick goes into the key
//!   derivation, so if they ever disagree no packet opens (no garbled audio).
//! - Nonce = a 64-bit send counter (never reused for a key), carried in the packet header,
//!   which is authenticated too.
//! - The receiver keeps a sliding window of seen counters, so recorded packets cannot be
//!   replayed.
//! - Both sides can show a short **safety code** made from the two public keys. If it
//!   matches on both screens, nobody (not even the signaling server) swapped the keys.
//!
//! Sealing runs on the audio thread: in place, no allocation, no locks.

use aes_gcm::Aes256Gcm;
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce, Tag};
use gachi_proto::audio::{self, SEALED_HEADER, TAG_LEN};
use gachi_proto::PeerId;
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use x25519_dalek::{PublicKey, StaticSecret};

/// Whether this CPU runs AES-GCM in hardware (checked once).
pub fn aes_in_hardware() -> bool {
    static FAST: OnceLock<bool> = OnceLock::new();
    *FAST.get_or_init(|| {
        #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
        let fast =
            std::is_x86_feature_detected!("aes") && std::is_x86_feature_detected!("pclmulqdq");
        #[cfg(target_arch = "aarch64")]
        let fast = std::arch::is_aarch64_feature_detected!("aes");
        #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
        let fast = false;
        fast
    })
}

/// A peer's public key and whether its CPU has AES in hardware (both from the room).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerKey {
    pub public: [u8; 32],
    pub aes: bool,
}

/// The cipher audio to and from one peer uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cipher {
    Aes256Gcm,
    ChaCha20Poly1305,
}

impl Cipher {
    /// AES when both sides have it in hardware, else ChaCha20. Both sides compute this from
    /// the same two facts, so they agree.
    pub fn pick(mine: bool, theirs: bool) -> Self {
        if mine && theirs {
            Self::Aes256Gcm
        } else {
            Self::ChaCha20Poly1305
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Aes256Gcm => "AES-256-GCM",
            Self::ChaCha20Poly1305 => "ChaCha20-Poly1305",
        }
    }

    /// HKDF info prefix. ChaCha20 keeps the original one, so it still talks to older builds.
    fn label_for_keys(self) -> &'static [u8] {
        match self {
            Self::Aes256Gcm => b"gachi v1 audio aes-256-gcm",
            Self::ChaCha20Poly1305 => b"gachi v1 audio",
        }
    }
}

/// One direction's key.
enum Aead {
    Aes(Box<Aes256Gcm>),
    ChaCha(ChaCha20Poly1305),
}

impl Aead {
    fn new(cipher: Cipher, key: &[u8]) -> Self {
        match cipher {
            Cipher::Aes256Gcm => Self::Aes(Box::new(Aes256Gcm::new(Key::from_slice(key)))),
            Cipher::ChaCha20Poly1305 => Self::ChaCha(ChaCha20Poly1305::new(Key::from_slice(key))),
        }
    }

    fn seal(&self, nonce: &Nonce, aad: &[u8], buf: &mut [u8]) -> Tag {
        let r = match self {
            Self::Aes(k) => k.encrypt_in_place_detached(nonce, aad, buf),
            Self::ChaCha(k) => k.encrypt_in_place_detached(nonce, aad, buf),
        };
        r.expect("payload is far below either cipher's length limit")
    }

    fn open(&self, nonce: &Nonce, aad: &[u8], buf: &mut [u8], tag: &Tag) -> bool {
        match self {
            Self::Aes(k) => k.decrypt_in_place_detached(nonce, aad, buf, tag).is_ok(),
            Self::ChaCha(k) => k.decrypt_in_place_detached(nonce, aad, buf, tag).is_ok(),
        }
    }
}

/// Our key pair for one session.
pub struct Identity {
    secret: StaticSecret,
    pub public: [u8; 32],
}

impl Identity {
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// Keys for talking to one peer, with the cipher both sides pick. `None` if their key is
    /// invalid (low-order point).
    pub fn session_with(
        &self,
        my_id: PeerId,
        their_id: PeerId,
        their: &PeerKey,
    ) -> Option<PeerCrypto> {
        let cipher = Cipher::pick(aes_in_hardware(), their.aes);
        self.session_using(cipher, my_id, their_id, &their.public)
    }

    /// [`Identity::session_with`] with a given cipher.
    pub fn session_using(
        &self,
        cipher: Cipher,
        my_id: PeerId,
        their_id: PeerId,
        their_public: &[u8; 32],
    ) -> Option<PeerCrypto> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*their_public));
        if !shared.was_contributory() {
            return None;
        }
        // both sides must derive the same bytes: order everything by peer id
        let (lo, hi) = if my_id < their_id {
            (&self.public, their_public)
        } else {
            (their_public, &self.public)
        };
        let mut info = Vec::with_capacity(64 + 32);
        info.extend_from_slice(cipher.label_for_keys());
        info.extend_from_slice(lo);
        info.extend_from_slice(hi);
        let mut okm = [0u8; 64];
        Hkdf::<Sha256>::new(None, shared.as_bytes())
            .expand(&info, &mut okm)
            .ok()?;
        let (lo_to_hi, hi_to_lo) = okm.split_at(32);
        let (send, recv) = if my_id < their_id {
            (lo_to_hi, hi_to_lo)
        } else {
            (hi_to_lo, lo_to_hi)
        };
        Some(PeerCrypto {
            send: Aead::new(cipher, send),
            recv: Aead::new(cipher, recv),
            cipher,
            counter: AtomicU64::new(0),
        })
    }
}

/// One twelve-digit code for the whole room, e.g. "4829 1305 7716", from every member's
/// public key. Everyone sees the same code unless someone's key was swapped on the way
/// (a man in the middle): then that person's code differs. The host reads it out, the
/// others compare.
///
/// Why 12 digits: a server in the middle would have to find keys that make the codes match.
/// Keys are new every session, so it must do that within the seconds of connecting;
/// 10^12 possibilities (~2^40) is far beyond that, while 6 digits would take about a second.
pub fn room_safety_code(members: &[(PeerId, [u8; 32])]) -> String {
    let mut sorted = members.to_vec();
    sorted.sort_by_key(|m| m.0);
    let mut h = Sha256::new().chain_update(b"gachi v1 room safety");
    for (id, key) in &sorted {
        h.update(id.to_le_bytes());
        h.update(key);
    }
    let h = h.finalize();
    let n = u64::from_le_bytes(h[..8].try_into().expect("8 bytes")) % 1_000_000_000_000;
    format!(
        "{:04} {:04} {:04}",
        n / 100_000_000,
        n / 10_000 % 10_000,
        n % 10_000
    )
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&counter.to_le_bytes());
    *Nonce::from_slice(&n)
}

/// Keys for one peer. Shared (via `Arc`) by the audio thread (sending audio) and the
/// receive thread (pings, decrypting), hence the atomic counter.
pub struct PeerCrypto {
    send: Aead,
    recv: Aead,
    cipher: Cipher,
    counter: AtomicU64,
}

impl PeerCrypto {
    pub fn cipher(&self) -> Cipher {
        self.cipher
    }

    /// Seals `plain` (an encoded inner packet) into `out` as a `Sealed` packet.
    /// `out` must hold `plain.len() + SEALED_OVERHEAD` bytes. Returns the packet length.
    pub fn seal(&self, me: PeerId, plain: &[u8], out: &mut [u8]) -> usize {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        let n = plain.len();
        audio::encode_sealed_header(out, me, counter);
        let (aad, rest) = out.split_at_mut(SEALED_HEADER);
        rest[..n].copy_from_slice(plain);
        let tag = self.send.seal(&nonce(counter), aad, &mut rest[..n]);
        rest[n..n + TAG_LEN].copy_from_slice(&tag);
        SEALED_HEADER + n + TAG_LEN
    }

    /// Decrypts a `Sealed` packet's body into `out`. Returns the inner packet's length, or
    /// `None` if it was forged, damaged or encrypted with another key.
    /// (Replay is checked separately with [`ReplayWindow`], after this succeeds.)
    pub fn open(&self, counter: u64, aad: &[u8], body: &[u8], out: &mut [u8]) -> Option<usize> {
        let n = body.len().checked_sub(TAG_LEN)?;
        let out = out.get_mut(..n)?;
        out.copy_from_slice(&body[..n]);
        let tag = Tag::from_slice(&body[n..]);
        self.recv.open(&nonce(counter), aad, out, tag).then_some(n)
    }
}

/// Remembers which counters arrived recently (WireGuard-style sliding window), so a packet
/// is accepted at most once. Allows reordering within the window.
#[derive(Default)]
pub struct ReplayWindow {
    /// highest counter seen + 1 (0 = nothing yet)
    top: u64,
    /// bit i = counter (top - 1 - i) was seen
    bits: u128,
}

impl ReplayWindow {
    const SIZE: u64 = 128;

    /// Returns true (and records it) if `counter` is new.
    pub fn accept(&mut self, counter: u64) -> bool {
        // (the last counter is never sent: taking it would wrap `top` and open the window
        // to everything seen before)
        if counter == u64::MAX {
            return false;
        }
        if counter >= self.top {
            let shift = counter + 1 - self.top;
            self.bits = if shift >= Self::SIZE {
                0
            } else {
                self.bits << shift
            };
            self.bits |= 1;
            self.top = counter + 1;
            return true;
        }
        let age = self.top - 1 - counter;
        if age >= Self::SIZE {
            return false; // too old to tell: drop
        }
        let bit = 1u128 << age;
        if self.bits & bit != 0 {
            return false;
        }
        self.bits |= bit;
        true
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_last_counter_does_not_reset_the_replay_window() {
        let mut w = super::ReplayWindow::default();
        assert!(w.accept(5));
        assert!(!w.accept(u64::MAX));
        assert!(!w.accept(5), "still remembered");
        assert!(w.accept(6));
    }

    use super::*;
    use gachi_proto::audio::{Packet, SampleFormat, MAX_PACKET, MAX_SEALED};

    fn pair_using(ca: Cipher, cb: Cipher) -> (PeerCrypto, PeerCrypto) {
        let (a, b) = (Identity::generate(), Identity::generate());
        (
            a.session_using(ca, 1, 2, &b.public).unwrap(),
            b.session_using(cb, 2, 1, &a.public).unwrap(),
        )
    }

    const BOTH: [Cipher; 2] = [Cipher::Aes256Gcm, Cipher::ChaCha20Poly1305];

    fn sealed_audio(c: &PeerCrypto) -> ([u8; MAX_SEALED], usize) {
        let mut plain = [0u8; MAX_PACKET];
        let n = audio::encode_audio(&mut plain, 1, 42, 1, SampleFormat::Pcm16, &[0.25; 32]);
        let mut wire = [0u8; MAX_SEALED];
        let len = c.seal(1, &plain[..n], &mut wire);
        (wire, len)
    }

    fn open(c: &PeerCrypto, wire: &[u8]) -> Option<usize> {
        let Ok(Packet::Sealed {
            counter, aad, body, ..
        }) = audio::decode(wire)
        else {
            return None;
        };
        let mut out = [0u8; MAX_SEALED];
        c.open(counter, aad, body, &mut out)
    }

    #[test]
    fn roundtrip_and_both_directions() {
        for c in BOTH {
            let (a, b) = pair_using(c, c);
            let (wire, len) = sealed_audio(&a);
            let n = open(&b, &wire[..len]).expect("b opens a's packet");
            assert_eq!(n, len - audio::SEALED_OVERHEAD);
            assert!(
                open(&a, &wire[..len]).is_none(),
                "directions use different keys"
            );
            let (wire, len) = sealed_audio(&b);
            assert!(open(&a, &wire[..len]).is_some());
        }
    }

    #[test]
    fn aes_only_when_both_have_it() {
        assert_eq!(Cipher::pick(true, true), Cipher::Aes256Gcm);
        assert_eq!(Cipher::pick(true, false), Cipher::ChaCha20Poly1305);
        assert_eq!(Cipher::pick(false, true), Cipher::ChaCha20Poly1305);
        assert_eq!(Cipher::pick(false, false), Cipher::ChaCha20Poly1305);
        // a has AES, b has not: each picks from its own fact and the other's, they agree
        let (a_has, b_has) = (true, false);
        let (sa, sb) = pair_using(Cipher::pick(a_has, b_has), Cipher::pick(b_has, a_has));
        assert_eq!(sa.cipher(), Cipher::ChaCha20Poly1305);
        assert_eq!(sa.cipher(), sb.cipher());
        let (wire, len) = sealed_audio(&sa);
        assert!(open(&sb, &wire[..len]).is_some());
        // two machines that both have it (like this one, most likely) get AES
        let (a, b) = (Identity::generate(), Identity::generate());
        let me = |i: &Identity| PeerKey {
            public: i.public,
            aes: aes_in_hardware(),
        };
        let (sa, sb) = (
            a.session_with(1, 2, &me(&b)).unwrap(),
            b.session_with(2, 1, &me(&a)).unwrap(),
        );
        assert_eq!(
            sa.cipher(),
            Cipher::pick(aes_in_hardware(), aes_in_hardware())
        );
        let (wire, len) = sealed_audio(&sa);
        assert!(open(&sb, &wire[..len]).is_some());
    }

    #[test]
    fn sides_that_disagree_on_the_cipher_open_nothing() {
        let (a, b) = pair_using(Cipher::Aes256Gcm, Cipher::ChaCha20Poly1305);
        let (wire, len) = sealed_audio(&a);
        assert!(open(&b, &wire[..len]).is_none());
        let (wire, len) = sealed_audio(&b);
        assert!(open(&a, &wire[..len]).is_none());
    }

    #[test]
    fn chacha_keys_match_older_builds() {
        // the ChaCha20 derivation is unchanged ("gachi v1 audio"): a packet sealed the old
        // way (same key bytes) still opens
        let (a, b) = (Identity::generate(), Identity::generate());
        let shared = a.secret.diffie_hellman(&PublicKey::from(b.public));
        let (lo, hi) = (&a.public, &b.public); // a has the lower id
        let mut info = b"gachi v1 audio".to_vec();
        info.extend_from_slice(lo);
        info.extend_from_slice(hi);
        let mut okm = [0u8; 64];
        Hkdf::<Sha256>::new(None, shared.as_bytes())
            .expand(&info, &mut okm)
            .unwrap();
        let old = ChaCha20Poly1305::new(Key::from_slice(&okm[..32]));
        let mut plain = *b"older build says hi";
        let tag = old
            .encrypt_in_place_detached(&nonce(0), b"hdr", &mut plain)
            .unwrap();
        let mut body = plain.to_vec();
        body.extend_from_slice(&tag);
        let sb = b
            .session_using(Cipher::ChaCha20Poly1305, 2, 1, &a.public)
            .unwrap();
        let mut out = [0u8; 64];
        assert_eq!(sb.open(0, b"hdr", &body, &mut out), Some(19));
        assert_eq!(&out[..19], b"older build says hi");
    }

    #[test]
    fn room_code_is_order_independent_and_catches_a_swapped_key() {
        let (k1, k2, k3) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let code = room_safety_code(&[(1, k1), (2, k2), (3, k3)]);
        assert_eq!(code, room_safety_code(&[(3, k3), (1, k1), (2, k2)]));
        assert_eq!(code.chars().filter(|c| c.is_ascii_digit()).count(), 12);
        assert_ne!(
            code,
            room_safety_code(&[(1, k1), (2, [9u8; 32]), (3, k3)]),
            "swapped key"
        );
        assert_ne!(
            code,
            room_safety_code(&[(1, k1), (2, k2)]),
            "someone missing"
        );
    }

    #[test]
    fn rejects_tampering_and_strangers() {
        for c in BOTH {
            let (a, b) = pair_using(c, c);
            let (mut wire, len) = sealed_audio(&a);
            wire[SEALED_HEADER + 5] ^= 1; // payload
            assert!(open(&b, &wire[..len]).is_none());
            let (mut wire, len) = sealed_audio(&a);
            wire[4] ^= 1; // counter (authenticated header)
            assert!(open(&b, &wire[..len]).is_none());
            let (mut wire, len) = sealed_audio(&a);
            wire[len - 1] ^= 1; // tag
            assert!(open(&b, &wire[..len]).is_none());

            let (mallory, _) = pair_using(c, c); // someone with a different key
            let (wire, len) = sealed_audio(&mallory);
            assert!(open(&b, &wire[..len]).is_none());
        }
    }

    #[test]
    fn sealing_is_cheap_enough_for_the_audio_thread() {
        // 24-bit stereo, 32 frames = 204-byte packet; 1500 packets/s per peer
        for c in BOTH {
            let (a, _) = pair_using(c, c);
            let mut plain = [0u8; MAX_PACKET];
            let n = audio::encode_audio(&mut plain, 1, 0, 2, SampleFormat::Pcm24, &[0.1; 64]);
            let mut wire = [0u8; MAX_SEALED];
            let t = std::time::Instant::now();
            for _ in 0..10_000 {
                std::hint::black_box(a.seal(1, &plain[..n], &mut wire));
            }
            let per = t.elapsed() / 10_000;
            println!("{} seal {n} bytes: {per:?}", c.label());
            assert!(
                per < std::time::Duration::from_micros(10),
                "{per:?} per packet"
            );
        }
    }

    #[test]
    fn rejects_low_order_keys() {
        assert!(Identity::generate()
            .session_with(
                1,
                2,
                &PeerKey {
                    public: [0u8; 32],
                    aes: true
                }
            )
            .is_none());
    }

    #[test]
    fn replay_window() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(0));
        assert!(!w.accept(0), "duplicate");
        assert!(w.accept(5));
        assert!(w.accept(3), "late but new");
        assert!(!w.accept(3));
        assert!(w.accept(500));
        assert!(!w.accept(5), "fell out of the window");
        assert!(w.accept(450));
        assert!(!w.accept(450));
    }
}
