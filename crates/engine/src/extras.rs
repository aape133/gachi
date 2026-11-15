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
/// ~2.5 s of 9 tracks at 48 kHz between the audio thread and the writer
const RING_BLOCKS: usize = 4096;

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
    pub fn start(
        dir: PathBuf,
        sample_rate: u32,
        names: Arc<Mutex<HashMap<PeerId, String>>>,
        slots: usize,
    ) -> Result<(RecTap, Recorder), String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("녹음 폴더를 만들 수 없어요 ({e})"))?;
        let (tx, rx) = rtrb::RingBuffer::new(RING_BLOCKS);
        let dropped = Arc::new(AtomicU64::new(0));
        let tap = RecTap { tx, acc: vec![RecBlock::EMPTY; slots].into_boxed_slice(), pos: 0, dropped: dropped.clone() };
        let thread = thread::Builder::new()
            .name("gachi-rec-writer".into())
            .spawn(move || write_loop(rx, dir, sample_rate, names, dropped))
            .map_err(|e| e.to_string())?;
        Ok((tap, Recorder { thread }))
    }

    /// Waits for the writer to finish (the tap must already be dropped by the audio thread).
    pub fn finish(self) -> Result<RecordingResult, String> {
        self.thread.join().map_err(|_| "녹음 저장 중 오류가 났어요".to_string())?
    }
}

fn safe_name(s: &str) -> String {
    let s: String = s.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let s = s.trim().trim_end_matches('.').to_string();
    if s.is_empty() { "track".into() } else { s }
}

struct Track {
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    written: u64,
    path: PathBuf,
}

fn open_track(dir: &Path, n: usize, name: &str, sample_rate: u32) -> Result<Track, String> {
    let path = dir.join(format!("{n:02}-{}.wav", safe_name(name)));
    let spec = hound::WavSpec { channels: 2, sample_rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let writer = hound::WavWriter::create(&path, spec).map_err(|e| format!("파일을 만들 수 없어요 ({e})"))?;
    Ok(Track { writer, written: 0, path })
}

fn write_loop(
    mut rx: rtrb::Consumer<RecBlock>,
    dir: PathBuf,
    sample_rate: u32,
    names: Arc<Mutex<HashMap<PeerId, String>>>,
    dropped: Arc<AtomicU64>,
) -> Result<RecordingResult, String> {
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
            if let std::collections::hash_map::Entry::Vacant(e) = tracks.entry(b.track) {
                let name = names.lock().unwrap().get(&b.track).cloned().unwrap_or_else(|| format!("peer{}", b.track));
                let n = if b.track == SELF_TRACK { 0 } else { order.iter().filter(|&&t| t != SELF_TRACK).count() + 1 };
                match open_track(&dir, n, &name, sample_rate) {
                    Ok(t) => {
                        e.insert(t);
                        order.push(b.track);
                    }
                    Err(e) => {
                        err = Some(e);
                        continue;
                    }
                }
            }
            let t = tracks.get_mut(&b.track).expect("inserted above");
            // a track that joins late (or had a gap) starts with silence, so all files line up
            while t.written < b.pos {
                let _ = t.writer.write_sample(0.0f32);
                let _ = t.writer.write_sample(0.0f32);
                t.written += 1;
            }
            for &s in &b.data[..b.frames as usize * 2] {
                if let Err(e) = t.writer.write_sample(s) {
                    err = Some(format!("녹음 파일에 쓸 수 없어요 ({e})"));
                    break;
                }
            }
            t.written += b.frames as u64;
            end = end.max(t.written);
        }
        if !got {
            if rx.is_abandoned() && rx.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    let mut files = Vec::new();
    for id in order {
        let mut t = tracks.remove(&id).expect("listed");
        // pad every track to the same length
        while t.written < end {
            let _ = t.writer.write_sample(0.0f32);
            let _ = t.writer.write_sample(0.0f32);
            t.written += 1;
        }
        t.writer.finalize().map_err(|e| format!("녹음 파일을 마무리할 수 없어요 ({e})"))?;
        files.push(t.path);
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
        let (mut tap, rec) = Recorder::start(dir.clone(), 48_000, names, 3).unwrap();
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
        assert!(r.files[1].file_name().unwrap().to_string_lossy().contains("민지_기타"));
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
}
