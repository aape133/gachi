//! UDP packet format (little endian).
//!
//! Every packet starts with a 4-byte common header:
//! `[0] MAGIC  [1] kind  [2..4] sender PeerId`
//!
//! Audio packets add 8 bytes: `seq u32, frames u16, channels u8, format u8`, then raw PCM.
//! At 32-sample packets we send ~1500 packets/s per peer, so headers are kept tiny.
//!
//! Peers only ever talk directly (no relay): if hole punching fails, they cannot connect.
//!
//! Encryption: audio, ping and pong travel inside `Sealed` packets:
//! `[0..4] common (sender)  [4..12] counter u64  [12..] ChaCha20-Poly1305(inner packet)  [16-byte tag]`.
//! The first 12 bytes are authenticated (AAD); the counter is the nonce and never repeats
//! per key.
//! (Sealing itself lives in `gachi-net`; this crate only knows the layout.)
//!
//! Room messages between peers (chat, metronome, …) travel as `Control` packets inside
//! `Sealed`: `[0..4] common  [4..8] seq u32  [8..] payload (JSON, see `crate::peer`)`, each
//! acknowledged by a `ControlAck` with the same seq. `Bye` says "I'm leaving on purpose".
//!
//! Everything here is allocation-free so the audio thread can encode directly.
//! Decoding validates every length: these bytes come from the internet.

use crate::PeerId;


pub const MAGIC: u8 = 0xA7;
pub const COMMON_HEADER: usize = 4;
pub const AUDIO_HEADER: usize = COMMON_HEADER + 8;
/// Max frames per packet. 128 frames of 24-bit stereo = 780 bytes, well under the MTU.
pub const MAX_FRAMES: usize = 128;
pub const MAX_CHANNELS: usize = 2;
pub const MAX_PACKET: usize = AUDIO_HEADER + MAX_FRAMES * MAX_CHANNELS * 3;
pub const SEALED_HEADER: usize = COMMON_HEADER + 8;
pub const TAG_LEN: usize = 16;
pub const SEALED_OVERHEAD: usize = SEALED_HEADER + TAG_LEN;
pub const CONTROL_HEADER: usize = COMMON_HEADER + 4;
/// Largest control payload. Keeps sealed control packets under ~1200 bytes, which fits the
/// smallest IPv6 path MTU (1280) with room for headers.
pub const MAX_CONTROL_PAYLOAD: usize = 1100;
/// Largest packet that can sit inside `Sealed` (audio or control).
pub const MAX_INNER: usize = if MAX_PACKET > CONTROL_HEADER + MAX_CONTROL_PAYLOAD {
    MAX_PACKET
} else {
    CONTROL_HEADER + MAX_CONTROL_PAYLOAD
};
/// Largest sealed packet.
pub const MAX_SEALED: usize = SEALED_OVERHEAD + MAX_INNER;
/// Largest packet on the wire.
pub const MAX_WIRE: usize = MAX_SEALED;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Audio = 1,
    Ping = 2,
    Pong = 3,
    Punch = 4,
    PunchAck = 5,
    // 6–9 were the old server's address reflection and relay (retired)
    /// an encrypted inner packet (see module docs)
    Sealed = 10,
    /// a reliable room message (inside `Sealed`)
    Control = 11,
    ControlAck = 12,
    /// leaving on purpose (inside `Sealed`)
    Bye = 13,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Audio,
            2 => Self::Ping,
            3 => Self::Pong,
            4 => Self::Punch,
            5 => Self::PunchAck,
            10 => Self::Sealed,
            11 => Self::Control,
            12 => Self::ControlAck,
            13 => Self::Bye,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SampleFormat {
    Pcm16 = 1,
    Pcm24 = 2,
}

impl SampleFormat {
    /// Bytes per sample.
    pub fn bytes(self) -> usize {
        match self {
            Self::Pcm16 => 2,
            Self::Pcm24 => 3,
        }
    }
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Pcm16),
            2 => Some(Self::Pcm24),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioHeader {
    pub seq: u32,
    pub frames: u16,
    pub channels: u8,
    pub format: SampleFormat,
}

