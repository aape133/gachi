//! Things the output callback adds besides the mix: test tone, metronome click, and the
//! per-track recording tap. All allocation-free on the audio thread.

use crate::params::{AtomicF32, AtomicI64};
use gachi_proto::PeerId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/* ───────────────────────── test tone ───────────────────────── */

/// 1 kHz sine at -18 dBFS with short fades, so it never clicks.
pub(crate) struct ToneGen {
    phase: f32,
    step: f32,
    fade: u32,
    total: u32,
}

impl ToneGen {
    pub fn new(sample_rate: u32) -> Self {
        Self { phase: 0.0, step: std::f32::consts::TAU * 1000.0 / sample_rate as f32, fade: sample_rate / 200, total: 0 }
    }

    /// `left` = frames still to play (the UI thread starts a tone by storing into it).
    pub fn add(&mut self, left: &AtomicU32, mix: &mut [f32]) {
        let n = left.load(Relaxed);
        if n == 0 {
            self.total = 0;
            return;
        }
        if n > self.total {
            self.total = n; // a new tone started
        }
        let take = ((mix.len() / 2) as u32).min(n);
        for (k, m) in mix.as_chunks_mut::<2>().0.iter_mut().take(take as usize).enumerate() {
            let remaining = n - k as u32;
            let played = self.total - remaining;
            let env = (played.min(remaining) as f32 / self.fade as f32).min(1.0);
            let s = self.phase.sin() * 0.125 * env;
            self.phase = (self.phase + self.step) % std::f32::consts::TAU;
            m[0] += s;
            m[1] += s;
        }
        // if the UI restarted the tone meanwhile, keep its value
        let _ = left.compare_exchange(n, n - take, Relaxed, Relaxed);
    }
}

/* ───────────────────────── metronome ───────────────────────── */

/// Shared with the app. Everyone in the room gets the same beat-0 time in server time,
/// converted to their own clock, so all clicks line up in real time.
pub struct MetronomeParams {
    pub on: AtomicBool,
    pub bpm: AtomicF32,
    /// beat 0, in µs on the engine's monotonic clock (see `Engine::set_metronome`)
    pub start_us: AtomicI64,
    pub beats_per_bar: AtomicU32,
    pub gain: AtomicF32,
}

impl Default for MetronomeParams {
    fn default() -> Self {
        Self {
            on: AtomicBool::new(false),
            bpm: AtomicF32::new(120.0),
            start_us: AtomicI64::new(0),
            beats_per_bar: AtomicU32::new(4),
            gain: AtomicF32::new(0.5),
        }
    }
}

pub(crate) struct Metronome {
    sr: f64,
    /// µs from writing a sample until it leaves the speaker (output buffer)
    out_latency_us: f64,
    origin: Instant,
    /// (sample count, µs) where the sample clock was last lined up with the system clock
    anchor: Option<(u64, f64)>,
    samples: u64,
    last_beat: Option<i64>,
    click_left: u32,
    click_len: u32,
    click_phase: f32,
    click_step: f32,
}

impl Metronome {
    pub fn new(sr: u32, out_latency_us: f64, origin: Instant) -> Self {
        Self {
            sr: sr as f64,
            out_latency_us,
            origin,
            anchor: None,
            samples: 0,
            last_beat: None,
            click_left: 0,
            click_len: sr / 40, // 25 ms
            click_phase: 0.0,
            click_step: 0.0,
        }
    }

    pub fn add(&mut self, p: &MetronomeParams, mix: &mut [f32]) {
        let now = self.origin.elapsed().as_secs_f64() * 1e6;
        self.add_at(p, mix, now);
    }

