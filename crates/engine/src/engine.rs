use crate::device::{find_device, host, HostChoice};
use crate::extras::{Metronome, MetronomeParams, RecOptions, RecTap, Recorder, RecordingResult, ToneGen, MIX_TRACK, SELF_TRACK};
use crate::frame::Frame;
use crate::jitter::JitterBuffer;
use crate::meter::{ChannelMeters, Meter, MAX_INPUT_METERS};
use crate::params::{PeerParams, SelfParams};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use gachi_proto::audio::MAX_FRAMES;
use gachi_proto::PeerId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Max peers mixed at once (slots are preallocated).
pub const MAX_PEERS: usize = 16;
/// Callback blocks are processed in chunks of this many frames (scratch buffers are this big).
const CHUNK: usize = 2048;
/// Per-peer ring between the network thread and the output callback: as many packets as
/// the jitter buffer has slots. After a stall everything sent meanwhile arrives within a
/// millisecond, between two callbacks; at 64 a stall over ~40 ms overflowed it, and the
/// packets dropped were the newest, the very ones playback goes on with (a Wi-Fi-like link
/// in `policy_sim`: four times as much broken sound).
const PEER_RING: usize = 256;
/// Own sound from the input to the output callback, in samples (interleaved stereo): two
/// of the biggest blocks.
const MONITOR_RING: usize = CHUNK * 2 * 2;

/// Receives each finished packet from the input callback. Called on the audio thread:
/// must not block or allocate.
pub trait PacketSink: Send + 'static {
    fn send(&mut self, frame: &Frame);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputChannels {
    /// one input (0-based), sent as mono
    Mono(usize),
    /// two inputs (0-based), sent as stereo
    Stereo(usize, usize),
    /// a mono input (e.g. a mic) in the center plus a stereo pair on top (e.g. the
    /// interface's loopback with a backing track), sent as stereo
    Mix { mono: usize, pair: (usize, usize) },
}

impl InputChannels {
    pub fn is_mono(&self) -> bool {
        matches!(self, Self::Mono(_))
    }

    /// Packed into one number, so the audio thread can pick up a change from an atomic.
    pub fn encode(self) -> u64 {
        let (tag, a, b, c) = match self {
            Self::Mono(i) => (1u64, i, 0, 0),
            Self::Stereo(i, j) => (2, i, j, 0),
            Self::Mix { mono, pair: (i, j) } => (3, mono, i, j),
        };
        let f = |x: usize| (x as u64) & 0xFFFF;
        tag << 56 | f(a) | f(b) << 16 | f(c) << 32
    }

