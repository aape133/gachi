//! The files a recording is written to: WAV or FLAC.
//!
//! All of this runs on the recording's writer thread, never on an audio thread.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// What a recording's files are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecFormat {
    /// uncompressed: 16 or 24 bit integers, or 32 bit float
    Wav { bits: u8 },
    /// lossless, around half the size of the same WAV: 16 or 24 bit
    Flac { bits: u8 },
}

impl Default for RecFormat {
    fn default() -> Self {
        Self::Wav { bits: 16 }
    }
}

impl RecFormat {
    /// From the settings' words, with values a format does not have put right.
    pub fn from_settings(format: &str, bits: u32) -> Self {
        match format {
            "flac" => Self::Flac { bits: if bits >= 24 { 24 } else { 16 } },
            _ => Self::Wav { bits: if bits >= 32 { 32 } else if bits >= 24 { 24 } else { 16 } },
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Wav { .. } => "wav",
            Self::Flac { .. } => "flac",
        }
    }

    /// Frames one file holds before a track goes on in the next. A WAV file holds 4 GB,
    /// and past that its length field wraps around (the file then reads as a few minutes):
    /// 3.84 GB of stereo at this many bits. Every track changes files at the same places,
    /// so the parts line up. FLAC has no such limit.
    pub fn part_frames(self) -> u64 {
        match self {
            Self::Wav { bits } => 3_840_000_000 / (2 * (bits as u64 / 8).max(1)),
            Self::Flac { .. } => 1 << 40,
        }
    }
}

fn io(e: std::io::Error) -> String {
    format!("녹음 파일에 쓸 수 없어요 ({e})")
}

/// One file being written. Samples come interleaved, `channels` (1 or 2) to a frame.
pub(crate) enum Sink {
    Wav(Box<WavSink>),
    Flac(Box<FlacSink>),
}

impl Sink {
    pub fn create(path: &Path, format: RecFormat, channels: usize, sample_rate: u32) -> Result<Self, String> {
        let made = |e: String| format!("파일을 만들 수 없어요 ({e})");
        Ok(match format {
            RecFormat::Wav { bits } => Self::Wav(Box::new(WavSink::create(path, bits, channels, sample_rate).map_err(made)?)),
            RecFormat::Flac { bits } => Self::Flac(Box::new(FlacSink::create(path, bits, channels, sample_rate).map_err(made)?)),
        })
    }

    pub fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        match self {
            Self::Wav(s) => s.write(samples),
            Self::Flac(s) => s.write(samples),
        }
    }

    /// `frames` frames of silence.
    pub fn silence(&mut self, frames: u64, channels: usize) -> Result<(), String> {
        const ZEROS: [f32; 4096] = [0.0; 4096];
        let mut left = frames * channels as u64;
        while left > 0 {
            let n = left.min(ZEROS.len() as u64) as usize;
            self.write(&ZEROS[..n])?;
            left -= n as u64;
        }
        Ok(())
    }

    /// Finishes the file (headers get their lengths, what is buffered goes out).
    pub fn finish(self) -> Result<(), String> {
        let done = |e: String| format!("녹음 파일을 마무리할 수 없어요 ({e})");
        match self {
            Self::Wav(s) => s.finish().map_err(done),
            Self::Flac(s) => s.finish().map_err(done),
        }
    }
}

/* ───────────────────────── WAV ───────────────────────── */

pub(crate) struct WavSink {
    w: hound::WavWriter<BufWriter<File>>,
    bits: u8,
    dither: Dither,
}

impl WavSink {
    fn create(path: &Path, bits: u8, channels: usize, sample_rate: u32) -> Result<Self, String> {
        let spec = hound::WavSpec {
            channels: channels as u16,
            sample_rate,
            bits_per_sample: bits as u16,
            sample_format: if bits == 32 { hound::SampleFormat::Float } else { hound::SampleFormat::Int },
        };
        Ok(Self { w: hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?, bits, dither: Dither::default() })
    }

    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let cannot = |e: hound::Error| format!("녹음 파일에 쓸 수 없어요 ({e})");
        match self.bits {
            32 => samples.iter().try_for_each(|&s| self.w.write_sample(s)).map_err(cannot),
            24 => samples.iter().try_for_each(|&s| self.w.write_sample(to_int(s, 24, 0.0))).map_err(cannot),
            _ => {
                for &s in samples {
                    let d = self.dither.of(s);
                    self.w.write_sample(to_int(s, 16, d) as i16).map_err(cannot)?;
                }
                Ok(())
            }
        }
    }

    fn finish(self) -> Result<(), String> {
        self.w.finalize().map_err(|e| e.to_string())
    }
}