    /// `now` = µs since `origin` when this block was requested.
    fn add_at(&mut self, p: &MetronomeParams, mix: &mut [f32], now: f64) {
        let frames = mix.len() / 2;
        // Count samples (smooth, no per-callback timing jitter), but re-anchor to the system
        // clock when they disagree by more than 2 ms (sound card drift, a stall, a restart).
        let expected = self.anchor.map(|(s, t)| t + (self.samples - s) as f64 * 1e6 / self.sr);
        let block_us = match expected {
            Some(e) if (e - now).abs() < 2000.0 => e,
            _ => {
                self.anchor = Some((self.samples, now));
                now
            }
        };
        self.samples += frames as u64;

        let on = p.on.load(Relaxed);
        if !on {
            self.last_beat = None;
        }
        let period = 60e6 / p.bpm.load().clamp(20.0, 400.0) as f64;
        let t0 = block_us + self.out_latency_us - p.start_us.load() as f64;
        let per_bar = p.beats_per_bar.load(Relaxed).max(1) as i64;
        let gain = p.gain.load();

        for (k, m) in mix.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            if on {
                let t = t0 + k as f64 * 1e6 / self.sr;
                if t >= 0.0 {
                    let beat = (t / period).floor() as i64;
                    if self.last_beat != Some(beat) {
                        // when switched on mid-beat, wait for the next beat instead of clicking late
                        let fresh = t - beat as f64 * period < 5_000.0;
                        if self.last_beat.is_some() || fresh {
                            let freq = if beat % per_bar == 0 { 1760.0 } else { 1320.0 }; // accent the downbeat
                            self.click_step = std::f32::consts::TAU * freq / self.sr as f32;
                            self.click_phase = 0.0;
                            self.click_left = self.click_len;
                        }
                        self.last_beat = Some(beat);
                    }
                }
            }
            if self.click_left > 0 {
                let env = self.click_left as f32 / self.click_len as f32;
                let s = self.click_phase.sin() * env * env * gain;
                self.click_phase += self.click_step;
                self.click_left -= 1;
                m[0] += s;
                m[1] += s;
            }
        }
    }
}

/* ───────────────────────── recording ───────────────────────── */

/// Track id used for your own input.
pub const SELF_TRACK: PeerId = 0;
const BLOCK: usize = 256;
/// ~2.5 s of 17 tracks (a full room) at 48 kHz between the audio thread and the writer: a
/// disk that stops for a moment (an antivirus scan, a drive waking up) loses nothing
const RING_BLOCKS: usize = 8192;
/// A WAV file holds 4 GB at most, and past that its length field wraps around: the file
/// then reads as a few minutes, the rest of the recording unreachable. So a track goes on
/// in a new file every this many frames (32-bit float stereo is 8 bytes a frame: 3.6 GB;
/// 2 h 46 min at 48 kHz, 41 min at 192 kHz). Every track changes files at the same places
/// on the timeline, so the parts line up like the first files do.
const PART_FRAMES: u64 = 480_000_000;

#[derive(Clone, Copy)]
pub(crate) struct RecBlock {
    track: PeerId,
    /// position of the first frame on the recording timeline
    pos: u64,
    frames: u16,
    data: [f32; BLOCK * 2],
}

impl RecBlock {
    const EMPTY: RecBlock = RecBlock { track: 0, pos: 0, frames: 0, data: [0.0; BLOCK * 2] };
}

/// Lives in the output callback while recording. One accumulator per track slot, so blocks
/// go out full (fewer, bigger ring pushes).
pub(crate) struct RecTap {
    tx: rtrb::Producer<RecBlock>,
    acc: Box<[RecBlock]>,
    pub pos: u64,
    dropped: Arc<AtomicU64>,
}

impl RecTap {
    /// `slot` is a stable index per track (0 = self, 1.. = peer slots).
    pub fn write(&mut self, slot: usize, track: PeerId, stereo: &[f32]) {
        let Some(mut a) = self.acc.get(slot).copied() else { return };
        let mut pos = self.pos;
        let mut rest = stereo;
        // a different track took this slot, or there was a gap: flush what we had
        if a.frames > 0 && (a.track != track || a.pos + a.frames as u64 != pos) {
            self.flush(&mut a);
        }
        while !rest.is_empty() {
            if a.frames == 0 {
                a.track = track;
                a.pos = pos;
            }
            let room = BLOCK - a.frames as usize;
            let n = room.min(rest.len() / 2);
            let at = a.frames as usize * 2;
            a.data[at..at + n * 2].copy_from_slice(&rest[..n * 2]);
            a.frames += n as u16;
            pos += n as u64;
            rest = &rest[n * 2..];
            if a.frames as usize == BLOCK {
                self.flush(&mut a);
            }
        }
        self.acc[slot] = a;
    }

    fn flush(&mut self, a: &mut RecBlock) {
        if self.tx.push(*a).is_err() {
            self.dropped.fetch_add(1, Relaxed);
        }
        a.frames = 0;
    }

    /// Pushes partial blocks (called once when recording stops).
    pub fn flush_all(&mut self) {
        for i in 0..self.acc.len() {
            let mut a = self.acc[i];
            if a.frames > 0 {
                self.flush(&mut a);
                self.acc[i] = a;
            }
        }
    }
}

/// What a finished recording produced.
#[derive(Debug, Clone)]
pub struct RecordingResult {
    pub dir: PathBuf,
    pub files: Vec<PathBuf>,
    pub seconds: f32,
    /// blocks lost because the disk could not keep up (should be 0)
    pub dropped_blocks: u64,
}