impl AudioHeader {
    pub fn samples(&self) -> usize {
        self.frames as usize * self.channels as usize
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Audio { sender: PeerId, header: AudioHeader, payload: &'a [u8] },
    Ping { sender: PeerId, nonce: u32, sent_us: u64 },
    /// `their_us`: the responder's clock when it answered (for clock offset estimates)
    Pong { sender: PeerId, nonce: u32, sent_us: u64, their_us: u64 },
    Punch { sender: PeerId, token: u32 },
    PunchAck { sender: PeerId, token: u32 },
    /// `aad` = the 12 header bytes, `body` = ciphertext followed by the tag.
    Sealed { sender: PeerId, counter: u64, aad: &'a [u8], body: &'a [u8] },
    Control { sender: PeerId, seq: u32, payload: &'a [u8] },
    ControlAck { sender: PeerId, seq: u32 },
    Bye { sender: PeerId },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("packet too short")]
    TooShort,
    #[error("bad magic byte")]
    BadMagic,
    #[error("unknown packet kind {0}")]
    UnknownKind(u8),
    #[error("invalid audio header")]
    BadAudioHeader,
    #[error("payload length does not match header")]
    LengthMismatch,
    #[error("packet too long")]
    TooLong,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

pub fn decode(buf: &[u8]) -> Result<Packet<'_>, DecodeError> {
    use DecodeError::*;
    if buf.len() < COMMON_HEADER {
        return Err(TooShort);
    }
    if buf[0] != MAGIC {
        return Err(BadMagic);
    }
    let kind = Kind::from_u8(buf[1]).ok_or(UnknownKind(buf[1]))?;
    let sender = u16_at(buf, 2);
    let body = &buf[COMMON_HEADER..];
    let need = |n: usize| if body.len() < n { Err(TooShort) } else { Ok(()) };

    Ok(match kind {
        Kind::Audio => {
            need(8)?;
            let header = AudioHeader {
                seq: u32_at(body, 0),
                frames: u16_at(body, 4),
                channels: body[6],
                format: SampleFormat::from_u8(body[7]).ok_or(BadAudioHeader)?,
            };
            if header.frames == 0
                || header.frames as usize > MAX_FRAMES
                || header.channels == 0
                || header.channels as usize > MAX_CHANNELS
            {
                return Err(BadAudioHeader);
            }
            let payload = &body[8..];
            let ok = payload.len() == header.samples() * header.format.bytes();
            if !ok {
                return Err(LengthMismatch);
            }
            Packet::Audio { sender, header, payload }
        }
        Kind::Ping => {
            need(12)?;
            Packet::Ping { sender, nonce: u32_at(body, 0), sent_us: u64_at(body, 4) }
        }
        Kind::Pong => {
            need(20)?;
            Packet::Pong { sender, nonce: u32_at(body, 0), sent_us: u64_at(body, 4), their_us: u64_at(body, 12) }
        }
        Kind::Control => {
            need(4)?;
            let payload = &body[4..];
            if payload.len() > MAX_CONTROL_PAYLOAD {
                return Err(TooLong);
            }
            Packet::Control { sender, seq: u32_at(body, 0), payload }
        }
        Kind::ControlAck => {
            need(4)?;
            Packet::ControlAck { sender, seq: u32_at(body, 0) }
        }
        Kind::Bye => Packet::Bye { sender },
        Kind::Punch | Kind::PunchAck => {
            need(4)?;
            let token = u32_at(body, 0);
            if kind == Kind::Punch {
                Packet::Punch { sender, token }
            } else {
                Packet::PunchAck { sender, token }
            }
        }
        Kind::Sealed => {
            need(8 + COMMON_HEADER + TAG_LEN)?;
            if buf.len() > MAX_SEALED {
                return Err(TooLong);
            }
            Packet::Sealed { sender, counter: u64_at(body, 0), aad: &buf[..SEALED_HEADER], body: &buf[SEALED_HEADER..] }
        }
    })
}

/// Reads PCM from an audio payload into interleaved f32. Returns samples written.
pub fn decode_samples(header: &AudioHeader, payload: &[u8], out: &mut [f32]) -> usize {
    let n = header.samples().min(out.len());
    match header.format {
        SampleFormat::Pcm16 => {
            for (o, c) in out[..n].iter_mut().zip(payload.as_chunks::<2>().0) {
                *o = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
            }
        }
        SampleFormat::Pcm24 => {
            for (o, c) in out[..n].iter_mut().zip(payload.as_chunks::<3>().0) {
                // place the 24 bits in the top of an i32, then shift back (sign-extends)
                *o = (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0;
            }
        }
    }
    n
}

fn put_common(out: &mut [u8], kind: Kind, sender: PeerId) {
    out[0] = MAGIC;
    out[1] = kind as u8;
    out[2..4].copy_from_slice(&sender.to_le_bytes());
}

/// Triangular noise in (-1, 1) for sample `k` of packet `seq`: two uniform values from a
/// hash, subtracted (no state to keep, so the audio thread carries nothing along).
#[inline]
fn tpdf(seq: u32, k: u32) -> f32 {
    let mut h = seq.wrapping_mul(0x9E37_79B9) ^ k.wrapping_mul(0x85EB_CA6B).rotate_left(13);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^= h >> 16;
    ((h & 0xFFFF) as f32 - (h >> 16) as f32) / 65536.0
}

/// Encodes interleaved samples into `out` (must be at least [`MAX_PACKET`] bytes).
/// Returns the packet length.
pub fn encode_audio(
    out: &mut [u8],
    sender: PeerId,
    seq: u32,
    channels: u8,
    format: SampleFormat,
    samples: &[f32],
) -> usize {
    let ch = channels as usize;
    debug_assert!(ch > 0 && ch <= MAX_CHANNELS && samples.len().is_multiple_of(ch));
    let frames = samples.len() / ch;
    debug_assert!(frames > 0 && frames <= MAX_FRAMES);
    let len = AUDIO_HEADER + samples.len() * format.bytes();

    put_common(out, Kind::Audio, sender);
    out[4..8].copy_from_slice(&seq.to_le_bytes());
    out[8..10].copy_from_slice(&(frames as u16).to_le_bytes());
    out[10] = channels;
    out[11] = format as u8;

    // 16 bits: rounding alone turns a quiet or fading sound into a buzz that follows it
    // (the error repeats with the signal). A little noise added first (±1 step, triangular)
    // makes the error plain steady hiss instead, far below anything played. Not for digital
    // silence (muted: stays exactly silent). 24 bits needs none: its steps are below the
    // noise of any converter.
    let dither = format == SampleFormat::Pcm16 && samples.iter().any(|&s| s != 0.0);
    let mut i = AUDIO_HEADER;
    for (k, &s) in samples.iter().enumerate() {
        let s = s.clamp(-1.0, 1.0);
        match format {
            SampleFormat::Pcm16 => {
                let noise = if dither { tpdf(seq, k as u32) } else { 0.0 };
                out[i..i + 2].copy_from_slice(&((s * 32767.0 + noise).round() as i16).to_le_bytes());
                i += 2;
            }
            SampleFormat::Pcm24 => {
                let b = ((s * 8_388_607.0).round() as i32).to_le_bytes();
                out[i..i + 3].copy_from_slice(&b[..3]);
                i += 3;
            }
        }
    }
    len
}

/// Writes only the audio header; the caller puts `payload_len` bytes after it.
/// Returns the full packet length.
pub fn encode_audio_header(
    out: &mut [u8],
    sender: PeerId,
    seq: u32,
    frames: u16,
    channels: u8,
    format: SampleFormat,
    payload_len: usize,
) -> usize {
    put_common(out, Kind::Audio, sender);
    out[4..8].copy_from_slice(&seq.to_le_bytes());
    out[8..10].copy_from_slice(&frames.to_le_bytes());
    out[10] = channels;
    out[11] = format as u8;
    AUDIO_HEADER + payload_len
}

pub fn encode_ping(out: &mut [u8], sender: PeerId, nonce: u32, sent_us: u64) -> usize {
    put_common(out, Kind::Ping, sender);
    out[4..8].copy_from_slice(&nonce.to_le_bytes());
    out[8..16].copy_from_slice(&sent_us.to_le_bytes());
    16
}

pub fn encode_pong(out: &mut [u8], sender: PeerId, nonce: u32, sent_us: u64, their_us: u64) -> usize {
    put_common(out, Kind::Pong, sender);
    out[4..8].copy_from_slice(&nonce.to_le_bytes());
    out[8..16].copy_from_slice(&sent_us.to_le_bytes());
    out[16..24].copy_from_slice(&their_us.to_le_bytes());
    24
}

/// `out` must hold `CONTROL_HEADER + payload.len()`; payload at most [`MAX_CONTROL_PAYLOAD`].
pub fn encode_control(out: &mut [u8], sender: PeerId, seq: u32, payload: &[u8]) -> usize {
    put_common(out, Kind::Control, sender);
    out[4..8].copy_from_slice(&seq.to_le_bytes());
    out[CONTROL_HEADER..CONTROL_HEADER + payload.len()].copy_from_slice(payload);
    CONTROL_HEADER + payload.len()
}

pub fn encode_control_ack(out: &mut [u8], sender: PeerId, seq: u32) -> usize {
    put_common(out, Kind::ControlAck, sender);
    out[4..8].copy_from_slice(&seq.to_le_bytes());
    8
}

pub fn encode_bye(out: &mut [u8], sender: PeerId) -> usize {
    put_common(out, Kind::Bye, sender);
    COMMON_HEADER
}

pub fn encode_punch(out: &mut [u8], ack: bool, sender: PeerId, token: u32) -> usize {
    put_common(out, if ack { Kind::PunchAck } else { Kind::Punch }, sender);
    out[4..8].copy_from_slice(&token.to_le_bytes());
    8
}

/// Writes the 12-byte sealed header (the caller encrypts the inner packet after it).
pub fn encode_sealed_header(out: &mut [u8], sender: PeerId, counter: u64) -> usize {
    put_common(out, Kind::Sealed, sender);
    out[4..12].copy_from_slice(&counter.to_le_bytes());
    SEALED_HEADER
}

/// Sender id of an already-encoded packet (no validation beyond length/magic).
pub fn peek_sender(buf: &[u8]) -> Option<PeerId> {
    (buf.len() >= COMMON_HEADER && buf[0] == MAGIC).then(|| u16_at(buf, 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(format: SampleFormat, tolerance: f32) {
        let samples: Vec<f32> = (0..64).map(|i| ((i as f32) * 0.1).sin() * 0.9).collect();
        let mut buf = [0u8; MAX_PACKET];
        let n = encode_audio(&mut buf, 3, 42, 2, format, &samples);
        let Packet::Audio { sender, header, payload } = decode(&buf[..n]).unwrap() else {
            panic!("not audio")
        };
        assert_eq!((sender, header.seq, header.frames, header.channels), (3, 42, 32, 2));
        let mut out = [0f32; 64];
        assert_eq!(decode_samples(&header, payload, &mut out), 64);
        for (a, b) in samples.iter().zip(out.iter()) {
            assert!((a - b).abs() < tolerance, "{a} vs {b}");
        }
    }

    #[test]
    fn pcm16_roundtrip() {
        roundtrip(SampleFormat::Pcm16, 1.0 / 10000.0); // rounding, the dither, 32767 against 32768
    }

    /// A tone quieter than one 16-bit step: rounding alone would send silence (or a square
    /// wave); with the noise added first, the tone is in what arrives.
    #[test]
    fn pcm16_keeps_what_is_below_a_step() {
        let amp = 0.4 / 32767.0;
        let (mut sum, mut all_zero) = (0f64, true);
        let mut buf = [0u8; MAX_PACKET];
        let mut out = [0f32; 64];
        for seq in 0..3000u32 {
            let tone: Vec<f32> = (0..64).map(|k| amp * (std::f32::consts::TAU * (seq as usize * 64 + k) as f32 / 48.0).sin()).collect();
            let n = encode_audio(&mut buf, 1, seq, 1, SampleFormat::Pcm16, &tone);
            let Ok(Packet::Audio { header, payload, .. }) = decode(&buf[..n]) else { panic!() };
            decode_samples(&header, payload, &mut out);
            for (o, t) in out.iter().zip(&tone) {
                sum += (*o * *t) as f64;
                all_zero &= *o == 0.0;
            }
        }
        // what arrived correlates with the tone at about its own power
        let power = 3000.0 * 64.0 * (amp as f64).powi(2) / 2.0;
        assert!(!all_zero && sum > 0.7 * power && sum < 1.3 * power, "{sum:e} vs {power:e}");
        // and silence stays silence
        let n = encode_audio(&mut buf, 1, 7, 1, SampleFormat::Pcm16, &[0.0; 64]);
        assert!(buf[AUDIO_HEADER..n].iter().all(|&b| b == 0));
    }

    #[test]
    fn pcm24_roundtrip() {
        roundtrip(SampleFormat::Pcm24, 1.0 / 4_000_000.0);
    }

    #[test]
    fn pcm24_negative_full_scale() {
        let mut buf = [0u8; MAX_PACKET];
        let n = encode_audio(&mut buf, 1, 0, 1, SampleFormat::Pcm24, &[-1.0]);
        let Packet::Audio { header, payload, .. } = decode(&buf[..n]).unwrap() else { panic!() };
        let mut out = [0f32; 1];
        decode_samples(&header, payload, &mut out);
        assert!((out[0] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_malformed_audio() {
        let mut buf = [0u8; MAX_PACKET];
        let n = encode_audio(&mut buf, 1, 0, 2, SampleFormat::Pcm16, &[0.0; 8]);
        assert_eq!(decode(&buf[..n - 1]), Err(DecodeError::LengthMismatch));
        buf[10] = 9; // channels
        assert_eq!(decode(&buf[..n]), Err(DecodeError::BadAudioHeader));
        assert_eq!(decode(&[MAGIC, 1]), Err(DecodeError::TooShort));
        assert_eq!(decode(&[0, 1, 0, 0]), Err(DecodeError::BadMagic));
        assert_eq!(decode(&[MAGIC, 99, 0, 0]), Err(DecodeError::UnknownKind(99)));
    }

    #[test]
    fn sealed_layout() {
        let mut buf = [0u8; 64];
        encode_sealed_header(&mut buf, 3, 0x0102_0304_0506_0708);
        let n = SEALED_HEADER + 20 + TAG_LEN;
        let Packet::Sealed { sender, counter, aad, body } = decode(&buf[..n]).unwrap() else { panic!() };
        assert_eq!((sender, counter, aad.len(), body.len()), (3, 0x0102_0304_0506_0708, 12, 36));
        assert_eq!(decode(&buf[..SEALED_HEADER + TAG_LEN]), Err(DecodeError::TooShort));
    }

    #[test]
    fn control_roundtrip() {
        let mut buf = [0u8; 32];
        let n = encode_pong(&mut buf, 5, 9, 123_456, 777);
        assert_eq!(decode(&buf[..n]), Ok(Packet::Pong { sender: 5, nonce: 9, sent_us: 123_456, their_us: 777 }));
        let n = encode_ping(&mut buf, 5, 9, 1);
        assert_eq!(decode(&buf[..n]), Ok(Packet::Ping { sender: 5, nonce: 9, sent_us: 1 }));
        let n = encode_bye(&mut buf, 5);
        assert_eq!(decode(&buf[..n]), Ok(Packet::Bye { sender: 5 }));
        let n = encode_control_ack(&mut buf, 5, 41);
        assert_eq!(decode(&buf[..n]), Ok(Packet::ControlAck { sender: 5, seq: 41 }));
        let mut big = [0u8; MAX_INNER];
        let n = encode_control(&mut big, 5, 40, b"{\"t\":\"chat\"}");
        assert_eq!(decode(&big[..n]), Ok(Packet::Control { sender: 5, seq: 40, payload: b"{\"t\":\"chat\"}" }));
        let n = encode_control(&mut big, 5, 40, &[b'x'; MAX_CONTROL_PAYLOAD]);
        assert!(decode(&big[..n]).is_ok());
        assert_eq!(n, MAX_INNER.min(CONTROL_HEADER + MAX_CONTROL_PAYLOAD));
        let n = encode_punch(&mut buf, false, 5, 77);
        assert_eq!(decode(&buf[..n]), Ok(Packet::Punch { sender: 5, token: 77 }));
    }
}