/// A sample as an integer of `bits` bits (`dither`: in steps, added before rounding).
#[inline]
fn to_int(s: f32, bits: u32, dither: f32) -> i32 {
    let max = ((1i64 << (bits - 1)) - 1) as f32;
    let v = if s.is_finite() { s } else { 0.0 };
    (v * max + dither).round().clamp(-max, max) as i32
}

/// Triangular dither of one step for 16 bit: what is below a step becomes a faint even
/// hiss instead of distortion that follows the sound. (24 bit needs none: its steps are
/// below any converter's noise.)
#[derive(Default)]
struct Dither(u32);

impl Dither {
    /// The dither for the sample `s`: none for digital silence (the stretches a recording
    /// fills in stay exactly silent).
    #[inline]
    fn of(&mut self, s: f32) -> f32 {
        if s == 0.0 { 0.0 } else { self.next() }
    }

    #[inline]
    fn next(&mut self) -> f32 {
        let mut r = || {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.0 >> 8) as f32 / (1u32 << 24) as f32
        };
        r() - r()
    }
}

/* ───────────────────────── FLAC ───────────────────────── */

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream};
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};

/// Frames to a FLAC block (the format's usual size).
const FLAC_BLOCK: usize = 4096;

pub(crate) struct FlacSink {
    out: BufWriter<File>,
    cfg: Verified<flacenc::config::Encoder>,
    /// the header as it stands (written first as a placeholder, again at the end)
    stream: Stream,
    buf: FrameBuf,
    ctx: Context,
    /// samples waiting for a full block
    pending: Vec<i32>,
    channels: usize,
    bits: u32,
    frame_no: usize,
    dither: Dither,
}

impl FlacSink {
    fn create(path: &Path, bits: u8, channels: usize, sample_rate: u32) -> Result<Self, String> {
        let bits = bits as usize;
        let mut stream = Stream::new(sample_rate as usize, channels, bits).map_err(|e| e.to_string())?;
        stream.stream_info_mut().set_block_sizes(FLAC_BLOCK, FLAC_BLOCK).map_err(|e| e.to_string())?;
        let cfg = flacenc::config::Encoder::default().into_verified().map_err(|(_, e)| e.to_string())?;
        let mut out = BufWriter::new(File::create(path).map_err(|e| e.to_string())?);
        out.write_all(&header_bytes(&stream)?).map_err(|e| e.to_string())?;
        Ok(Self {
            out,
            cfg,
            stream,
            buf: FrameBuf::with_size(channels, FLAC_BLOCK).map_err(|e| e.to_string())?,
            ctx: Context::new(bits, channels),
            pending: Vec::with_capacity(FLAC_BLOCK * channels * 2),
            channels,
            bits: bits as u32,
            frame_no: 0,
            dither: Dither::default(),
        })
    }

    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        for &s in samples {
            let d = if self.bits == 16 { self.dither.of(s) } else { 0.0 };
            self.pending.push(to_int(s, self.bits, d));
        }
        let block = FLAC_BLOCK * self.channels;
        let mut at = 0;
        while self.pending.len() - at >= block {
            self.encode(at, block)?;
            at += block;
        }
        self.pending.drain(..at);
        Ok(())
    }

    /// Encodes `pending[at..at + len]` as one block and writes it.
    fn encode(&mut self, at: usize, len: usize) -> Result<(), String> {
        let bad = |e: &dyn std::fmt::Display| format!("FLAC으로 바꿀 수 없어요 ({e})");
        (&mut self.buf, &mut self.ctx).fill_interleaved(&self.pending[at..at + len]).map_err(|e| bad(&e))?;
        let frame = flacenc::encode_fixed_size_frame(&self.cfg, &self.buf, self.frame_no, self.stream.stream_info()).map_err(|e| bad(&e))?;
        self.frame_no += 1;
        let mut bytes = ByteSink::new();
        frame.write(&mut bytes).map_err(|e| bad(&e))?;
        self.stream.stream_info_mut().update_frame_info(&frame);
        self.out.write_all(bytes.as_slice()).map_err(io)
    }

    fn finish(mut self) -> Result<(), String> {
        // the last block is as long as what is left
        if !self.pending.is_empty() {
            let left = self.pending.len();
            self.buf = FrameBuf::with_size(self.channels, left / self.channels).map_err(|e| e.to_string())?;
            self.encode(0, left)?;
        }
        let info = self.stream.stream_info_mut();
        // Every block is `FLAC_BLOCK` frames but the last, which does not count: the two
        // block sizes in the header must stay the same. Counting each frame had put the last
        // one's size in as the smallest, and a header whose two sizes differ says "blocks of
        // any size", in which a frame's number is read as a count of samples, not of blocks:
        // players that jump by those numbers landed in the wrong place (no sound after a
        // while in a DAW, no jumping in some players; playing from the start was fine)
        info.set_block_sizes(FLAC_BLOCK, FLAC_BLOCK).map_err(|e| e.to_string())?;
        info.set_md5_digest(&self.ctx.md5_digest());
        info.set_total_samples(self.ctx.total_samples());
        // the header again, now with the length and the checksum
        let header = header_bytes(&self.stream)?;
        self.out.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        self.out.write_all(&header).map_err(|e| e.to_string())?;
        self.out.flush().map_err(|e| e.to_string())
    }
}