/// The writer side: a thread that drains the ring into one WAV file per track.
pub(crate) struct Recorder {
    thread: thread::JoinHandle<Result<RecordingResult, String>>,
}

impl Recorder {
    /// Creates the tap (goes to the audio thread) and starts the writer.
    /// `names` maps track ids to display names; it is read when a track first appears.
    /// Files are named `<prefix><number> <name>.wav`.
    pub fn start(
        dir: PathBuf,
        prefix: String,
        sample_rate: u32,
        names: Arc<Mutex<HashMap<PeerId, String>>>,
        slots: usize,
    ) -> Result<(RecTap, Recorder), String> {
        Self::start_parts(dir, prefix, sample_rate, names, slots, PART_FRAMES)
    }

    /// (`part_frames`: see [`PART_FRAMES`]; tests use small parts)
    fn start_parts(
        dir: PathBuf,
        prefix: String,
        sample_rate: u32,
        names: Arc<Mutex<HashMap<PeerId, String>>>,
        slots: usize,
        part_frames: u64,
    ) -> Result<(RecTap, Recorder), String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("녹음 폴더를 만들 수 없어요 ({e})"))?;
        let (tx, rx) = rtrb::RingBuffer::new(RING_BLOCKS);
        let dropped = Arc::new(AtomicU64::new(0));
        let tap = RecTap { tx, acc: vec![RecBlock::EMPTY; slots].into_boxed_slice(), pos: 0, dropped: dropped.clone() };
        let thread = thread::Builder::new()
            .name("gachi-rec-writer".into())
            .spawn(move || write_loop(rx, dir, prefix, sample_rate, names, dropped, part_frames))
            .map_err(|e| e.to_string())?;
        Ok((tap, Recorder { thread }))
    }

    /// Waits for the writer to finish (the tap must already be dropped by the audio thread).
    pub fn finish(self) -> Result<RecordingResult, String> {
        self.thread.join().map_err(|_| "녹음 저장 중 오류가 났어요".to_string())?
    }
}

/// `s` as (part of) a file name: without the characters Windows does not take in one.
pub fn safe_name(s: &str) -> String {
    let s: String = s.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let s = s.trim().trim_end_matches('.').to_string();
    if s.is_empty() { "track".into() } else { s }
}

type Wav = hound::WavWriter<std::io::BufWriter<std::fs::File>>;

/// One track of a recording: its files (one per part of the timeline, see [`PART_FRAMES`]).
struct Track {
    /// the file being written (opened with the first frame that goes into it)
    writer: Option<Wav>,
    /// which part of the timeline that file is
    part: u64,
    /// frames of the timeline this track is written up to
    written: u64,
    /// what its file names start with, then its number and name
    prefix: std::sync::Arc<str>,
    n: usize,
    name: String,
    files: Vec<PathBuf>,
}

impl Track {
    /// A track whose first sound is at `pos`: it starts with the part `pos` is in (a track
    /// that joins hours in has no files of silence for the hours before).
    fn new(prefix: std::sync::Arc<str>, n: usize, name: String, pos: u64, part_frames: u64) -> Self {
        Self { writer: None, part: pos / part_frames, written: pos - pos % part_frames, prefix, n, name, files: Vec::new() }
    }

