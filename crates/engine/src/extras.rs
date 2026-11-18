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
    /// beats counted in from `start_us` before bar 1 (whole bars), in a higher click
    pub count_in: AtomicU32,
    pub gain: AtomicF32,
    /// this machine's output latency in µs: each click is played this much ahead, so it
    /// sounds on the beat. The real figure (the driver's report, else the real block),
    /// kept current by the engine
    pub out_latency_us: AtomicU32,
}

impl Default for MetronomeParams {
    fn default() -> Self {
        Self {
            on: AtomicBool::new(false),
            bpm: AtomicF32::new(120.0),
            start_us: AtomicI64::new(0),
            beats_per_bar: AtomicU32::new(4),
            count_in: AtomicU32::new(0),
            gain: AtomicF32::new(0.5),
            out_latency_us: AtomicU32::new(0),
        }
    }
}

pub(crate) struct Metronome {
    sr: f64,
    /// µs from writing a sample until it leaves the speaker (output buffer)
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
    pub fn new(sr: u32, origin: Instant) -> Self {
        Self {
            sr: sr as f64,
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
        let t0 = block_us + p.out_latency_us.load(Relaxed) as f64 - p.start_us.load() as f64;
        let per_bar = p.beats_per_bar.load(Relaxed).max(1) as i64;
        let count_in = p.count_in.load(Relaxed) as i64;
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
                            // the downbeat is accented; the bars counted in are a fifth higher,
                            // so that one hears where bar 1 begins
                            let down = (beat - count_in).rem_euclid(per_bar) == 0;
                            let freq = match (beat < count_in, down) {
                                (true, true) => 2640.0,
                                (true, false) => 1980.0,
                                (false, true) => 1760.0,
                                (false, false) => 1320.0,
                            };
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

use crate::recfile::{RecFormat, Sink};

/// Track id used for your own input.
pub const SELF_TRACK: PeerId = 0;
/// Track id of the mix: everyone together, as heard here.
pub const MIX_TRACK: PeerId = PeerId::MAX;
const BLOCK: usize = 256;
/// ~2.5 s of 18 tracks (a full room and the mix) at 48 kHz between the audio thread and the
/// writer: a disk that stops for a moment (an antivirus scan, a drive waking up) loses nothing
const RING_BLOCKS: usize = 8192;

/// How a recording is made.
#[derive(Clone, Debug, Default)]
pub struct RecOptions {
    /// what every file's name starts with (which recording it is, for a file taken out of
    /// its folder)
    pub prefix: String,
    pub format: RecFormat,
    /// also one file of everyone together, as heard here (see [`MIX_TRACK`])
    pub mix: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct RecBlock {
    track: PeerId,
    /// position of the first frame on the recording timeline
    pos: u64,
    frames: u16,
    /// what the track is: 1 (the same sound in both of `data`'s channels) or 2
    channels: u8,
    data: [f32; BLOCK * 2],
}

impl RecBlock {
    const EMPTY: RecBlock = RecBlock { track: 0, pos: 0, frames: 0, channels: 2, data: [0.0; BLOCK * 2] };
}

/// Lives in the output callback while recording. One accumulator per track slot, so blocks
/// go out full (fewer, bigger ring pushes).
pub(crate) struct RecTap {
    tx: rtrb::Producer<RecBlock>,
    acc: Box<[RecBlock]>,
    pub pos: u64,
    /// the mix is wanted too
    pub mix: bool,
    dropped: Arc<AtomicU64>,
}

impl RecTap {
    /// `slot` is a stable index per track (0 = self, 1.. = peer slots, then the mix).
    /// `stereo` is interleaved stereo whatever the track is; `channels` says what it is (a
    /// mono track has the same sound in both, and is written as one channel).
    pub fn write(&mut self, slot: usize, track: PeerId, stereo: &[f32], channels: u8) {
        let Some(mut a) = self.acc.get(slot).copied() else { return };
        let mut pos = self.pos;
        let mut rest = stereo;
        // a different track took this slot, there was a gap, or the track turned stereo (or
        // mono): flush what we had
        if a.frames > 0 && (a.track != track || a.pos + a.frames as u64 != pos || a.channels != channels) {
            self.flush(&mut a);
        }
        while !rest.is_empty() {
            if a.frames == 0 {
                a.track = track;
                a.pos = pos;
                a.channels = channels;
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

/// The writer side: a thread that drains the ring into one file per track.
pub(crate) struct Recorder {
    thread: thread::JoinHandle<Result<RecordingResult, String>>,
    /// writing failed (a full disk…): said here as soon as it happens, so that the user
    /// hears of it then and not when the recording is stopped an hour later
    error: Arc<Mutex<Option<String>>>,
}

impl Recorder {
    /// Creates the tap (goes to the audio thread) and starts the writer.
    /// `names` maps track ids to display names; it is read when a track first appears.
    /// Files are named `<prefix><number> <name>.<ext>`.
    pub fn start(
        dir: PathBuf,
        opts: RecOptions,
        sample_rate: u32,
        names: Arc<Mutex<HashMap<PeerId, String>>>,
        slots: usize,
    ) -> Result<(RecTap, Recorder), String> {
        let part_frames = opts.format.part_frames();
        Self::start_parts(dir, opts, sample_rate, names, slots, part_frames)
    }

    /// (`part_frames`: see [`RecFormat::part_frames`]; tests use small parts)
    fn start_parts(
        dir: PathBuf,
        opts: RecOptions,
        sample_rate: u32,
        names: Arc<Mutex<HashMap<PeerId, String>>>,
        slots: usize,
        part_frames: u64,
    ) -> Result<(RecTap, Recorder), String> {
        std::fs::create_dir_all(&dir).map_err(|e| gachi_lang::tr!("rec.cant_create_recording", e))?;
        let (tx, rx) = rtrb::RingBuffer::new(RING_BLOCKS);
        let dropped = Arc::new(AtomicU64::new(0));
        let error = Arc::new(Mutex::new(None));
        let tap = RecTap { tx, acc: vec![RecBlock::EMPTY; slots].into_boxed_slice(), pos: 0, mix: opts.mix, dropped: dropped.clone() };
        let said = error.clone();
        let thread = thread::Builder::new()
            .name("gachi-rec-writer".into())
            .spawn(move || write_loop(rx, dir, opts, sample_rate, names, dropped, said, part_frames))
            .map_err(|e| e.to_string())?;
        Ok((tap, Recorder { thread, error }))
    }

    /// Writing failed: why. The recording goes on taking sound (and throwing it away) until
    /// it is stopped; what was written until then is kept.
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    /// Waits for the writer to finish (the tap must already be dropped by the audio thread).
    pub fn finish(self) -> Result<RecordingResult, String> {
        self.thread.join().map_err(|_| gachi_lang::tr!("rec.something_went_wrong"))?
    }
}

/// `s` as (part of) a file name: without the characters Windows does not take in one.
pub fn safe_name(s: &str) -> String {
    let s: String = s.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let s = s.trim().trim_end_matches('.').to_string();
    if s.is_empty() { "track".into() } else { s }
}

/// What every track of one recording is written with.
struct Files<'a> {
    dir: &'a Path,
    prefix: &'a str,
    format: RecFormat,
    sample_rate: u32,
    /// frames to a file (see [`RecFormat::part_frames`])
    part_frames: u64,
}

/// One track of a recording: its files (one per part of the timeline).
struct Track {
    /// the file being written (opened with the first frame that goes into it)
    sink: Option<Sink>,
    /// which part of the timeline that file is
    part: u64,
    /// frames of the timeline this track is written up to
    written: u64,
    /// its number (the mix has none) and name in the file names
    n: Option<usize>,
    name: String,
    /// 1 or 2: a track sent as mono is written as mono
    channels: usize,
    /// it was mono and turned stereo: its files from then on say so
    widened: bool,
    files: Vec<PathBuf>,
    mono: Vec<f32>,
}

impl Track {
    /// A track whose first sound is at `pos`: it starts with the part `pos` is in (a track
    /// that joins hours in has no files of silence for the hours before).
    fn new(n: Option<usize>, name: String, pos: u64, channels: usize, part_frames: u64) -> Self {
        Self {
            sink: None,
            part: pos / part_frames,
            written: pos - pos % part_frames,
            n,
            name,
            channels,
            widened: false,
            files: Vec::new(),
            mono: Vec::new(),
        }
    }

    /// The track is `channels` wide from here. A file cannot change its width: a mono
    /// track that turns stereo goes on in a stereo file (one that turns mono stays in its
    /// stereo file, the same sound in both channels).
    fn widen(&mut self, channels: usize) -> Result<(), String> {
        if channels > self.channels {
            self.close()?;
            self.widened = !self.files.is_empty();
            self.channels = channels;
        }
        Ok(())
    }

    fn file_name(&self, f: &Files, part: u64) -> String {
        let num = self.n.map(|n| format!("{n:02} ")).unwrap_or_default();
        let wide = if self.widened { gachi_lang::tr!("rec.stereo_file") } else { String::new() };
        let more = if part > 0 { format!(" - {}", part + 1) } else { String::new() };
        format!("{}{num}{}{wide}{more}.{}", f.prefix, safe_name(&self.name), f.format.ext())
    }

    /// Writes `frames` frames at the track's end, into the part (or parts) they fall in:
    /// `data` (interleaved stereo), or silence.
    fn put(&mut self, f: &Files, data: Option<&[f32]>, frames: u64) -> Result<(), String> {
        let mut done = 0;
        while done < frames {
            let part = self.written / f.part_frames;
            if self.sink.is_none() || part != self.part {
                self.close()?;
                let path = f.dir.join(self.file_name(f, part));
                let mut sink = Sink::create(&path, f.format, self.channels, f.sample_rate)?;
                self.files.push(path);
                // a file that starts inside its part (the track turned stereo there):
                // silence up to here, so that it lines up with every other file
                sink.silence(self.written - part * f.part_frames, self.channels)?;
                self.sink = Some(sink);
                self.part = part;
            }
            let Self { sink, mono, channels, .. } = self;
            let Some(sink) = sink.as_mut() else { break };
            let n = (frames - done).min((part + 1) * f.part_frames - self.written);
            match data {
                Some(d) => {
                    let d = &d[done as usize * 2..(done + n) as usize * 2];
                    if *channels == 2 {
                        sink.write(d)?;
                    } else {
                        mono.clear();
                        mono.extend(d.iter().step_by(2));
                        sink.write(mono)?;
                    }
                }
                None => sink.silence(n, *channels)?,
            }
            self.written += n;
            done += n;
        }
        Ok(())
    }

    /// Finishes the file being written (its header gets the length).
    fn close(&mut self) -> Result<(), String> {
        match self.sink.take() {
            Some(s) => s.finish(),
            None => Ok(()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_loop(
    mut rx: rtrb::Consumer<RecBlock>,
    dir: PathBuf,
    opts: RecOptions,
    sample_rate: u32,
    names: Arc<Mutex<HashMap<PeerId, String>>>,
    dropped: Arc<AtomicU64>,
    said: Arc<Mutex<Option<String>>>,
    part_frames: u64,
) -> Result<RecordingResult, String> {
    // (as it is given, less what a file name cannot have: it ends in a space, and may be empty)
    let prefix: String = opts.prefix.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let f = Files { dir: &dir, prefix: &prefix, format: opts.format, sample_rate, part_frames };
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
            let channels = if b.channels == 1 { 1 } else { 2 };
            let t = tracks.entry(b.track).or_insert_with(|| {
                let (n, name) = match b.track {
                    SELF_TRACK => (Some(0), None),
                    MIX_TRACK => (None, Some(gachi_lang::tr!("rec.mix_file"))),
                    _ => (Some(order.iter().filter(|&&t| t != SELF_TRACK && t != MIX_TRACK).count() + 1), None),
                };
                let name = name.or_else(|| names.lock().ok().and_then(|n| n.get(&b.track).cloned())).unwrap_or_else(|| format!("peer{}", b.track));
                order.push(b.track);
                Track::new(n, name, b.pos, channels, part_frames)
            });
            // a track that joins late (or had a gap) gets silence there, so all files line up
            let gap = b.pos.saturating_sub(t.written);
            let frames = b.frames as usize;
            let wrote = t.put(&f, None, gap).and_then(|_| t.widen(channels)).and_then(|_| t.put(&f, Some(&b.data[..frames * 2]), frames as u64));
            match wrote {
                Ok(()) => end = end.max(t.written),
                Err(e) => {
                    tracing::error!("recording: {e}");
                    if let Ok(mut s) = said.lock() {
                        *s = Some(e.clone());
                    }
                    err = Some(e);
                }
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
            err = t.put(&f, None, pad).err();
        }
        if let Err(e) = t.close() {
            err.get_or_insert(e);
        }
        files.append(&mut t.files);
    }
    if let Some(e) = err {
        return Err(if files.is_empty() { e } else { gachi_lang::tr!("rec.e_what_recorded", e) });
    }
    Ok(RecordingResult { dir, files, seconds: end as f32 / sample_rate as f32, dropped_blocks: dropped.load(Relaxed) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 32 bit float WAV (what the files are read back as here).
    fn float(prefix: &str) -> RecOptions {
        RecOptions { prefix: prefix.into(), format: RecFormat::Wav { bits: 32 }, mix: false }
    }

    #[test]
    fn recording_writes_aligned_tracks() {
        let dir = std::env::temp_dir().join(format!("gachi-rec-test-{}", std::process::id()));
        let names = Arc::new(Mutex::new(HashMap::from([(SELF_TRACK, "나".to_string()), (7, "민지/기타".to_string())])));
        let (mut tap, rec) = Recorder::start(dir.clone(), float("합주 0903 - "), 48_000, names, 3).unwrap();
        let block = [0.5f32; 128]; // 64 frames
        for i in 0..100 {
            tap.write(0, SELF_TRACK, &block, 2);
            if i >= 50 {
                tap.write(1, 7, &block, 2); // joins halfway
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
        let mut m = Metronome::new(48_000, Instant::now());
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
    fn clicks_leave_as_early_as_the_output_is_late() {
        // the first click's frame in a block stream, for an output latency (µs)
        let first = |latency_us: u32| {
            let p = MetronomeParams::default();
            p.bpm.store(120.0);
            p.start_us.store(100_000); // beat 0 at 100 ms
            p.out_latency_us.store(latency_us, Relaxed);
            p.on.store(true, Relaxed);
            let mut m = Metronome::new(48_000, Instant::now());
            for b in 0..200u64 {
                let mut mix = [0.0f32; 128];
                m.add_at(&p, &mut mix, (b * 64) as f64 * 1e6 / 48_000.0);
                if let Some(k) = mix.as_chunks::<2>().0.iter().position(|f| f[0] != 0.0) {
                    return b * 64 + k as u64;
                }
            }
            panic!("no click");
        };
        // (a click starts at phase 0: its first sound is the sample after the beat)
        let on_beat = first(0);
        assert!((4800..=4801).contains(&on_beat), "on the beat itself with no output latency: {on_beat}");
        // 64 frames of buffer and the driver's own: ~3 ms, played 144 frames sooner
        assert_eq!(on_beat - first(3000), 144);
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
        let (mut tap, rec) = Recorder::start_parts(dir.clone(), float(""), 48_000, names, 3, 1000).unwrap();
        let block = [0.5f32; 128];
        for i in 0..40 {
            tap.write(0, SELF_TRACK, &block, 2);
            if i >= 20 {
                tap.write(1, 7, &block, 2); // joins at frame 1280: in the second part
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

    /// A track sent as mono is a mono file; the mix has a name instead of a number; and a
    /// mono track that turns stereo goes on in a stereo file that lines up with the rest.
    #[test]
    fn mono_tracks_are_mono_files_and_the_mix_has_its_name() {
        // file names are made on the recorder's thread: the whole process speaks Korean here
        // (no engine test asks for another language)
        gachi_lang::choose("ko-KR");
        let dir = std::env::temp_dir().join(format!("gachi-rec-mono-{}", std::process::id()));
        let names = Arc::new(Mutex::new(HashMap::from([(SELF_TRACK, "me".to_string()), (7, "gt".to_string())])));
        let opts = RecOptions { prefix: "r - ".into(), format: RecFormat::Wav { bits: 16 }, mix: true };
        let (mut tap, rec) = Recorder::start(dir.clone(), opts, 48_000, names, 4).unwrap();
        assert!(tap.mix);
        let block = [0.25f32; 128]; // 64 frames
        for i in 0..40 {
            tap.write(0, SELF_TRACK, &block, 1);
            tap.write(1, 7, &block, if i < 10 { 1 } else { 2 }); // turns stereo at frame 640
            tap.write(3, MIX_TRACK, &block, 2);
            tap.pos += 64;
        }
        tap.flush_all();
        drop(tap);
        let r = rec.finish().unwrap();
        assert!(rec_error_free(&r));
        let spec = |name: &str| {
            let w = hound::WavReader::open(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            (w.spec().channels, w.spec().bits_per_sample, w.duration())
        };
        assert_eq!(spec("r - 00 me.wav"), (1, 16, 2560), "mono, as it was sent");
        assert_eq!(spec("r - 믹스.wav"), (2, 16, 2560), "the mix: stereo, no number");
        // the guitar: 640 frames of mono (the file ends where it turned stereo); then a
        // stereo file of the whole length whose sound starts at frame 640
        assert_eq!(spec("r - 01 gt.wav"), (1, 16, 640));
        assert_eq!(spec("r - 01 gt (스테레오).wav"), (2, 16, 2560));
        let wide: Vec<i16> = hound::WavReader::open(dir.join("r - 01 gt (스테레오).wav")).unwrap().samples::<i16>().map(|s| s.unwrap()).collect();
        assert!(wide[..640 * 2].iter().all(|&s| s == 0), "silence before it turned stereo");
        assert!(wide[640 * 2..].iter().all(|&s| (s - 8192).abs() <= 2), "its sound from there on");
        assert_eq!(r.files.len(), 4);
        std::fs::remove_dir_all(dir).ok();
    }

    fn rec_error_free(r: &RecordingResult) -> bool {
        r.dropped_blocks == 0 && !r.files.is_empty()
    }

    /// Writing that fails is said at once, not when the recording is stopped.
    #[test]
    fn a_write_that_fails_is_said_at_once() {
        // a folder where the first file should be: it cannot be created
        let dir = std::env::temp_dir().join(format!("gachi-rec-fail-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("x - 00 me.wav")).unwrap();
        let names = Arc::new(Mutex::new(HashMap::from([(SELF_TRACK, "me".to_string())])));
        let (mut tap, rec) = Recorder::start(dir.clone(), float("x - "), 48_000, names, 2).unwrap();
        assert_eq!(rec.error(), None);
        let block = [0.25f32; 512];
        tap.write(0, SELF_TRACK, &block, 2);
        let t = Instant::now();
        while rec.error().is_none() && t.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(rec.error().is_some(), "the writer said nothing while the recording went on");
        drop(tap);
        assert!(rec.finish().is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    /// The bars counted in click higher than the bars that follow, beat for beat in step.
    #[test]
    fn the_count_in_clicks_higher_then_the_bars_begin() {
        let p = MetronomeParams::default();
        p.bpm.store(300.0); // 200 ms beats
        p.start_us.store(0);
        p.beats_per_bar.store(2, Relaxed);
        p.count_in.store(2, Relaxed); // one bar of two beats
        p.on.store(true, Relaxed);
        let mut m = Metronome::new(48_000, Instant::now());
        let mut out = Vec::new();
        for i in 0..(48_000 / 256) {
            let mut mix = [0.0f32; 512];
            m.add_at(&p, &mut mix, i as f64 * 256.0 * 1e6 / 48_000.0);
            out.extend(mix.as_chunks::<2>().0.iter().map(|f| f[0]));
        }
        // the pitch of the click at each beat: times the sound crosses zero going up, in
        // its first 10 ms
        let pitch = |beat: usize| {
            let at = beat * 9600;
            let c = out[at..at + 480].windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
            c as f32 * 100.0
        };
        let near = |hz: f32, want: f32| (hz - want).abs() <= 150.0;
        let got: Vec<f32> = (0..4).map(pitch).collect();
        assert!(near(got[0], 2640.0) && near(got[1], 1980.0), "the bar counted in: {got:?}");
        assert!(near(got[2], 1760.0) && near(got[3], 1320.0), "bar 1 and on: {got:?}");
    }
}