    pub fn decode(v: u64) -> Option<Self> {
        let f = |shift: u32| ((v >> shift) & 0xFFFF) as usize;
        match v >> 56 {
            1 => Some(Self::Mono(f(0))),
            2 => Some(Self::Stereo(f(0), f(16))),
            3 => Some(Self::Mix { mono: f(0), pair: (f(16), f(32)) }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub host: HostChoice,
    /// device name; `None`/empty = system default
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub sample_rate: u32,
    pub buffer_size: u32,
    /// frames per network packet; 0 = follow the device's block (see [`auto_packet_frames`])
    pub packet_frames: u16,
    pub input: InputChannels,
}

/// The largest packet "automatic" makes: a lost packet is a hole this long (1.3 ms at
/// 48 kHz) and the jitter buffer counts in these, so bigger blocks are sent in parts.
pub const AUTO_PACKET_MAX: usize = 64;

/// The packet size that fits a device block of `block` frames: the block itself up to
/// [`AUTO_PACKET_MAX`], else the largest equal part of it that is not bigger (96 → 48,
/// 128 → 64, 144 → 48, 480 → 60). A packet as long as the block leaves when the block
/// arrives: a smaller one gains nothing (the block's packets leave together) and costs
/// packets, a bigger or uneven one waits for the next block. Blocks with no usable equal
/// part (a prime number of frames, say) get the maximum.
pub fn auto_packet_frames(block: usize) -> usize {
    const MIN: usize = 8;
    if block == 0 {
        return AUTO_PACKET_MAX;
    }
    if block <= AUTO_PACKET_MAX {
        // (tiny blocks: several to a packet)
        return if block >= MIN { block } else { block * MIN.div_ceil(block) };
    }
    match (MIN * 2..=AUTO_PACKET_MAX).rev().find(|d| block.is_multiple_of(*d)) {
        Some(d) => d,
        None => AUTO_PACKET_MAX,
    }
}

/// What the device actually gave us (may differ from what was asked).
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub sample_rate: u32,
    /// `None` when the driver picks the buffer size itself
    pub buffer_size: Option<u32>,
    pub input_device: String,
    pub output_device: String,
    /// how many input channels the input device has
    pub input_channels: u16,
    /// ASIO: the driver's own (input, output) latency in frames, as it reports it (buffer
    /// plus safety offset and transfer: more than the buffer alone). `None` elsewhere.
    pub driver_latency: Option<(u32, u32)>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("no {0} device")]
    NoDevice(&'static str),
    #[error("unsupported device config: {0}")]
    Unsupported(String),
    #[error("device config: {0}")]
    Config(#[from] cpal::DefaultStreamConfigError),
    #[error("could not open stream: {0}")]
    Build(#[from] cpal::BuildStreamError),
    #[error("could not start stream: {0}")]
    Play(#[from] cpal::PlayStreamError),
    #[error("audio thread failed: {0}")]
    Thread(String),
}

/// Handle to a peer's slot in the mixer.
#[derive(Clone)]
pub struct PeerHandle {
    pub params: Arc<PeerParams>,
    pub meter: Arc<Meter>,
    pub stats: Arc<SharedJitterStats>,
}

const WARMUP_CALLBACKS: u32 = 8;

/// Frames per callback as the driver actually delivers them (WASAPI shared mode usually
/// ignores the requested buffer and runs ~10 ms). Written by the audio callbacks.
pub struct CallbackStats {
    pub last: AtomicU32,
    pub min: AtomicU32,
    pub max: AtomicU32,
    pub count: AtomicU32,
    /// input side: frames per network packet in use now (it follows the block when the
    /// packet size is automatic)
    pub packet: AtomicU32,
    /// how long our processing takes per callback (incl. sending, on the input side)
    pub load: crate::load::CallbackLoad,
}

impl Default for CallbackStats {
    fn default() -> Self {
        Self {
            last: AtomicU32::new(0),
            min: AtomicU32::new(u32::MAX),
            max: AtomicU32::new(0),
            count: AtomicU32::new(0),
            packet: AtomicU32::new(0),
            load: Default::default(),
        }
    }
}

impl CallbackStats {
    #[inline]
    fn record(&self, frames: u32) {
        self.last.store(frames, Relaxed);
        // the first callbacks are not typical: WASAPI fills its whole output buffer once
        // (e.g. 1056 frames) before settling into its period (480)
        if self.count.fetch_add(1, Relaxed) >= WARMUP_CALLBACKS {
            self.min.fetch_min(frames, Relaxed);
            self.max.fetch_max(frames, Relaxed);
        }
    }

    /// (min, max) frames per callback once past the first few, if known yet.
    pub fn range(&self) -> Option<(u32, u32)> {
        match self.count.load(Relaxed) {
            0 => None,
            // still warming up: the latest callback is a better guess than nothing
            n if n <= WARMUP_CALLBACKS => {
                let last = self.last.load(Relaxed);
                Some((last, last))
            }
            _ => Some((self.min.load(Relaxed), self.max.load(Relaxed))),
        }
    }
}

/// Jitter stats published by the output callback.
#[derive(Default)]
pub struct SharedJitterStats {
    pub depth: AtomicU32,
    pub target: AtomicU32,
    pub underruns: AtomicU32,
    pub concealed: AtomicU32,
    pub late: AtomicU32,
    /// clock drift correction currently applied, ppm
    pub drift_ppm: AtomicI32,
}

struct PeerSlot {
    id: PeerId,
    rx: rtrb::Consumer<Frame>,
    jitter: JitterBuffer,
    handle: PeerHandle,
    /// the left and right gains in use (see `glide`)
    gains: (f32, f32),
    /// what its packets carry now: 1 (mono) or 2 (a recording writes mono as mono)
    channels: u8,
}

/// Gains move to where the faders (and mute, solo) put them over a few ms instead of
/// jumping: a jump in the middle of a sound is a click, and a fader dragged is a row of
/// them. `cur` is NaN until first used (then it starts at the target).
const GLIDE_S: f32 = 0.004;

fn glide_step(sample_rate: u32) -> f32 {
    1.0 - (-1.0 / (GLIDE_S * sample_rate.max(1) as f32)).exp()
}

/// `cur` is at `target` (or is put there, when new or as good as there).
#[inline]
fn settled(cur: &mut f32, target: f32) -> bool {
    if cur.is_nan() || (*cur - target).abs() < 1e-4 {
        *cur = target;
    }
    *cur == target
}

/// One sample's step of `cur` towards `target`.
#[inline]
fn glide(cur: &mut f32, target: f32, step: f32) -> f32 {
    *cur += (target - *cur) * step;
    *cur
}

enum Cmd {
    AddPeer(Box<PeerSlot>),
    RemovePeer(PeerId),
    StartRec(Box<RecTap>),
    StopRec,
}

/// Things the audio thread hands back so they are freed elsewhere.
enum Garbage {
    Peer(#[allow(dead_code)] Box<PeerSlot>),
    Rec(#[allow(dead_code)] Box<RecTap>),
}

fn pack_latency(l: Option<(u32, u32)>) -> u64 {
    l.map_or(0, |(i, o)| (i as u64) << 32 | o as u64)
}

fn unpack_latency(v: u64) -> Option<(u32, u32)> {
    (v != 0).then_some(((v >> 32) as u32, v as u32))
}

/// Where the cpal streams live (they cannot move between threads).
enum Owner {
    /// our own small thread, parked until the engine is dropped
    Thread { stop_tx: mpsc::Sender<()>, thread: Option<thread::JoinHandle<()>> },
    /// the shared ASIO thread (see `host_thread`), by stream id
    Asio(u64),
    /// DAW mode: our bridge thread, woken by each block the plugin hands over
    #[cfg(windows)]
    Bridge { stop: Arc<AtomicBool>, thread: Option<thread::JoinHandle<()>>, end: Arc<gachi_bridge::AppEnd> },
}

pub struct Engine {
    cmd_tx: rtrb::Producer<Cmd>,
    garbage_rx: rtrb::Consumer<Garbage>,
    owner: Owner,
    pub info: StreamInfo,
    pub self_params: Arc<SelfParams>,
    pub self_meter: Arc<Meter>,
    pub master_meter: Arc<Meter>,
    /// peak per input channel of the device (see [`StreamInfo::input_channels`])
    pub input_meters: Arc<ChannelMeters>,
    pub metronome: Arc<MetronomeParams>,
    /// frames per input / output callback, as the driver really runs
    pub input_cb: Arc<CallbackStats>,
    pub output_cb: Arc<CallbackStats>,
    /// a stream reported an error (device unplugged, driver reset…)
    failed: Arc<AtomicBool>,
    started: Instant,
    origin: Instant,
    recorder: Option<Recorder>,
    rec_tap_back: bool,
    /// the (input, output) latency the ASIO driver reports now, packed `in << 32 | out`
    /// (0 = never told): read at the start and again at every driver check
    driver_latency_now: std::sync::atomic::AtomicU64,
    /// the others' sound comes back in what we send (see `feedback`)
    feedback: Arc<AtomicBool>,
    fb_stop: Arc<AtomicBool>,
    fb_thread: Option<thread::JoinHandle<()>>,
}

impl Engine {
    /// Opens the devices and starts both callbacks. cpal streams are not `Send` on every
    /// platform, so they live on an owner thread until the engine is dropped: the shared
    /// ASIO thread for ASIO (drivers are bound to one thread), else a thread of our own.
    pub fn start(cfg: EngineConfig, sink: Box<dyn PacketSink>) -> Result<Self, EngineError> {
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(32);
        let (garbage_tx, garbage_rx) = rtrb::RingBuffer::new(32);
        let self_params = Arc::new(SelfParams::default());
        self_params.input_map.store(cfg.input.encode(), Relaxed);
        let self_meter = Arc::new(Meter::default());
        let input_meters = Arc::new(ChannelMeters::default());
        let master_meter = Arc::new(Meter::default());
        let metronome = Arc::new(MetronomeParams::default());
        let input_cb = Arc::new(CallbackStats::default());
        let output_cb = Arc::new(CallbackStats::default());
        let failed = Arc::new(AtomicBool::new(false));
        let origin = Instant::now();
        // the feedback check: two taps the audio threads fill, read by a thread of its own
        let feedback = Arc::new(AtomicBool::new(false));
        let (fb_sent, fb_sent_rx) = rtrb::RingBuffer::new(FB_RING);
        let (fb_played, fb_played_rx) = rtrb::RingBuffer::new(FB_RING);
        let fb_stop = Arc::new(AtomicBool::new(false));
        let fb_thread = {
            let (feedback, stop) = (feedback.clone(), fb_stop.clone());
            thread::Builder::new()
                .name("gachi-feedback".into())
                .spawn(move || feedback_check(fb_sent_rx, fb_played_rx, &feedback, &stop))
                .ok()
        };
        let shared = Shared {
            self_params: self_params.clone(),
            self_meter: self_meter.clone(),
            master_meter: master_meter.clone(),
            input_meters: input_meters.clone(),
            metronome: metronome.clone(),
            input_cb: input_cb.clone(),
            output_cb: output_cb.clone(),
            failed: failed.clone(),
            origin,
            fb_sent,
            fb_played,
        };

        let daw = cfg.host == HostChoice::Daw;
        let (owner, info) = if daw {
            start_bridge(&cfg, sink, cmd_rx, garbage_tx, shared)?
        } else if cfg.host == HostChoice::Asio && crate::device::ASIO_AVAILABLE {
            let opened = crate::host_thread::run(crate::host_thread::SLOW, move |kept| {
                open_streams(&cfg, sink, cmd_rx, garbage_tx, shared).map(|(i, o, info)| (kept.keep(vec![i, o]), info))
            })
            .ok_or_else(|| EngineError::Thread("ASIO 드라이버가 응답하지 않아요".into()))??;
            (Owner::Asio(opened.0), opened.1)
        } else {
            let (ready_tx, ready_rx) = mpsc::channel();
            let (stop_tx, stop_rx) = mpsc::channel::<()>();
            let thread = thread::Builder::new()
                .name("gachi-audio-owner".into())
                .spawn(move || match open_streams(&cfg, sink, cmd_rx, garbage_tx, shared) {
                    Ok((input, output, info)) => {
                        let _ = ready_tx.send(Ok(info));
                        let _ = stop_rx.recv(); // park until Engine is dropped
                        drop(input);
                        drop(output);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                })
                .map_err(|e| EngineError::Thread(e.to_string()))?;
            let info = ready_rx.recv().map_err(|e| EngineError::Thread(e.to_string()))??;
            (Owner::Thread { stop_tx, thread: Some(thread) }, info)
        };
        // the first callbacks tell the real buffer size (the driver's own): wait for them
        // briefly, so what we report right after opening is not a guess
        // (DAW mode: the plugin may not be there yet, and it reports its block size itself)
        let t = Instant::now();
        while !daw && output_cb.count.load(Relaxed) <= WARMUP_CALLBACKS + 1 && t.elapsed() < std::time::Duration::from_millis(300) {
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut info = info;
        if info.buffer_size.is_none() {
            info.buffer_size = output_cb.range().map(|(_, max)| max);
        }
        tracing::info!(?info, "audio engine started");
        let driver_latency_now = std::sync::atomic::AtomicU64::new(pack_latency(info.driver_latency));
        Ok(Self {
            driver_latency_now,
            cmd_tx,
            garbage_rx,
            owner,
            info,
            self_params,
            self_meter,
            master_meter,
            input_meters,
            metronome,
            input_cb,
            output_cb,
            failed,
            started: Instant::now(),
            origin,
            recorder: None,
            rec_tap_back: false,
            feedback,
            fb_stop,
            fb_thread,
        })
    }

    /// Adds a peer to the mixer. Feed its decoded packets into the returned producer.
    pub fn add_peer(&mut self, id: PeerId) -> (rtrb::Producer<Frame>, PeerHandle) {
        self.collect_garbage();
        let (tx, rx) = rtrb::RingBuffer::new(PEER_RING);
        let handle = PeerHandle {
            params: Arc::new(PeerParams::default()),
            meter: Arc::new(Meter::default()),
            stats: Arc::new(SharedJitterStats::default()),
        };
        let slot = Box::new(PeerSlot { id, rx, jitter: JitterBuffer::new(3), handle: handle.clone(), gains: (f32::NAN, f32::NAN), channels: 2 });
        if self.cmd_tx.push(Cmd::AddPeer(slot)).is_err() {
            tracing::error!("engine command ring full; peer {id} not added");
        }
        (tx, handle)
    }

    pub fn remove_peer(&mut self, id: PeerId) {
        let _ = self.cmd_tx.push(Cmd::RemovePeer(id));
        self.collect_garbage();
    }

    /// Frees peer slots the audio thread handed back (never free on the audio thread).
    pub fn collect_garbage(&mut self) {
        while let Ok(g) = self.garbage_rx.pop() {
            if matches!(g, Garbage::Rec(_)) {
                self.rec_tap_back = true;
            }
            drop(g);
        }
    }

    /// Plays a 1 kHz tone on the output (not sent to anyone).
    pub fn play_test_tone(&self, secs: f32) {
        self.self_params.test_tone.store((secs.clamp(0.0, 10.0) * self.info.sample_rate as f32) as u32, Relaxed);
    }

    /// Metronome clicks on beat 0 at `start` (wall clock) and every beat after. Each peer
    /// converts the room's shared start time to its own wall clock before calling this.
    pub fn set_metronome(&self, on: bool, bpm: f32, start: SystemTime) {
        let m = &self.metronome;
        if on {
            // wall clock → our monotonic clock, via one (now, now) pair
            let now_mono = self.origin.elapsed().as_micros() as i64;
            let delta = match start.duration_since(SystemTime::now()) {
                Ok(ahead) => ahead.as_micros() as i64,
                Err(behind) => -(behind.duration().as_micros() as i64),
            };
            m.bpm.store(bpm);
            m.start_us.store(now_mono + delta);
        }
        m.on.store(on, Relaxed);
    }

    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }

    /// Starts writing one WAV per track (you + each peer, as heard here) into `dir`.
    /// `names` maps track ids ([`SELF_TRACK`] for you) to file names; peers that join later
    /// get a track that starts with silence, so every file lines up. `opts`: the files'
    /// format and names, and whether the mix is recorded too.
    pub fn start_recording(&mut self, dir: PathBuf, opts: RecOptions, names: Arc<Mutex<HashMap<PeerId, String>>>) -> Result<(), String> {
        if self.recorder.is_some() {
            return Err("이미 녹음 중이에요".into());
        }
        let (tap, rec) = Recorder::start(dir, opts, self.info.sample_rate, names, MAX_PEERS + 2)?;
        self.rec_tap_back = false;
        self.cmd_tx.push(Cmd::StartRec(Box::new(tap))).map_err(|_| "오디오 엔진이 바빠요. 다시 시도해 주세요.".to_string())?;
        self.recorder = Some(rec);
        Ok(())
    }

    /// The recording cannot be written (a full disk…): why. To be stopped by the caller;
    /// what was written until then is kept.
    pub fn recording_error(&self) -> Option<String> {
        self.recorder.as_ref().and_then(|r| r.error())
    }

    /// Stops recording and waits for the files to be finished.
    pub fn stop_recording(&mut self) -> Option<Result<RecordingResult, String>> {
        let rec = self.recorder.take()?;
        if self.cmd_tx.push(Cmd::StopRec).is_ok() {
            // the audio thread flushes and hands the tap back; dropping it ends the writer
            let deadline = Instant::now() + Duration::from_secs(2);
            while !self.rec_tap_back && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
                self.collect_garbage();
            }
        }
        if !self.rec_tap_back {
            // audio thread is not running (device lost): the tap goes away with the engine
            self.recorder = Some(rec);
            return Some(Err("오디오 장치가 멈춰서 녹음을 마무리하지 못했어요. 나갈 때 저장돼요.".into()));
        }
        Some(rec.finish())
    }

    /// Input + output buffer latency in ms (what this machine adds on its own).
    /// Input + output buffer in ms, from what the callbacks really get once they ran
    /// (WASAPI shared mode ignores the requested size and usually runs ~10 ms).
    pub fn local_latency_ms(&self) -> f32 {
        let (i, o) = self.device_latency();
        (i + o) as f32 / self.info.sample_rate as f32 * 1000.0
    }

    /// The device stopped working: a stream error, or (after a 2 s start) the callbacks
    /// stopped coming. `last` is the caller's memory of the callback counts.
    pub fn is_broken(&self, last: &mut (u32, u32)) -> bool {
        if self.failed.load(Relaxed) {
            return true;
        }
        if self.is_daw() {
            // no blocks just means no plugin (yet): shown as such, nothing to reopen
            return false;
        }
        let now = (self.input_cb.count.load(Relaxed), self.output_cb.count.load(Relaxed));
        let stalled = self.started.elapsed() > std::time::Duration::from_secs(2) && now.1 == last.1;
        *last = now;
        stalled
    }

    /// Sends other input channels from now on (no reopening of the device). DAW mode always
    /// sends the plugin's stereo as it comes.
    pub fn set_input(&self, input: InputChannels) {
        if !self.is_daw() {
            self.self_params.input_map.store(input.encode(), Relaxed);
        }
    }

    /// The others' sound is coming back in what we send (a DAW routing, a loopback input,
    /// speakers into the microphone): they hear themselves again. Checked once a second.
    pub fn feedback(&self) -> bool {
        self.feedback.load(Relaxed)
    }

    /// Running in DAW mode (no device of our own; see [`HostChoice::Daw`]).
    pub fn is_daw(&self) -> bool {
        #[cfg(windows)]
        if matches!(self.owner, Owner::Bridge { .. }) {
            return true;
        }
        false
    }

    /// DAW mode: another gachi plugin (a second track) is waiting for the bridge: only one
    /// plugin sends at a time, the other passes its sound through.
    pub fn plugin_waiting(&self) -> bool {
        #[cfg(windows)]
        if let Owner::Bridge { end, .. } = &self.owner {
            return end.plugin_waiting();
        }
        false
    }

    /// DAW mode: whether a gachi plugin is connected and processing (`None` otherwise).
    pub fn plugin_connected(&self) -> Option<bool> {
        #[cfg(windows)]
        if let Owner::Bridge { end, .. } = &self.owner {
            return Some(end.plugin_alive());
        }
        None
    }

    /// ASIO only: whether the driver now runs another sample rate or buffer size than our
    /// streams (changed in its control panel; drivers rarely restart the streams by
    /// themselves), or does not answer at all. Asks the driver: waits at most a second.
    pub fn driver_check(&self) -> DriverCheck {
        // DAW mode: follow the DAW's sample rate (the plugin reports it)
        #[cfg(windows)]
        if let Owner::Bridge { end, .. } = &self.owner {
            let (rate, _) = end.format();
            if rate != 0 && rate != self.info.sample_rate {
                tracing::info!(rate, now_rate = self.info.sample_rate, "DAW sample rate changed");
                return DriverCheck::Changed;
            }
            return DriverCheck::Same;
        }
        if !matches!(self.owner, Owner::Asio(_)) {
            return DriverCheck::Same;
        }
        // the driver's control panel is open: it may not answer until closed (no hang);
        // whatever was changed there is picked up once it is
        if crate::device::panel_open() {
            return DriverCheck::Same;
        }
        let Ok(now) = crate::device::asio_driver_now() else { return DriverCheck::NoAnswer };
        let Some(crate::device::DriverNow { rate, pref, latency }) = now else { return DriverCheck::Same };
        if let Some(l) = latency {
            let before = self.driver_latency_now.swap(pack_latency(Some(l)), Relaxed);
            if before != pack_latency(Some(l)) {
                tracing::info!(?l, before = ?unpack_latency(before), "ASIO driver latency now");
            }
        }
        let running = self.output_cb.range().map(|(_, max)| max);
        let rate_changed = rate != 0 && rate != self.info.sample_rate;
        let buffer_changed = pref != 0 && self.output_cb.count.load(Relaxed) > WARMUP_CALLBACKS && running != Some(pref);
        if rate_changed || buffer_changed {
            tracing::info!(rate, pref, ?running, now_rate = self.info.sample_rate, "ASIO driver settings changed");
            return DriverCheck::Changed;
        }
        DriverCheck::Same
    }

    /// (input, output) latency of this machine's audio in frames: what the ASIO driver
    /// reports now (never less than the callbacks), else the callback sizes.
    pub fn device_latency(&self) -> (u32, u32) {
        let (bi, bo) = self.real_buffers();
        match unpack_latency(self.driver_latency_now.load(Relaxed)) {
            Some((i, o)) => (i.max(bi), o.max(bo)),
            None => (bi, bo),
        }
    }

    /// (input, output) device latency in ms, and whether the driver reported it (else it is
    /// the callback sizes: DAW mode, drivers that do not tell).
    pub fn device_latency_ms(&self) -> (f32, f32, bool) {
        let (i, o) = self.device_latency();
        let ms = |n: u32| n as f32 / self.info.sample_rate.max(1) as f32 * 1000.0;
        (ms(i), ms(o), unpack_latency(self.driver_latency_now.load(Relaxed)).is_some())
    }

    /// Frames per network packet we send now.
    pub fn packet_frames(&self) -> u32 {
        self.input_cb.packet.load(Relaxed)
    }

    /// (input, output) frames per callback: measured when known, else what was asked for.
    pub fn real_buffers(&self) -> (u32, u32) {
        let asked = self.info.buffer_size.unwrap_or(480);
        let real = |c: &CallbackStats| c.range().map_or(asked, |(_, max)| max);
        (real(&self.input_cb), real(&self.output_cb))
    }
}

/// What the ASIO driver says when asked (see [`Engine::driver_check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverCheck {
    Same,
    /// another sample rate or buffer size: reopen to follow it
    Changed,
    /// no answer within a second: the driver hangs (reopening would hang too)
    NoAnswer,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.fb_stop.store(true, Relaxed);
        if let Some(t) = self.fb_thread.take() {
            let _ = t.join();
        }
        match &mut self.owner {
            Owner::Thread { stop_tx, thread } => {
                let _ = stop_tx.send(());
                if let Some(t) = thread.take() {
                    let _ = t.join();
                }
            }
            #[cfg(windows)]
            Owner::Bridge { stop, thread, .. } => {
                stop.store(true, Relaxed);
                if let Some(t) = thread.take() {
                    let _ = t.join();
                }
            }
            Owner::Asio(id) => {
                let id = *id;
                if crate::host_thread::run(crate::host_thread::SLOW, move |kept| kept.release(id)).is_none() {
                    // the driver hangs with our streams: the recording finishes whenever
                    // they are let go, not here (that would hang too)
                    if let Some(rec) = self.recorder.take() {
                        let _ = thread::Builder::new().name("gachi-rec-finish".into()).spawn(move || rec.finish());
                    }
                }
            }
        }
        // streams are gone, so the recording tap is dropped too: finish the files
        if let Some(rec) = self.recorder.take() {
            let _ = rec.finish();
        }
    }
}

/* ───────────────────────── stream setup ───────────────────────── */

/// The device's own format: its current sample rate (ASIO: set in the interface's control
/// panel; WASAPI: Windows sound settings) and its own buffer. We never ask for others:
/// many USB drivers restart the whole device when an app does, which also scrambles their
/// Windows playback/loopback routing for every other program.
fn pick_config(device: &cpal::Device, input: bool) -> Result<(cpal::StreamConfig, cpal::SampleFormat), EngineError> {
    let default = if input { device.default_input_config()? } else { device.default_output_config()? };
    let mut config = default.config();
    config.buffer_size = cpal::BufferSize::Default;
    Ok((config, default.sample_format()))
}

/// State shared between the engine handle and the callbacks.
struct Shared {
    self_params: Arc<SelfParams>,
    self_meter: Arc<Meter>,
    master_meter: Arc<Meter>,
    input_meters: Arc<ChannelMeters>,
    metronome: Arc<MetronomeParams>,
    input_cb: Arc<CallbackStats>,
    output_cb: Arc<CallbackStats>,
    failed: Arc<AtomicBool>,
    origin: Instant,
    /// 12 kHz mono taps for the feedback check: what we send, and the others' mix we play
    fb_sent: rtrb::Producer<f32>,
    fb_played: rtrb::Producer<f32>,
}

fn open_streams(
    cfg: &EngineConfig,
    sink: Box<dyn PacketSink>,
    cmd_rx: rtrb::Consumer<Cmd>,
    garbage_tx: rtrb::Producer<Garbage>,
    shared: Shared,
) -> Result<(cpal::Stream, cpal::Stream, StreamInfo), EngineError> {
    let Shared { self_params, self_meter, master_meter, input_meters, metronome, input_cb, output_cb, failed, origin, fb_sent, fb_played } = shared;
    let host = host(cfg.host);
    let in_dev = find_device(&host, cfg.input_device.as_deref(), true).ok_or(EngineError::NoDevice("input"))?;
    let out_dev = find_device(&host, cfg.output_device.as_deref(), false).ok_or(EngineError::NoDevice("output"))?;
    // ASIO: one driver serves both directions, and cpal keeps the driver's stream state per
    // `Device` value. Opening the output on a second `Device` for the same driver rebuilds
    // the driver buffers without the inputs: the input then delivers only zeros. Use the
    // same `Device` for both.
    let out_dev = if cfg.host == HostChoice::Asio && in_dev.name().ok() == out_dev.name().ok() { in_dev.clone() } else { out_dev };

    let (in_cfg, in_fmt) = pick_config(&in_dev, true)?;
    let (out_cfg, out_fmt) = pick_config(&out_dev, false)?;
    if in_cfg.sample_rate != out_cfg.sample_rate {
        // TODO: resample (rubato) instead of refusing
        return Err(EngineError::Unsupported(format!(
            "입력 장치는 {} Hz, 출력 장치는 {} Hz예요. Windows 소리 설정(장치 속성 → 고급)에서 두 장치를 같은 샘플레이트로 맞춰 주세요",
            in_cfg.sample_rate.0, out_cfg.sample_rate.0
        )));
    }

    let out_buffer = match out_cfg.buffer_size {
        cpal::BufferSize::Fixed(n) => n,
        cpal::BufferSize::Default => 512,
    };
    let (input, output) = processors(
        cfg,
        sink,
        cmd_rx,
        garbage_tx,
        Shared {
            self_params,
            self_meter,
            master_meter,
            input_meters,
            metronome,
            input_cb,
            output_cb,
            failed: failed.clone(),
            origin,
            fb_sent,
            fb_played,
        },
        (out_cfg.sample_rate.0, in_cfg.channels as usize, out_cfg.channels as usize, out_buffer),
        true,
    );

    let in_stream = build_input(&in_dev, &in_cfg, in_fmt, input, failed.clone())?;
    let out_stream = build_output(&out_dev, &out_cfg, out_fmt, output, failed)?;
    in_stream.play()?;
    out_stream.play()?;

    let info = StreamInfo {
        sample_rate: in_cfg.sample_rate.0,
        buffer_size: match in_cfg.buffer_size {
            cpal::BufferSize::Fixed(n) => Some(n),
            cpal::BufferSize::Default => None,
        },
        input_device: in_dev.name().unwrap_or_default(),
        output_device: out_dev.name().unwrap_or_default(),
        input_channels: in_cfg.channels,
        // (on the ASIO thread with the streams running: what the driver says now)
        driver_latency: if cfg.host == HostChoice::Asio { crate::device::asio_latencies() } else { None },
    };
    Ok((in_stream, out_stream, info))
}

/// The input and output processing, for a device or the DAW bridge. `format` = (sample
/// rate, input channels, output channels, output buffer frames). `local_monitor`: whether
/// software monitoring of your own input may be mixed in here (not in DAW mode: the plugin
/// passes your sound through itself, with no delay).
fn processors(
    cfg: &EngineConfig,
    sink: Box<dyn PacketSink>,
    cmd_rx: rtrb::Consumer<Cmd>,
    garbage_tx: rtrb::Producer<Garbage>,
    shared: Shared,
    format: (u32, usize, usize, u32),
    local_monitor: bool,
) -> (InputProcessor, OutputProcessor) {
    let Shared { self_params, self_meter, master_meter, input_meters, metronome, input_cb, output_cb, origin, fb_sent, fb_played, .. } = shared;
    let (sample_rate, in_channels, out_channels, out_buffer) = format;
    // your own sound, frame by frame (interleaved stereo), input → output: same device,
    // same clock, same buffer switch: no packets and no jitter buffer in between
    let (monitor_tx, monitor_rx) = rtrb::RingBuffer::new(MONITOR_RING);
    // automatic: what fits the buffer asked for, until the callbacks show the real block
    let auto = cfg.packet_frames == 0;
    let packet_frames = if auto { auto_packet_frames(cfg.buffer_size as usize) } else { (cfg.packet_frames as usize).clamp(8, MAX_FRAMES) };
    input_cb.packet.store(packet_frames as u32, Relaxed);
    let input = InputProcessor {
        channels: in_channels,
        map: cfg.input,
        map_code: cfg.input.encode(),
        gain: f32::NAN,
        glide: glide_step(sample_rate),
        packet_frames,
        auto,
        want: packet_frames,
        block: (0, 0),
        acc: Frame::EMPTY,
        acc_pos: 0,
        seq: 0,
        sink,
        monitor_tx,
        params: self_params.clone(),
        meter: self_meter,
        input_meters,
        scratch: vec![0.0; CHUNK * in_channels],
        cb: input_cb,
        fb: (crate::feedback::Decimator::new(sample_rate), fb_sent),
    };
    let output = OutputProcessor {
        channels: out_channels,
        cmd_rx,
        garbage_tx,
        peers: (0..MAX_PEERS).map(|_| None).collect(),
        monitor_rx,
        monitor_missing: 0,
        params: self_params,
        meter: master_meter,
        tone: ToneGen::new(sample_rate),
        metro: Metronome::new(sample_rate, out_buffer as f64 * 1e6 / sample_rate as f64, origin),
        metro_params: metronome,
        rec: None,
        limiter: crate::limiter::Limiter::new(sample_rate),
        rec_mix: vec![0.0; CHUNK * 2],
        rec_limiter: crate::limiter::Limiter::new(sample_rate),
        master: f32::NAN,
        glide: glide_step(sample_rate),
        mix: vec![0.0; CHUNK * 2],
        scratch: vec![0.0; CHUNK * 2],
        out_scratch: vec![0.0; CHUNK * out_channels],
        cb: output_cb,
        local_monitor,
        fb: (crate::feedback::Decimator::new(sample_rate), fb_played),
    };

    (input, output)
}

/// Logs a stream error and marks the engine as failed (the session reopens the device).
fn err_handler(failed: Arc<AtomicBool>) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e| {
        tracing::error!("audio stream error: {e}");
        failed.store(true, Relaxed);
    }
}

fn build_input(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    fmt: cpal::SampleFormat,
    p: InputProcessor,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, EngineError> {
    fn typed<T>(
        dev: &cpal::Device,
        cfg: &cpal::StreamConfig,
        mut p: InputProcessor,
        failed: Arc<AtomicBool>,
    ) -> Result<cpal::Stream, EngineError>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let (broke, mut dead) = (failed.clone(), false);
        Ok(dev.build_input_stream(
            cfg,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                if dead {
                    return;
                }
                // a panic here would unwind into the driver's C callback and take the whole
                // app down: stop this stream instead and let the session reopen the device
                let run = std::panic::AssertUnwindSafe(|| {
                    let t = p.cb.load.begin();
                    let ch = p.channels;
                    p.cb.record((data.len() / ch) as u32);
                    for chunk in data.chunks(CHUNK * ch) {
                        let mut scratch = std::mem::take(&mut p.scratch);
                        for (o, &s) in scratch.iter_mut().zip(chunk) {
                            *o = f32::from_sample(s);
                        }
                        p.process(&scratch[..chunk.len()]);
                        p.scratch = scratch;
                    }
                    p.cb.load.end(t);
                });
                if std::panic::catch_unwind(run).is_err() {
                    dead = true;
                    broke.store(true, Relaxed);
                }
            },
            err_handler(failed),
            None,
        )?)
    }
    match fmt {
        cpal::SampleFormat::F32 => typed::<f32>(dev, cfg, p, failed),
        cpal::SampleFormat::I32 => typed::<i32>(dev, cfg, p, failed),
        cpal::SampleFormat::I16 => typed::<i16>(dev, cfg, p, failed),
        other => Err(EngineError::Unsupported(format!("input sample format {other:?}"))),
    }
}

fn build_output(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    fmt: cpal::SampleFormat,
    p: OutputProcessor,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, EngineError> {
    fn typed<T>(
        dev: &cpal::Device,
        cfg: &cpal::StreamConfig,
        mut p: OutputProcessor,
        failed: Arc<AtomicBool>,
    ) -> Result<cpal::Stream, EngineError>
    where
        T: SizedSample + FromSample<f32>,
    {
        let (broke, mut dead) = (failed.clone(), false);
        Ok(dev.build_output_stream(
            cfg,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                if dead {
                    data.fill(T::EQUILIBRIUM);
                    return;
                }
                // (see the input callback: a panic must not reach the driver)
                let run = std::panic::AssertUnwindSafe(|| {
                    let t = p.cb.load.begin();
                    let ch = p.channels;
                    p.cb.record((data.len() / ch) as u32);
                    for chunk in data.chunks_mut(CHUNK * ch) {
                        let mut out = std::mem::take(&mut p.out_scratch);
                        let n = chunk.len();
                        p.process(&mut out[..n]);
                        for (o, &s) in chunk.iter_mut().zip(&out[..n]) {
                            *o = T::from_sample(s);
                        }
                        p.out_scratch = out;
                    }
                    p.cb.load.end(t);
                });
                if std::panic::catch_unwind(run).is_err() {
                    dead = true;
                    broke.store(true, Relaxed);
                    data.fill(T::EQUILIBRIUM);
                }
            },
            err_handler(failed),
            None,
        )?)
    }
    match fmt {
        cpal::SampleFormat::F32 => typed::<f32>(dev, cfg, p, failed),
        cpal::SampleFormat::I32 => typed::<i32>(dev, cfg, p, failed),
        cpal::SampleFormat::I16 => typed::<i16>(dev, cfg, p, failed),
        other => Err(EngineError::Unsupported(format!("output sample format {other:?}"))),
    }
}

/* ───────────────────────── feedback check ───────────────────────── */

/// Each 12 kHz tap's ring: 2 s, enough between two checks.
const FB_RING: usize = 24_000;

/// Once a second: does what we send contain the others' mix we played? Off the audio
/// threads (a check costs a few ms of one core).
fn feedback_check(mut sent_rx: rtrb::Consumer<f32>, mut played_rx: rtrb::Consumer<f32>, flag: &AtomicBool, stop: &AtomicBool) {
    use crate::feedback::{best_match, Verdict, HISTORY};
    let mut sent: std::collections::VecDeque<f32> = std::collections::VecDeque::with_capacity(HISTORY * 2);
    let mut played = sent.clone();
    let mut verdict = Verdict::default();
    let mut ticks = 0u32;
    while !stop.load(Relaxed) {
        thread::sleep(Duration::from_millis(100));
        // drain both at the same moment, so their ends line up (within a callback)
        while let Ok(v) = sent_rx.pop() {
            sent.push_back(v);
        }
        while let Ok(v) = played_rx.pop() {
            played.push_back(v);
        }
        for q in [&mut sent, &mut played] {
            while q.len() > HISTORY {
                q.pop_front();
            }
        }
        ticks += 1;
        if !ticks.is_multiple_of(10) || sent.len() < HISTORY || played.len() < HISTORY {
            continue;
        }
        let m = best_match(sent.make_contiguous(), played.make_contiguous());
        let was = flag.load(Relaxed);
        let now = verdict.update(m);
        if now != was {
            tracing::warn!(on = now, ?m, "feedback: the others' sound {} what we send", if now { "comes back in" } else { "no longer in" });
        }
        flag.store(now, Relaxed);
    }
}

/* ───────────────────────── DAW mode ───────────────────────── */

/// Frames waiting from the plugin beyond which we skip to the newest (back from a stall:
/// send what the player plays now, not 85 ms of backlog).
#[cfg(windows)]
const STALE_BACKLOG: usize = 4096;

#[cfg(not(windows))]
fn start_bridge(
    _: &EngineConfig,
    _: Box<dyn PacketSink>,
    _: rtrb::Consumer<Cmd>,
    _: rtrb::Producer<Garbage>,
    _: Shared,
) -> Result<(Owner, StreamInfo), EngineError> {
    Err(EngineError::Unsupported("DAW 연결 모드는 Windows에서만 돼요".into()))
}

/// DAW mode: opens the bridge and runs the same input/output processing on our own thread,
/// once per block the plugin hands over. Format: the DAW's, as the plugin reported it (48 kHz
/// / 64 until a plugin connects; a different rate then reopens the engine, like a driver
/// change would).
#[cfg(windows)]
fn start_bridge(
    cfg: &EngineConfig,
    sink: Box<dyn PacketSink>,
    cmd_rx: rtrb::Consumer<Cmd>,
    garbage_tx: rtrb::Producer<Garbage>,
    shared: Shared,
) -> Result<(Owner, StreamInfo), EngineError> {
    let end = Arc::new(gachi_bridge::AppEnd::create(&gachi_bridge::name()).map_err(|e| EngineError::Unsupported(e.to_string()))?);
    let (rate, block) = end.format();
    let sample_rate = if rate == 0 { 48_000 } else { rate };
    let block = if block == 0 { 64 } else { block };
    let params = shared.self_params.clone();
    // the plugin hands over stereo: sent as it comes (no channel choice in DAW mode)
    let stereo = InputChannels::Stereo(0, 1);
    params.input_map.store(stereo.encode(), Relaxed);
    let cfg = EngineConfig { input: stereo, ..cfg.clone() };
    // (a block is at most `CHUNK` frames and a packet at least 8: every packet of a block fits)
    let (hold_tx, hold_rx) = rtrb::RingBuffer::<Frame>::new(CHUNK / 8);
    let (input, output) = processors(&cfg, Box::new(Held(hold_tx)), cmd_rx, garbage_tx, shared, (sample_rate, 2, 2, block), false);
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (end, stop) = (end.clone(), stop.clone());
        thread::Builder::new()
            .name("gachi-bridge".into())
            .spawn(move || run_bridge(&end, input, output, &stop, &params, hold_rx, sink))
            .map_err(|e| EngineError::Thread(e.to_string()))?
    };
    let name = "DAW (gachi 플러그인)".to_string();
    let info = StreamInfo {
        sample_rate,
        buffer_size: Some(block),
        input_device: name.clone(),
        output_device: name,
        input_channels: 2,
        // the DAW's own driver latency is the DAW's to know: blocks only
        driver_latency: None,
    };
    Ok((Owner::Bridge { stop, thread: Some(thread), end }, info))
}

/// The bridge thread: wait for a block from the plugin, send it (input processing), mix the
/// same number of frames (output processing) and hand the mix straight back, so the plugin
/// can play it in the same DAW block.
#[cfg(windows)]
fn run_bridge(
    end: &gachi_bridge::AppEnd,
    mut input: InputProcessor,
    mut output: OutputProcessor,
    stop: &AtomicBool,
    params: &SelfParams,
    mut held: rtrb::Consumer<Frame>,
    mut sink: Box<dyn PacketSink>,
) {
    if !gachi_bridge::pro_audio() {
        tracing::info!("MMCSS unavailable for the bridge thread");
    }
    let mut up = vec![0f32; CHUNK * 2];
    let mut down = vec![0f32; CHUNK * 2];
    while !stop.load(Relaxed) {
        // (muted, or your own sound switched off in the app: the plugin plays none of it)
        let own = !params.muted.load(Relaxed) && params.own_on.load(Relaxed);
        end.set_own_gain(if own { params.monitor_gain.load() } else { 0.0 });
        if !end.wait(50) && end.up_available() == 0 {
            continue;
        }
        if end.up_available() > STALE_BACKLOG {
            end.skip_up_to_latest(CHUNK);
        }
        loop {
            let n = end.read_up(&mut up);
            if n == 0 {
                break;
            }
            let t = input.cb.load.begin();
            input.cb.record(n as u32);
            input.process(&up[..n * 2]);
            let took_in = crate::load::CallbackLoad::now().saturating_sub(t);
            let t = output.cb.load.begin();
            output.cb.record(n as u32);
            output.process(&mut down[..n * 2]);
            output.cb.load.end(t);
            end.write_down(&down[..n * 2]);
            // the mix is on its way: now the block's packets (their time counts as input)
            let t = crate::load::CallbackLoad::now();
            while let Ok(f) = held.pop() {
                sink.send(&f);
            }
            input.cb.load.record(took_in + crate::load::CallbackLoad::now().saturating_sub(t));
        }
    }
}

/// The input processing's sink in DAW mode: the block's packets wait here until the mix has
/// gone back to the plugin, then `run_bridge` sends them. The plugin waits only a moment for
/// the mix (a quarter of the block, 167 µs at 32 frames), and sending to a full room of 15
/// takes 80-100 µs, twice that with copies for lossy links (measured: gachi-net's
/// `send_cost`): sent first, as a device's input callback does, the mix would miss that
/// wait. The packets leave in the same block, some tens of µs later.
#[cfg(windows)]
struct Held(rtrb::Producer<Frame>);

#[cfg(windows)]
impl PacketSink for Held {
    fn send(&mut self, frame: &Frame) {
        let _ = self.0.push(*frame);
    }
}

/* ───────────────────────── input callback ───────────────────────── */

struct InputProcessor {
    channels: usize,
    map: InputChannels,
    map_code: u64,
    /// the input gain in use, and the per-sample step it moves by (see `glide`)
    gain: f32,
    glide: f32,
    packet_frames: usize,
    /// the packet size follows the block
    auto: bool,
    /// the packet size to go on with, from the next packet that starts
    want: usize,
    /// (frames, how many in a row) of the blocks as they come
    block: (usize, u32),
    acc: Frame,
    acc_pos: usize,
    seq: u32,
    sink: Box<dyn PacketSink>,
    monitor_tx: rtrb::Producer<f32>,
    params: Arc<SelfParams>,
    meter: Arc<Meter>,
    input_meters: Arc<ChannelMeters>,
    scratch: Vec<f32>,
    cb: Arc<CallbackStats>,
    /// tap for the feedback check (what we send, 12 kHz mono)
    fb: (crate::feedback::Decimator, rtrb::Producer<f32>),
}

/// Blocks of one size in a row before the automatic packet size goes by it.
const AUTO_STEADY_BLOCKS: u32 = 4;

impl InputProcessor {
    /// At the start of a block: go on with the packet size wanted. What is left of the
    /// last packet goes out short, once, so that from here on packets end where blocks end
    /// (carried over, that rest would ride along for good: every packet that much late).
    #[inline]
    fn follow(&mut self, out_ch: usize) {
        if self.packet_frames == self.want {
            return;
        }
        if self.acc_pos > 0 {
            self.acc.seq = self.seq;
            self.acc.frames = self.acc_pos as u16;
            self.acc.channels = out_ch as u8;
            self.sink.send(&self.acc);
            self.seq = self.seq.wrapping_add(1);
            self.acc_pos = 0;
        }
        self.packet_frames = self.want;
        self.cb.packet.store(self.want as u32, Relaxed);
    }

    fn process(&mut self, data: &[f32]) {
        // a new channel choice takes effect at once, without reopening the device
        let code = self.params.input_map.load(Relaxed);
        if code != self.map_code {
            if let Some(m) = InputChannels::decode(code) {
                self.map = m;
            }
            self.map_code = code;
        }
        let target = if self.params.muted.load(Relaxed) { 0.0 } else { self.params.input_gain.load() };
        let steady = settled(&mut self.gain, target);
        let ch = self.channels;
        if self.auto {
            // the block the device (or the DAW) really hands over: the buffer asked for is
            // not always the one given, and a DAW changes it as it likes. One that holds
            // for a few blocks counts; a host that varies it block by block keeps what it had
            let n = data.len() / ch;
            self.block = if n == self.block.0 { (n, self.block.1.saturating_add(1)) } else { (n, 1) };
            if self.block.1 == AUTO_STEADY_BLOCKS {
                self.want = auto_packet_frames(n).min(MAX_FRAMES);
            }
        }
        let at = |i: usize| i.min(ch - 1);
        // (channels sent, left source, right source, centered mono source, its gain)
        let (out_ch, a, b, m, mg) = match self.map {
            InputChannels::Mono(i) => (1, at(i), at(i), 0, 0.0),
            InputChannels::Stereo(i, j) => (2, at(i), at(j), 0, 0.0),
            // mic panned center at equal power, pair at unity on top
            InputChannels::Mix { mono, pair: (i, j) } => (2, at(i), at(j), at(mono), std::f32::consts::FRAC_1_SQRT_2),
        };
        self.follow(out_ch);
        let (mut peak_l, mut peak_r) = (0f32, 0f32);
        let metered = ch.min(MAX_INPUT_METERS);
        let mut ch_peaks = [0f32; MAX_INPUT_METERS];

        for frame in data.chunks_exact(ch) {
            for (p, s) in ch_peaks[..metered].iter_mut().zip(frame) {
                *p = p.max(s.abs());
            }
            let center = frame[m] * mg;
            let gain = if steady { target } else { glide(&mut self.gain, target, self.glide) };
            let l = (frame[a] + center) * gain;
            let r = (frame[b] + center) * gain;
            peak_l = peak_l.max(l.abs());
            peak_r = peak_r.max(r.abs());
            if let Some(v) = self.fb.0.push(l, r) {
                let _ = self.fb.1.push(v);
            }
            // to the output side for monitoring and recording, as it comes
            let _ = self.monitor_tx.push(l);
            let _ = self.monitor_tx.push(r);
            if out_ch == 1 {
                self.acc.data[self.acc_pos] = l;
            } else {
                self.acc.data[self.acc_pos * 2] = l;
                self.acc.data[self.acc_pos * 2 + 1] = r;
            }
            self.acc_pos += 1;
            if self.acc_pos == self.packet_frames {
                self.acc.seq = self.seq;
                self.acc.frames = self.packet_frames as u16;
                self.acc.channels = out_ch as u8;
                self.sink.send(&self.acc);
                self.seq = self.seq.wrapping_add(1);
                self.acc_pos = 0;
            }
        }
        self.meter.update_stereo(&[peak_l, peak_r]);
        for (c, &p) in ch_peaks[..metered].iter().enumerate() {
            self.input_meters.update(c, p);
        }
    }
}

/* ───────────────────────── output callback ───────────────────────── */

struct OutputProcessor {
    channels: usize,
    cmd_rx: rtrb::Consumer<Cmd>,
    garbage_tx: rtrb::Producer<Garbage>,
    peers: Vec<Option<Box<PeerSlot>>>,
    monitor_rx: rtrb::Consumer<f32>,
    /// own-sound frames the output wanted and the input had not given (startup; tests)
    monitor_missing: u64,
    params: Arc<SelfParams>,
    meter: Arc<Meter>,
    tone: ToneGen,
    metro: Metronome,
    metro_params: Arc<MetronomeParams>,
    rec: Option<Box<RecTap>>,
    limiter: crate::limiter::Limiter,
    /// the recording's mix (see `MIX_TRACK`), and the limiter that keeps it within full scale
    rec_mix: Vec<f32>,
    rec_limiter: crate::limiter::Limiter,
    /// the master gain in use, and the per-sample step gains move by (see `glide`)
    master: f32,
    glide: f32,
    mix: Vec<f32>,
    scratch: Vec<f32>,
    out_scratch: Vec<f32>,
    cb: Arc<CallbackStats>,
    /// software monitoring may be mixed in (not in DAW mode)
    local_monitor: bool,
    /// tap for the feedback check (the others' mix we play, 12 kHz mono)
    fb: (crate::feedback::Decimator, rtrb::Producer<f32>),
}

impl OutputProcessor {
    fn apply_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.pop() {
            match cmd {
                Cmd::AddPeer(slot) => match self.peers.iter_mut().find(|p| p.is_none()) {
                    Some(free) => *free = Some(slot),
                    None => {
                        let _ = self.garbage_tx.push(Garbage::Peer(slot));
                    }
                },
                Cmd::RemovePeer(id) => {
                    for p in self.peers.iter_mut() {
                        if p.as_ref().is_some_and(|s| s.id == id) {
                            if let Some(slot) = p.take() {
                                let _ = self.garbage_tx.push(Garbage::Peer(slot));
                            }
                        }
                    }
                }
                Cmd::StartRec(tap) => {
                    if let Some(old) = self.rec.replace(tap) {
                        let _ = self.garbage_tx.push(Garbage::Rec(old));
                    }
                }
                Cmd::StopRec => {
                    if let Some(mut tap) = self.rec.take() {
                        tap.flush_all();
                        let _ = self.garbage_tx.push(Garbage::Rec(tap));
                    }
                }
            }
        }
    }

    /// `out` is interleaved with `self.channels` channels, at most CHUNK frames.
    /// Your own sound for this block (interleaved stereo, `dst.len()` samples): what the
    /// input callback handed over in this buffer switch. Input runs first, so this is the
    /// block that just came in (nothing added); if the output ran first it is the previous
    /// one (one block). If more than a block extra piled up (the output ran without the input
    /// for a while), the newest block is kept.
    /// (A free function of the fields it needs: the caller holds other buffers of `self`.)
    fn take_own_sound(rx: &mut rtrb::Consumer<f32>, missing: &mut u64, dst: &mut [f32]) {
        let need = dst.len();
        let avail = rx.slots();
        if avail > 2 * need {
            if let Ok(c) = rx.read_chunk(avail - need) {
                c.commit_all();
            }
        }
        let n = rx.slots().min(need);
        if let Ok(c) = rx.read_chunk(n) {
            let (a, b) = c.as_slices();
            dst[..a.len()].copy_from_slice(a);
            dst[a.len()..n].copy_from_slice(b);
            c.commit_all();
        }
        dst[n..].fill(0.0);
        *missing += ((need - n) / 2) as u64;
    }

    fn process(&mut self, out: &mut [f32]) {
        self.apply_commands();
        let frames = out.len() / self.channels;
        let mix = &mut self.mix[..frames * 2];
        let scratch = &mut self.scratch[..frames * 2];
        mix.fill(0.0);

        let step = self.glide;
        let any_solo = self.peers.iter().flatten().any(|p| p.handle.params.solo.load(Relaxed));
        for (slot, p) in self.peers.iter_mut().enumerate() {
            let Some(p) = p else { continue };
            while let Ok(f) = p.rx.pop() {
                p.channels = f.channels;
                p.jitter.push(f);
            }
            let params = &p.handle.params;
            p.jitter.set_target_f(params.jitter_target.load(Relaxed) as f64 / 100.0);
            p.jitter.set_base_ratio(params.rate_ratio.load() as f64);
            p.jitter.read_stereo(scratch);
            p.handle.meter.update_stereo(scratch);
            // (only the tracks picked on the cards. One switched on while recording starts
            // there: the writer fills what came before with silence, as for a late joiner)
            if let (Some(rec), true) = (self.rec.as_mut(), params.rec.load(Relaxed)) {
                rec.write(slot + 1, p.id, scratch, p.channels); // pre-fader, as received
            }

            let st = p.jitter.stats();
            let s = &p.handle.stats;
            s.depth.store(st.depth, Relaxed);
            s.target.store(st.target, Relaxed);
            s.underruns.store(st.underruns, Relaxed);
            s.concealed.store(st.concealed, Relaxed);
            s.late.store(st.late, Relaxed);
            s.drift_ppm.store(st.drift_ppm, Relaxed);

            // constant-power pan, normalized so center = unity; muted (or not the solo) = 0
            let off = params.mute.load(Relaxed) || (any_solo && !params.solo.load(Relaxed));
            let gain = if off { 0.0 } else { params.gain.load() };
            let angle = (params.pan.load().clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
            let gl = gain * angle.cos() * std::f32::consts::SQRT_2;
            let gr = gain * angle.sin() * std::f32::consts::SQRT_2;
            let (cl, cr) = (&mut p.gains.0, &mut p.gains.1);
            let frames_in = mix.as_chunks_mut::<2>().0.iter_mut().zip(scratch.as_chunks::<2>().0);
            if settled(cl, gl) & settled(cr, gr) {
                if gl == 0.0 && gr == 0.0 {
                    continue;
                }
                for (m, s) in frames_in {
                    m[0] += s[0] * gl;
                    m[1] += s[1] * gr;
                }
            } else {
                for (m, s) in frames_in {
                    m[0] += s[0] * glide(cl, gl, step);
                    m[1] += s[1] * glide(cr, gr, step);
                }
            }
        }

        // the others' mix as it leaves (before our own sound, master and metronome): what must
        // never come back in what we send
        for m in mix.as_chunks::<2>().0 {
            if let Some(v) = self.fb.0.push(m[0], m[1]) {
                let _ = self.fb.1.push(v);
            }
        }

        // your own input: always read (keeps it current and feeds the recording),
        // mixed in only if software monitoring is on
        Self::take_own_sound(&mut self.monitor_rx, &mut self.monitor_missing, scratch);
        if let Some(rec) = self.rec.as_mut() {
            if self.params.rec.load(Relaxed) {
                let mono = InputChannels::decode(self.params.input_map.load(Relaxed)).is_some_and(|m| m.is_mono());
                rec.write(0, SELF_TRACK, scratch, if mono { 1 } else { 2 });
            }
            if rec.mix {
                // everyone together, as heard here: the others at their faders, our own
                // sound (whether or not we monitor it), the master fader; no metronome, no
                // test tone. Kept within full scale by a limiter of its own (the files may
                // be 16 bit)
                let master = self.params.master.load();
                let out = &mut self.rec_mix[..frames * 2];
                for ((o, m), s) in out.iter_mut().zip(mix.iter()).zip(scratch.iter()) {
                    *o = (*m + *s) * master;
                }
                self.rec_limiter.process(out);
                rec.write(MAX_PEERS + 1, MIX_TRACK, out, 2);
            }
            rec.pos += frames as u64;
        }
        if self.local_monitor && self.params.monitor.load(Relaxed) {
            for (m, s) in mix.iter_mut().zip(scratch.iter()) {
                *m += *s;
            }
        }

        let master = self.params.master.load();
        if settled(&mut self.master, master) {
            mix.iter_mut().for_each(|s| *s *= master);
        } else {
            for m in mix.as_chunks_mut::<2>().0 {
                let g = glide(&mut self.master, master, step);
                m[0] *= g;
                m[1] *= g;
            }
        }
        self.meter.update_stereo(mix);
        // local-only sounds: not recorded, not sent, not affected by the master fader
        self.tone.add(&self.params.test_tone, mix);
        self.metro.add(&self.metro_params, mix);
        // never past full scale: the driver would hard-clip (no delay added)
        self.limiter.process(mix);

        let ch = self.channels;
        for (o, m) in out.chunks_exact_mut(ch).zip(mix.as_chunks::<2>().0) {
            if ch == 1 {
                o[0] = (m[0] + m[1]) * 0.5;
            } else {
                o[0] = m[0];
                o[1] = m[1];
                o[2..].fill(0.0);
            }
        }
    }
}

#[cfg(test)]
mod input_code_tests {
    use super::InputChannels;

    #[test]
    fn input_routing_survives_the_atomic() {
        for m in [InputChannels::Mono(3), InputChannels::Stereo(4, 5), InputChannels::Mix { mono: 0, pair: (8, 9) }] {
            assert_eq!(InputChannels::decode(m.encode()), Some(m));
        }
        assert_eq!(InputChannels::decode(0), None);
    }
}

#[cfg(all(test, windows))]
mod daw_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// The bridge name travels in an environment variable (process-wide): one test at a time.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    struct Count(Arc<AtomicUsize>);
    impl PacketSink for Count {
        fn send(&mut self, _: &Frame) {
            self.0.fetch_add(1, Relaxed);
        }
    }

    /// The whole DAW-mode path in one process: a "plugin" hands blocks over the bridge, the
    /// engine sends them as packets and hands back the mix of a peer.
    #[test]
    fn daw_mode_sends_the_plugins_sound_and_returns_the_mix() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("GACHI_BRIDGE_NAME", format!("gachi-engine-test-{}", std::process::id()));
        let sent = Arc::new(AtomicUsize::new(0));
        let cfg = EngineConfig {
            host: HostChoice::Daw,
            input_device: None,
            output_device: None,
            sample_rate: 48_000,
            buffer_size: 64,
            packet_frames: 32,
            input: InputChannels::Mono(0),
        };
        let mut engine = Engine::start(cfg, Box::new(Count(sent.clone()))).expect("DAW mode starts without a plugin");
        assert!(engine.is_daw());
        assert_eq!(engine.plugin_connected(), Some(false));
        assert!(!engine.is_broken(&mut (0, 0)), "no plugin yet is not a broken device");

        let mut plugin = gachi_bridge::PluginEnd::open(&gachi_bridge::name(), 1).unwrap();
        plugin.set_format(48_000, 64);
        let (mut peer_tx, peer) = engine.add_peer(2);
        peer.params.jitter_target.store(300, Relaxed); // 3 packets
        engine.self_params.monitor_gain.store(0.5);

        let up = [0.5f32; 64 * 2];
        let mut down = [0f32; 64 * 2];
        let (mut answered, mut heard) = (0, 0f32);
        for b in 0..400u32 {
            // the peer sends 64 frames per block too (two 32-frame packets)
            for k in 0..2 {
                let mut f = Frame { seq: b * 2 + k, channels: 1, frames: 32, ..Frame::EMPTY };
                f.data[..32].fill(0.25);
                let _ = peer_tx.push(f);
            }
            plugin.write_up(&up);
            let t = Instant::now();
            while plugin.down_available() < 64 && t.elapsed() < Duration::from_millis(20) {
                std::hint::spin_loop();
            }
            if plugin.read_down(&mut down) == 64 {
                answered += 1;
                heard = heard.max(down[0].abs());
            }
            thread::sleep(Duration::from_micros(1300));
        }
        assert!(answered >= 395, "mix came back for {answered} of 400 blocks");
        assert!(sent.load(Relaxed) >= 790, "{} packets sent", sent.load(Relaxed));
        assert!((heard - 0.25).abs() < 0.01, "the peer is in the mix ({heard})");
        assert_eq!(engine.plugin_connected(), Some(true));
        assert_eq!(plugin.own_gain(), 0.5, "the app's own-sound level reached the plugin");
        // the app's monitor button, off: the plugin plays none of your own sound
        engine.self_params.own_on.store(false, Relaxed);
        for _ in 0..20 {
            plugin.write_up(&up);
            thread::sleep(Duration::from_micros(1300));
            plugin.read_down(&mut down);
        }
        assert_eq!(plugin.own_gain(), 0.0, "own sound switched off in the app");
        engine.self_params.own_on.store(true, Relaxed);
        assert_eq!(engine.real_buffers(), (64, 64), "the DAW's block size");
        assert_eq!(engine.driver_check(), DriverCheck::Same);
        plugin.set_format(44_100, 64);
        assert_eq!(engine.driver_check(), DriverCheck::Changed, "follows the DAW's sample rate");
    }

