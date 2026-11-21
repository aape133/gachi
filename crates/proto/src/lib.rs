//! Wire formats shared by the gachi client and server.
//!
//! - [`audio`]: the UDP packet format (audio, ping, hole punching, encryption envelope)
//! - [`signal`]: JSON messages with the connection server (Cloudflare Worker)
//! - [`peer`]: room messages sent peer to peer (chat, metronome, …)
//! - [`code`]: room codes

pub mod audio;
pub mod code;
pub mod peer;
pub mod signal;

/// Peer id, assigned by the room host (the room creator is 1). `0` means "nobody".
pub type PeerId = u16;

/// Bytes as lowercase hex (keys and sealed blobs in JSON messages).
pub fn to_hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 15) as usize] as char);
    }
    s
}

/// Hex back to bytes; `None` if it is not hex.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    s.as_chunks::<2>()
        .0
        .iter()
        .map(|[h, l]| Some(digit(*h)? << 4 | digit(*l)?))
        .collect()
}

/// A 32-byte key from hex.
pub fn key_from_hex(s: &str) -> Option<[u8; 32]> {
    from_hex(s)?.try_into().ok()
}

#[cfg(test)]
mod hex_tests {
    #[test]
    fn round_trips() {
        let b = [0u8, 1, 0xab, 0xff];
        assert_eq!(super::to_hex(&b), "0001abff");
        assert_eq!(super::from_hex("0001ABff").unwrap(), b);
        assert!(super::from_hex("abc").is_none());
        assert!(super::from_hex("zz").is_none());
        assert!(super::key_from_hex("00").is_none());
        assert_eq!(super::key_from_hex(&"11".repeat(32)), Some([0x11; 32]));
    }
}
