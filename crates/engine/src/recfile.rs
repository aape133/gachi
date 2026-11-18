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
    gachi_lang::tr!("rec.cant_write_recording", e)
}

/// One file being written. Samples come interleaved, `channels` (1 or 2) to a frame.
pub(crate) enum Sink {
    Wav(Box<WavSink>),
    Flac(Box<FlacSink>),
}

impl Sink {
    pub fn create(path: &Path, format: RecFormat, channels: usize, sample_rate: u32) -> Result<Self, String> {
        let made = |e: String| gachi_lang::tr!("rec.cant_create_file", e);
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
        let done = |e: String| gachi_lang::tr!("rec.cant_finish_recording", e);
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
    channels: u32,
    /// samples since the header last got the length, and how many go between two updates
    since_header: usize,
    header_every: usize,
}

/// Seconds of sound between two updates of a WAV file's length in its header: if the app
/// or the PC goes down mid-recording, the file reads up to there (else its header says
/// it is empty, and players open it as such).
const WAV_HEADER_EVERY_S: usize = 10;

impl WavSink {
    fn create(path: &Path, bits: u8, channels: usize, sample_rate: u32) -> Result<Self, String> {
        let spec = hound::WavSpec {
            channels: channels as u16,
            sample_rate,
            bits_per_sample: bits as u16,
            sample_format: if bits == 32 { hound::SampleFormat::Float } else { hound::SampleFormat::Int },
        };
        Ok(Self {
            w: hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?,
            bits,
            dither: Dither::new(),
            channels: channels as u32,
            since_header: 0,
            header_every: sample_rate as usize * channels * WAV_HEADER_EVERY_S,
        })
    }

    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let cannot = |e: hound::Error| gachi_lang::tr!("rec.cant_write_recording", e);
        match self.bits {
            32 => samples.iter().try_for_each(|&s| self.w.write_sample(s)).map_err(cannot)?,
            24 => samples.iter().try_for_each(|&s| self.w.write_sample(to_int(s, 24, 0.0))).map_err(cannot)?,
            _ => {
                for &s in samples {
                    let d = self.dither.of(s);
                    self.w.write_sample(to_int(s, 16, d) as i16).map_err(cannot)?;
                }
            }
        }
        self.since_header += samples.len();
        // (between whole frames only: a header in the middle of one would be refused)
        if self.since_header >= self.header_every && self.w.len().is_multiple_of(self.channels) {
            self.since_header = 0;
            self.w.flush().map_err(cannot)?;
        }
        Ok(())
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
struct Dither(u32);

impl Dither {
    /// Each file its own: tracks with the same hiss would add it up in step when mixed
    /// (16 tracks: 24 dB more of it instead of 12).
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        Dither(n.wrapping_mul(0x9E37_79B9) ^ t)
    }

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
            dither: Dither::new(),
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
        let bad = |e: &dyn std::fmt::Display| gachi_lang::tr!("rec.cant_convert_flac", e);
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

    #[test]
    fn a_wav_cut_off_mid_recording_still_reads() {
        // 11 s of stereo at a low rate (quick), then the app "goes down": the writer is
        // never finished (no header written at the end, nothing more flushed)
        let path = temp("cut.wav");
        let rate = 1000;
        let mut s = Sink::create(&path, RecFormat::Wav { bits: 16 }, 2, rate).unwrap();
        for _ in 0..11 {
            s.write(&sine(rate as usize, rate, 50.0, 2)).unwrap();
        }
        std::mem::forget(s);
        let r = hound::WavReader::open(&path).unwrap();
        let secs = r.duration() / rate;
        assert!(secs >= WAV_HEADER_EVERY_S as u32, "reads as {secs} s: the header kept up within {WAV_HEADER_EVERY_S} s");
        drop(r);
        std::fs::remove_file(path).ok();
    }

    /// What a recording does to the sound, measured on a tone read back from each format:
    /// level kept, error no more than the format's own rounding (16 bit: the dither's
    /// even hiss, ~ 91 dB below full scale; 24 bit and float: far below).
    #[test]
    fn recordings_keep_the_sound() {
        let (rate, hz, frames) = (48_000u32, 997.0f64, 48_000usize);
        let w = std::f64::consts::TAU * hz / rate as f64;
        for level in [-6.0f64, -40.0] {
            let amp = 10f64.powf(level / 20.0);
            let sound: Vec<f32> = (0..frames).map(|k| (amp * (w * k as f64).sin()) as f32).collect();
            for format in [RecFormat::Wav { bits: 16 }, RecFormat::Wav { bits: 24 }, RecFormat::Wav { bits: 32 }, RecFormat::Flac { bits: 16 }, RecFormat::Flac { bits: 24 }] {
                let path = temp(&format!("q{level}.{}", format.ext()));
                let mut s = Sink::create(&path, format, 1, rate).unwrap();
                s.write(&sound).unwrap();
                s.finish().unwrap();
                let back: Vec<f64> = match format {
                    RecFormat::Wav { bits: 32 } => hound::WavReader::open(&path).unwrap().samples::<f32>().map(|s| s.unwrap() as f64).collect(),
                    RecFormat::Wav { bits } => {
                        let max = ((1i64 << (bits - 1)) - 1) as f64;
                        hound::WavReader::open(&path).unwrap().samples::<i32>().map(|s| s.unwrap() as f64 / max).collect()
                    }
                    RecFormat::Flac { bits } => {
                        let max = ((1i64 << (bits - 1)) - 1) as f64;
                        claxon::FlacReader::open(&path).unwrap().samples().map(|s| s.unwrap() as f64 / max).collect()
                    }
                };
                std::fs::remove_file(&path).ok();
                assert_eq!(back.len(), frames);
                let (a, snr) = fit(&back, w);
                let gain = 20.0 * (a / amp).log10();
                println!("{format:?} at {level} dBFS: level {gain:+.4} dB, signal/error {snr:.1} dB");
                assert!(gain.abs() < 0.01, "{format:?}: level moved {gain:+.4} dB");
                let bits = match format {
                    RecFormat::Wav { bits } | RecFormat::Flac { bits } => bits,
                };
                let wanted = match bits {
                    16 => 85.0 + level,
                    24 => 120.0 + level,
                    _ => 130.0,
                };
                assert!(snr > wanted, "{format:?} at {level} dBFS: signal/error {snr:.1} dB (wanted > {wanted})");
            }
        }
    }

    /// Two files written at once do not get the same hiss.
    #[test]
    fn each_file_gets_its_own_dither() {
        let (mut a, mut b) = (Dither::new(), Dither::new());
        let same = (0..1000).filter(|_| a.next() == b.next()).count();
        assert!(same < 10, "{same} of 1000 the same");
    }

    /// Least-squares fit of `x` to a sine at `w` rad/sample: (amplitude, signal/error dB).
    fn fit(x: &[f64], w: f64) -> (f64, f64) {
        let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (k, &v) in x.iter().enumerate() {
            let (s, c) = (w * k as f64).sin_cos();
            ss += s * s;
            cc += c * c;
            sc += s * c;
            xs += v * s;
            xc += v * c;
        }
        let det = ss * cc - sc * sc;
        let (a, b) = ((xs * cc - xc * sc) / det, (xc * ss - xs * sc) / det);
        let (mut sig, mut err) = (0.0, 0.0);
        for (k, &v) in x.iter().enumerate() {
            let (s, c) = (w * k as f64).sin_cos();
            let m = a * s + b * c;
            sig += m * m;
            err += (v - m) * (v - m);
        }
        ((a * a + b * b).sqrt(), 10.0 * (sig / err.max(1e-300)).log10())
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