/// "fLaC" and the stream's description (no audio: the frames are written one by one).
fn header_bytes(stream: &Stream) -> Result<Vec<u8>, String> {
    let mut bytes = ByteSink::new();
    stream.write(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.as_slice().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, rate: u32, hz: f32, channels: usize) -> Vec<f32> {
        (0..frames).flat_map(|i| std::iter::repeat_n(0.5 * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin(), channels)).collect()
    }

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("gachi-recfile-{}-{name}", std::process::id()))
    }

    #[test]
    fn the_settings_words_give_a_format_that_exists() {
        assert_eq!(RecFormat::from_settings("wav", 16), RecFormat::Wav { bits: 16 });
        assert_eq!(RecFormat::from_settings("wav", 32), RecFormat::Wav { bits: 32 });
        assert_eq!(RecFormat::from_settings("flac", 32), RecFormat::Flac { bits: 24 }, "FLAC has no float");
        // (a format that is no longer there, like an "opus" saved by a build that had it)
        assert_eq!(RecFormat::from_settings("opus", 16), RecFormat::default());
        // 3.84 GB of stereo: 32 bit float as before, more frames with fewer bits
        assert_eq!(RecFormat::Wav { bits: 32 }.part_frames(), 480_000_000);
        assert_eq!(RecFormat::Wav { bits: 16 }.part_frames(), 960_000_000);
    }

    #[test]
    fn wav_in_three_depths_reads_back() {
        for (bits, tol) in [(16u8, 2.0 / 32768.0), (24, 1e-6), (32, 0.0)] {
            let path = temp(&format!("{bits}.wav"));
            let sound = sine(1000, 48_000, 440.0, 1);
            let mut s = Sink::create(&path, RecFormat::Wav { bits }, 1, 48_000).unwrap();
            s.write(&sound).unwrap();
            s.silence(24, 1).unwrap();
            s.finish().unwrap();
            let mut r = hound::WavReader::open(&path).unwrap();
            assert_eq!((r.spec().channels, r.spec().bits_per_sample, r.duration()), (1, bits as u16, 1024));
            let back: Vec<f32> = if bits == 32 {
                r.samples::<f32>().map(|s| s.unwrap()).collect()
            } else {
                let max = ((1i64 << (bits - 1)) - 1) as f32;
                r.samples::<i32>().map(|s| s.unwrap() as f32 / max).collect()
            };
            let worst = sound.iter().zip(&back).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
            assert!(worst <= tol, "{bits} bit: off by {worst}");
            std::fs::remove_file(path).ok();
        }
    }

    /// FLAC is lossless: what a decoder gives back is the 24 bit samples we wrote, block
    /// by block and with a last block that is not full.
    #[test]
    fn flac_reads_back_exactly() {
        let path = temp("a.flac");
        let frames = FLAC_BLOCK * 2 + 777;
        let sound = sine(frames, 48_000, 440.0, 2);
        let mut s = Sink::create(&path, RecFormat::Flac { bits: 24 }, 2, 48_000).unwrap();
        // in uneven pieces, as the recording hands them over
        for piece in sound.chunks(2 * 250) {
            s.write(piece).unwrap();
        }
        s.finish().unwrap();
        let mut r = claxon::FlacReader::open(&path).expect("a FLAC file a decoder accepts");
        let info = r.streaminfo();
        assert_eq!((info.channels, info.bits_per_sample, info.sample_rate, info.samples), (2, 24, 48_000, Some(frames as u64)));
        // blocks of one size (the last, shorter one does not count): what lets a player
        // jump to a time by the frames' numbers
        assert_eq!((info.min_block_size, info.max_block_size), (FLAC_BLOCK as u16, FLAC_BLOCK as u16));
        let back: Vec<i32> = r.samples().map(|s| s.expect("every block decodes")).collect();
        let wrote: Vec<i32> = sound.iter().map(|&s| to_int(s, 24, 0.0)).collect();
        assert_eq!(back.len(), wrote.len());
        assert!(back == wrote, "not the samples that were written");
        // and it is smaller than the same sound as WAV
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size < (frames * 2 * 3) as u64 * 8 / 10, "{size} bytes for {frames} frames");
        std::fs::remove_file(path).ok();
    }
}