    /// Writes `frames` frames at the track's end, into the part (or parts) they fall in:
    /// `data` (interleaved stereo), or silence.
    fn put(&mut self, dir: &Path, sample_rate: u32, part_frames: u64, data: Option<&[f32]>, frames: u64) -> Result<(), String> {
        let cannot = |e: hound::Error| format!("녹음 파일에 쓸 수 없어요 ({e})");
        let mut done = 0;
        while done < frames {
            let part = self.written / part_frames;
            if self.writer.is_none() || part != self.part {
                self.close()?;
                let name = safe_name(&self.name);
                let (prefix, n) = (&self.prefix, self.n);
                let file = if part == 0 { format!("{prefix}{n:02} {name}.wav") } else { format!("{prefix}{n:02} {name} - {}.wav", part + 1) };
                let path = dir.join(file);
                let spec = hound::WavSpec { channels: 2, sample_rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
                self.writer = Some(hound::WavWriter::create(&path, spec).map_err(|e| format!("파일을 만들 수 없어요 ({e})"))?);
                self.part = part;
                self.files.push(path);
            }
            let Some(w) = self.writer.as_mut() else { break };
            let n = (frames - done).min((part + 1) * part_frames - self.written);
            match data {
                Some(d) => {
                    for &s in &d[done as usize * 2..(done + n) as usize * 2] {
                        w.write_sample(s).map_err(cannot)?;
                    }
                }
                None => {
                    for _ in 0..n * 2 {
                        w.write_sample(0.0f32).map_err(cannot)?;
                    }
                }
            }
            self.written += n;
            done += n;
        }
        Ok(())
    }

    /// Finishes the file being written (its header gets the length).
    fn close(&mut self) -> Result<(), String> {
        match self.writer.take() {
            Some(w) => w.finalize().map_err(|e| format!("녹음 파일을 마무리할 수 없어요 ({e})")),
            None => Ok(()),
        }
    }
}

fn write_loop(
    mut rx: rtrb::Consumer<RecBlock>,
    dir: PathBuf,
    prefix: String,
    sample_rate: u32,
    names: Arc<Mutex<HashMap<PeerId, String>>>,
    dropped: Arc<AtomicU64>,
    part_frames: u64,
) -> Result<RecordingResult, String> {
    // (as it is given, less what a file name cannot have: it ends in a space, and may be empty)
    let prefix: std::sync::Arc<str> = prefix.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).collect::<String>().into();
    let mut tracks: HashMap<PeerId, Track> = HashMap::new();
    let mut order: Vec<PeerId> = Vec::new();
    let mut end: u64 = 0;
    let mut err: Option<String> = None;
    loop {
        let mut got = false;
        while let Ok(b) = rx.pop() {
            got = true;
            if err.is_some() {
                continue;
            }
            let t = tracks.entry(b.track).or_insert_with(|| {
                let name = names.lock().ok().and_then(|n| n.get(&b.track).cloned()).unwrap_or_else(|| format!("peer{}", b.track));
                let n = if b.track == SELF_TRACK { 0 } else { order.iter().filter(|&&t| t != SELF_TRACK).count() + 1 };
                order.push(b.track);
                Track::new(prefix.clone(), n, name, b.pos, part_frames)
            });
            // a track that joins late (or had a gap) gets silence there, so all files line up
            let gap = b.pos.saturating_sub(t.written);
            let frames = b.frames as usize;
            let wrote = t
                .put(&dir, sample_rate, part_frames, None, gap)
                .and_then(|_| t.put(&dir, sample_rate, part_frames, Some(&b.data[..frames * 2]), frames as u64));
            match wrote {
                Ok(()) => end = end.max(t.written),
                Err(e) => err = Some(e),
            }
        }
        if !got {
            if rx.is_abandoned() && rx.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    // every file is finished, also when writing failed along the way (a full disk): what
    // was written until then stays a file that plays. Left unfinished, its header said
    // "empty", and the whole recording was lost with the last minute
    let mut files = Vec::new();
    for id in order {
        let Some(mut t) = tracks.remove(&id) else { continue };
        if err.is_none() {
            // every track as long as the longest (within its last file: one that ended parts
            // ago is not filled up with files of silence)
            let part_end = (t.written.max(1) - 1) / part_frames * part_frames + part_frames;
            let pad = end.min(part_end).saturating_sub(t.written);
            err = t.put(&dir, sample_rate, part_frames, None, pad).err();
        }
        if let Err(e) = t.close() {
            err.get_or_insert(e);
        }
        files.append(&mut t.files);
    }
    if let Some(e) = err {
        return Err(if files.is_empty() { e } else { format!("{e}. 그때까지 녹음된 부분은 파일로 남아 있어요.") });
    }
    Ok(RecordingResult { dir, files, seconds: end as f32 / sample_rate as f32, dropped_blocks: dropped.load(Relaxed) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_writes_aligned_tracks() {
        let dir = std::env::temp_dir().join(format!("gachi-rec-test-{}", std::process::id()));
        let names = Arc::new(Mutex::new(HashMap::from([(SELF_TRACK, "나".to_string()), (7, "민지/기타".to_string())])));
        let (mut tap, rec) = Recorder::start(dir.clone(), "합주 0903 - ".into(), 48_000, names, 3).unwrap();
        let block = [0.5f32; 128]; // 64 frames
        for i in 0..100 {
            tap.write(0, SELF_TRACK, &block);
            if i >= 50 {
                tap.write(1, 7, &block); // joins halfway
            }
            tap.pos += 64;
        }
        tap.flush_all();
        drop(tap);
        let r = rec.finish().unwrap();
        assert_eq!(r.files.len(), 2);
        let name = |i: usize| r.files[i].file_name().unwrap().to_string_lossy().to_string();
        assert_eq!((name(0), name(1)), ("합주 0903 - 00 나.wav".to_string(), "합주 0903 - 01 민지_기타.wav".to_string()));
        for f in &r.files {
            let reader = hound::WavReader::open(f).unwrap();
            assert_eq!(reader.duration(), 6400, "{f:?} padded to the same length");
        }
        let late: Vec<f32> = hound::WavReader::open(&r.files[1]).unwrap().samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(late[0], 0.0, "silence before the peer joined");
        assert_eq!(late[50 * 64 * 2], 0.5);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn metronome_clicks_on_the_beat() {
        let p = MetronomeParams::default();
        p.bpm.store(300.0); // 200 ms beats
        p.start_us.store(10_000); // beat 0 at 10 ms
        p.on.store(true, Relaxed);
        let mut m = Metronome::new(48_000, 0.0, Instant::now());
        let mut onsets = Vec::new();
        let mut quiet = u32::MAX; // samples since the last non-silent one
        let mut frame = 0u64;
        for _ in 0..(48_000 * 2 / 256) {
            let mut mix = [0.0f32; 512];
            // callbacks arrive with up to ±0.5 ms of timing jitter; the sample clock smooths it out
            let now = frame as f64 * 1e6 / 48_000.0 + if frame.is_multiple_of(3) { 500.0 } else { -400.0 };
            m.add_at(&p, &mut mix, now);
            for f in mix.as_chunks::<2>().0 {
                if f[0] != 0.0 {
                    if quiet > 100 {
                        onsets.push(frame);
                    }
                    quiet = 0;
                } else {
                    quiet = quiet.saturating_add(1);
                }
                frame += 1;
            }
        }
        assert_eq!(onsets.len(), 10, "10 clicks in 2 s at 300 BPM: {onsets:?}");
        for (i, &f) in onsets.iter().enumerate() {
            let want = (10_000.0 + i as f64 * 200_000.0) * 0.048;
            assert!((f as f64 - want).abs() < 48.0, "click {i} at frame {f}, want {want} (within 1 ms)");
        }
    }

    #[test]
    fn test_tone_plays_for_the_requested_length() {
        let mut t = ToneGen::new(48_000);
        let left = AtomicU32::new(1000);
        let mut total = 0;
        for _ in 0..10 {
            let mut mix = [0.0f32; 256];
            t.add(&left, &mut mix);
            total += mix.as_chunks::<2>().0.iter().filter(|f| f[0] != 0.0).count();
        }
        assert_eq!(left.load(Relaxed), 0);
        assert!((990..=1000).contains(&total), "{total}");
    }

    /// A long recording goes on in new files (a WAV holds 4 GB at most), every track at the
    /// same places, and a track that joins late starts in the part it joined in.
    #[test]
    fn a_long_recording_goes_on_in_parts_that_line_up() {
        let dir = std::env::temp_dir().join(format!("gachi-rec-parts-{}", std::process::id()));
        let names = Arc::new(Mutex::new(HashMap::from([(SELF_TRACK, "me".to_string()), (7, "late".to_string())])));
        // parts of 1000 frames; blocks of 64 do not end where the parts do
        let (mut tap, rec) = Recorder::start_parts(dir.clone(), String::new(), 48_000, names, 3, 1000).unwrap();
        let block = [0.5f32; 128];
        for i in 0..40 {
            tap.write(0, SELF_TRACK, &block);
            if i >= 20 {
                tap.write(1, 7, &block); // joins at frame 1280: in the second part
            }
            tap.pos += 64;
        }
        tap.flush_all();
        drop(tap);
        let r = rec.finish().unwrap();
        let len = |name: &str| hound::WavReader::open(dir.join(name)).map(|w| w.duration()).unwrap_or(u32::MAX);
        // 2560 frames: two whole parts and 560 of the third
        assert_eq!((len("00 me.wav"), len("00 me - 2.wav"), len("00 me - 3.wav")), (1000, 1000, 560));
        assert_eq!((len("01 late - 2.wav"), len("01 late - 3.wav")), (1000, 560));
        assert!(!dir.join("01 late.wav").exists(), "no file of silence for the part before it joined");
        assert_eq!(r.files.len(), 5);
        let late: Vec<f32> = hound::WavReader::open(dir.join("01 late - 2.wav")).unwrap().samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!((late[279 * 2], late[280 * 2]), (0.0, 0.5), "its sound starts at frame 1280 of the timeline");
        let me: Vec<f32> = hound::WavReader::open(dir.join("00 me - 2.wav")).unwrap().samples::<f32>().map(|s| s.unwrap()).collect();
        assert!(me.iter().all(|&s| s == 0.5), "nothing missing where a block is split between two files");
        std::fs::remove_dir_all(dir).ok();
    }
}