    /// Noise the way a player is noise to the check: different seeds never match.
    fn noise(seed: &mut u64) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        ((*seed >> 11) as f32 / (1u64 << 53) as f32 - 0.5) * 0.4
    }

    /// Runs DAW mode for `secs` with a peer playing noise and the plugin sending its own
    /// noise, plus (if `loop_back`) the mix it got back, as a DAW routing gachi's output
    /// into its input would. Returns whether the engine flagged feedback.
    fn run_loop(name: &str, loop_back: bool, secs: f64) -> bool {
        std::env::set_var("GACHI_BRIDGE_NAME", name);
        let cfg = EngineConfig {
            host: HostChoice::Daw,
            input_device: None,
            output_device: None,
            sample_rate: 48_000,
            buffer_size: 64,
            packet_frames: 32,
            input: InputChannels::Stereo(0, 1),
        };
        let mut engine = Engine::start(cfg, Box::new(Count(Arc::new(AtomicUsize::new(0))))).unwrap();
        let mut plugin = gachi_bridge::PluginEnd::open(&gachi_bridge::name(), 1).unwrap();
        plugin.set_format(48_000, 64);
        let (mut peer_tx, peer) = engine.add_peer(2);
        peer.params.jitter_target.store(300, Relaxed);
        let (mut peer_seed, mut own_seed) = (0x1234_5678_9abc_def1u64, 0x0fed_cba9_8765_4321u64);
        let mut up = [0f32; 64 * 2];
        let mut down = [0f32; 64 * 2];
        let blocks = (secs * 48_000.0 / 64.0) as u32;
        for b in 0..blocks {
            for k in 0..2 {
                let mut f = Frame { seq: b * 2 + k, channels: 1, frames: 32, ..Frame::EMPTY };
                for s in &mut f.data[..32] {
                    *s = noise(&mut peer_seed);
                }
                let _ = peer_tx.push(f);
            }
            for i in 0..64 {
                let own = noise(&mut own_seed) * 0.7;
                // the loop: what came back last block goes out again
                let back = if loop_back { down[i * 2] * 0.5 } else { 0.0 };
                up[i * 2] = own + back;
                up[i * 2 + 1] = own + back;
            }
            plugin.write_up(&up);
            let t = Instant::now();
            while plugin.down_available() < 64 && t.elapsed() < Duration::from_millis(20) {
                std::hint::spin_loop();
            }
            if plugin.read_down(&mut down) < 64 {
                down.fill(0.0);
            }
            thread::sleep(Duration::from_micros(1300));
        }
        engine.feedback()
    }

    #[test]
    fn the_others_routed_back_into_the_plugin_are_noticed() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let base = format!("gachi-engine-fb-{}", std::process::id());
        assert!(!run_loop(&format!("{base}-clean"), false, 4.0), "no loop: no warning");
        assert!(run_loop(&format!("{base}-loop"), true, 4.0), "the others' mix came back: warned");
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::*;

    struct Nowhere;
    impl PacketSink for Nowhere {
        fn send(&mut self, _: &Frame) {}
    }

    fn shared() -> Shared {
        Shared {
            self_params: Arc::new(SelfParams::default()),
            self_meter: Arc::new(Meter::default()),
            master_meter: Arc::new(Meter::default()),
            input_meters: Arc::new(ChannelMeters::default()),
            metronome: Arc::new(MetronomeParams::default()),
            input_cb: Arc::new(CallbackStats::default()),
            output_cb: Arc::new(CallbackStats::default()),
            failed: Arc::new(AtomicBool::new(false)),
            origin: Instant::now(),
            fb_sent: rtrb::RingBuffer::new(4096).0,
            fb_played: rtrb::RingBuffer::new(4096).0,
        }
    }

    #[test]
    fn automatic_packet_size_fits_the_block() {
        for (block, packet) in [(16, 16), (32, 32), (48, 48), (64, 64), (96, 48), (128, 64), (144, 48), (256, 64), (480, 60), (441, 63), (1024, 64)] {
            assert_eq!(auto_packet_frames(block), packet, "block {block}");
        }
        // nothing fits: the biggest; tiny blocks: several to a packet
        assert_eq!(auto_packet_frames(127), 64);
        assert_eq!(auto_packet_frames(4), 8);
        assert_eq!(auto_packet_frames(0), 64);
    }

    /// The sizes of the packets sent, block by block.
    struct Sizes(Arc<std::sync::Mutex<Vec<u16>>>);
    impl PacketSink for Sizes {
        fn send(&mut self, f: &Frame) {
            self.0.lock().unwrap().push(f.frames);
        }
    }

    /// Automatic: every block leaves whole, in equal packets, whatever the device's block
    /// is (not the buffer that was asked for), and a block size that changes is followed.
    #[test]
    fn automatic_packets_follow_the_real_block() {
        let cfg = EngineConfig {
            host: HostChoice::Asio,
            input_device: None,
            output_device: None,
            sample_rate: 48_000,
            buffer_size: 64, // asked for
            packet_frames: 0,
            input: InputChannels::Stereo(0, 1),
        };
        let sh = shared();
        let stats = sh.input_cb.clone();
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (garbage_tx, _garbage_rx) = rtrb::RingBuffer::new(8);
        let (mut input, _output) = processors(&cfg, Box::new(Sizes(sent.clone())), cmd_rx, garbage_tx, sh, (48_000, 2, 2, 64), true);
        assert_eq!(stats.packet.load(Relaxed), 64, "until the device shows its block: what fits the buffer asked for");
        let mut run = |block: usize, blocks: usize| -> Vec<Vec<u16>> {
            let data = vec![0.1f32; block * 2];
            (0..blocks)
                .map(|_| {
                    input.process(&data);
                    std::mem::take(&mut *sent.lock().unwrap())
                })
                .collect()
        };
        // the device gives 96 (not the 64 asked for)
        let got = run(96, 40);
        assert!(got[10..].iter().all(|b| b == &[48, 48]), "96 → two of 48 per block: {:?}", &got[..12]);
        assert_eq!(stats.packet.load(Relaxed), 48);
        // nothing was lost over the change: every frame handed in was sent
        assert_eq!(got.iter().flatten().map(|&n| n as usize).sum::<usize>(), 96 * 40);
        // the block becomes 144, then 32
        let got = run(144, 40);
        assert!(got[10..].iter().all(|b| b == &[48, 48, 48]), "{:?}", &got[..12]);
        let got = run(32, 40);
        assert!(got[10..].iter().all(|b| b == &[32]), "{:?}", &got[..12]);
        assert_eq!(stats.packet.load(Relaxed), 32);
        // a host that hands over blocks of all sizes: the packet size stays where it was
        for k in 0..60 {
            run(20 + (k * 7) % 40, 1);
        }
        assert_eq!(stats.packet.load(Relaxed), 32);
    }

    /// A chosen packet size is kept whatever the block.
    #[test]
    fn a_chosen_packet_size_is_kept() {
        let cfg = EngineConfig {
            host: HostChoice::Asio,
            input_device: None,
            output_device: None,
            sample_rate: 48_000,
            buffer_size: 96,
            packet_frames: 64,
            input: InputChannels::Stereo(0, 1),
        };
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (garbage_tx, _garbage_rx) = rtrb::RingBuffer::new(8);
        let (mut input, _output) = processors(&cfg, Box::new(Sizes(sent.clone())), cmd_rx, garbage_tx, shared(), (48_000, 2, 2, 96), true);
        let data = vec![0.1f32; 96 * 2];
        for _ in 0..20 {
            input.process(&data);
        }
        assert!(sent.lock().unwrap().iter().all(|&n| n == 64));
    }

    /// Frames the software monitor adds on top of the device's own buffers: input and output
    /// run in the same ASIO buffer switch (input first), so what comes in during a block can
    /// go out in that same block (0 added).
    /// (delay at the block's middle frame, smallest and largest delay over the last 100
    /// blocks, frames that did not follow their predecessor, monitor jitter-buffer stats)
    fn monitor_delay(block: usize, packet: u16, output_first: bool) -> (Option<i64>, i64, i64, usize, u64) {
        let cfg = EngineConfig {
            host: HostChoice::Asio,
            input_device: None,
            output_device: None,
            sample_rate: 48_000,
            buffer_size: block as u32,
            packet_frames: packet,
            input: InputChannels::Stereo(0, 1),
        };
        let sh = shared();
        let params = sh.self_params.clone();
        params.monitor.store(true, Relaxed);
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (garbage_tx, _garbage_rx) = rtrb::RingBuffer::new(8);
        let (mut input, mut output) = processors(&cfg, Box::new(Nowhere), cmd_rx, garbage_tx, sh, (48_000, 2, 2, block as u32), true);
        // each input sample carries its own frame number (small, so the limiter leaves it be)
        let code = |n: usize| 1e-6 * (n as f32 + 1.0);
        let mut inbuf = vec![0f32; block * 2];
        let mut out = vec![0f32; block * 2];
        let mut delay = None;
        let (mut lo, mut hi, mut breaks) = (i64::MAX, i64::MIN, 0usize);
        let mut prev: Option<i64> = None;
        for b in 0..400 {
            for k in 0..block {
                inbuf[k * 2] = code(b * block + k);
                inbuf[k * 2 + 1] = code(b * block + k);
            }
            if output_first {
                output.process(&mut out);
                input.process(&inbuf);
            } else {
                input.process(&inbuf);
                output.process(&mut out);
            }
            if b >= 300 {
                for k in 0..block {
                    let v = out[k * 2];
                    let n = (v / 1e-6).round() as i64 - 1;
                    // the frame that came out at output position b·block + k
                    let d = (b * block + k) as i64 - n;
                    if v > 0.5e-6 && (v / 1e-6 - (v / 1e-6).round()).abs() < 0.01 {
                        lo = lo.min(d);
                        hi = hi.max(d);
                        if k == block / 2 {
                            delay = Some(d);
                        }
                    }
                    if prev.is_some_and(|p| n != p + 1) {
                        breaks += 1;
                    }
                    prev = Some(n);
                }
            }
        }
        (delay, lo, hi, breaks, output.monitor_missing)
    }

    /// Any block and packet size: the block that came in goes out (it used to go through a
    /// one-packet jitter buffer, which broke up at 48/32 and from 256/32 on: crackling).
    #[test]
    fn the_software_monitor_adds_nothing_beyond_the_device() {
        for (block, packet) in [(32, 32), (64, 32), (128, 32), (64, 16), (48, 32), (96, 32), (256, 32), (512, 32), (1024, 16), (2048, 32)] {
            let (d, lo, hi, breaks, missing) = monitor_delay(block, packet, false);
            println!("monitor: block {block:>4}, packet {packet}: added {d:?} (range {lo}..{hi}), breaks {breaks}, missing {missing}");
            assert_eq!((lo, hi, breaks), (0, 0, 0), "block {block}, packet {packet}");
            assert_eq!(missing, 0, "block {block}");
        }
    }

    /// The two audio callbacks never touch the allocator: with three others in the room,
    /// packets lost (concealment), a stall and its burst (catching up by playing faster),
    /// targets moved up and down (stretching), a muted and a soloed peer.
    #[test]
    fn the_audio_callbacks_do_not_allocate() {
        let cfg = EngineConfig {
            host: HostChoice::Asio,
            input_device: None,
            output_device: None,
            sample_rate: 48_000,
            buffer_size: 64,
            packet_frames: 32,
            input: InputChannels::Stereo(0, 1),
        };
        let sh = shared();
        sh.self_params.monitor.store(true, Relaxed);
        let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (garbage_tx, _garbage_rx) = rtrb::RingBuffer::new(8);
        let (mut input, mut output) = processors(&cfg, Box::new(Nowhere), cmd_rx, garbage_tx, sh, (48_000, 2, 2, 64), true);
        let mut peers = Vec::new();
        for id in 1..=3u16 {
            let (tx, rx) = rtrb::RingBuffer::<Frame>::new(PEER_RING);
            let handle = PeerHandle {
                params: Arc::new(PeerParams::default()),
                meter: Arc::new(Meter::default()),
                stats: Arc::new(SharedJitterStats::default()),
            };
            let slot = Box::new(PeerSlot { id, rx, jitter: JitterBuffer::new(3), handle: handle.clone(), gains: (f32::NAN, f32::NAN), channels: 2 });
            assert!(cmd_tx.push(Cmd::AddPeer(slot)).is_ok());
            peers.push((tx, handle));
        }
        let tone = |seq: u32| {
            let mut f = Frame { seq, channels: 2, frames: 32, ..Frame::EMPTY };
            for k in 0..32 {
                let v = 0.3 * (std::f32::consts::TAU * 220.0 * (seq as usize * 32 + k) as f32 / 48_000.0).sin();
                f.data[k * 2] = v;
                f.data[k * 2 + 1] = v;
            }
            f
        };
        let inbuf = vec![0.1f32; 64 * 2];
        let mut out = vec![0f32; 64 * 2];
        // warm up outside the guard (the peers are taken in, buffers primed)
        let mut seq = 0u32;
        let mut held: Vec<u32> = Vec::with_capacity(256);
        let mut block = |b: u32, input: &mut InputProcessor, output: &mut OutputProcessor, peers: &mut Vec<(rtrb::Producer<Frame>, PeerHandle)>, held: &mut Vec<u32>| {
            for _ in 0..2 {
                let stalled = (600..630).contains(&(b % 1000)); // 40 ms of nothing, then all at once
                let lost = seq % 97 == 5;
                if stalled {
                    held.push(seq);
                } else {
                    for s in held.drain(..) {
                        for (tx, _) in peers.iter_mut() {
                            let _ = tx.push(tone(s));
                        }
                    }
                    if !lost {
                        for (tx, _) in peers.iter_mut() {
                            let _ = tx.push(tone(seq));
                        }
                    }
                }
                seq += 1;
            }
            // targets move, a peer is muted, another soloed for a while
            let target = match (b / 400) % 3 {
                0 => 300,
                1 => 1500,
                _ => 500,
            };
            for (_, h) in peers.iter() {
                h.params.jitter_target.store(target, Relaxed);
            }
            peers[0].1.params.mute.store((b / 700) % 2 == 1, Relaxed);
            peers[1].1.params.solo.store((b / 900) % 2 == 1, Relaxed);
            input.process(&inbuf);
            output.process(&mut out);
        };
        for b in 0..100 {
            block(b, &mut input, &mut output, &mut peers, &mut held);
        }
        let calls = crate::alloc_guard::count(|| {
            for b in 100..4100 {
                block(b, &mut input, &mut output, &mut peers, &mut held);
            }
        });
        let st = peers[0].1.stats.clone();
        assert!(st.concealed.load(Relaxed) > 0 && st.underruns.load(Relaxed) > 0, "the test must exercise concealment and dry-outs");
        assert_eq!(calls, 0, "the audio callbacks asked the allocator {calls} times");
        // (the guard does count: one Vec is an allocation and a free)
        assert!(crate::alloc_guard::count(|| drop(std::hint::black_box(vec![1u8; 100]))) >= 2);
    }

    /// A host that runs the output before the input: one block late, still unbroken.
    #[test]
    fn the_software_monitor_survives_output_first() {
        for block in [32, 64, 256] {
            let (_, lo, hi, breaks, missing) = monitor_delay(block, 32, true);
            assert_eq!((lo, hi, breaks), (block as i64, block as i64, 0), "block {block}");
            assert_eq!(missing, block as u64, "only the very first block is empty");
        }
    }
}
