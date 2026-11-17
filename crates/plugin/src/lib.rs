//! The gachi DAW plugin (VST3, Windows): put it at the end of the track (or group, or
//! master) whose sound you want to play with others. Each block it
//!
//! 1. hands the block to the gachi app over the same-PC bridge (`gachi-bridge`): sent to
//!    the room as is (after the DAW's effects),
//! 2. waits a moment for the app's answer: the other players, mixed (usually back within
//!    microseconds: then played in this very block, no added delay),
//! 3. outputs your own sound (at the level gachi's "own sound" fader sets; the plugin's own
//!    "My sound" switch can turn it off) plus that mix.
//!
//! The order matters: the block is sent before the mix is added, so the others' sound never
//! goes back to them. Without the app (not running, or not in DAW mode) the plugin passes the
//! sound through unchanged and keeps looking for it in the background; no DAW thread ever
//! waits on that. Parameters: "Bypass" (the DAW's bypass button: plain pass-through, nothing
//! sent) and "My sound" (hear yourself through the plugin, or not: off when you listen to
//! the interface's direct monitoring instead). No editor: the DAW shows its generic one.

#![allow(non_snake_case)]
// Every VST3 method is `unsafe fn` (the host hands raw pointers). The rules here: a pointer
// from the host is checked for null before use; buffers are read only within the sizes the
// host states (and the bus layout we agreed to); nothing on the audio thread blocks.

use std::cell::Cell;
use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vst3::{uid, Class, ComRef, ComWrapper, Steinberg::Vst::*, Steinberg::*};

const NAME: &str = "gachi";
const VENDOR: &str = "gachi";
/// Parameter ids.
const P_BYPASS: u32 = 0;
const P_OWN: u32 = 1;
/// VST3's "never ends": the DAW keeps calling us during silence (else it may stop processing
/// a silent track, and the others' sound would stop with it).
const INFINITE_TAIL: u32 = u32::MAX;
/// Longest the plugin waits for the app's mix: a quarter of the block, at most this much
/// (measured: the answer is back in ~5 µs, 45 µs at p99.9).
const MAX_WAIT: Duration = Duration::from_micros(300);

fn copy_cstring(src: &str, dst: &mut [c_char]) {
    let c = CString::new(src).unwrap_or_default();
    let bytes = c.as_bytes_with_nul();
    let too_long = bytes.len() > dst.len();
    for (s, d) in bytes.iter().zip(dst.iter_mut()) {
        *d = *s as c_char;
    }
    if too_long {
        if let Some(last) = dst.last_mut() {
            *last = 0;
        }
    }
}

fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut len = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        len += 1;
    }
    if len < dst.len() {
        dst[len] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

/* ───────────────────────── the bridge, kept connected in the background ───────────────────────── */

/// The plugin's end of the bridge, opened and re-opened by a background thread (opening makes
/// system calls: not for the DAW's audio thread). The audio thread only `try_lock`s it, and
/// passes the sound through when that fails, so the background thread keeps off the lock
/// while the audio thread reports all is well (else it would cost a block of the others'
/// sound now and then).
struct Link {
    end: Mutex<Option<Bridge>>,
    stop: AtomicBool,
    /// blocks the audio thread exchanged with the app (it counts, the background thread
    /// looks whether the count moved)
    blocks: AtomicU32,
    /// at the last block: still the owner, and the app alive
    well: AtomicBool,
}

#[cfg(windows)]
type Bridge = gachi_bridge::PluginEnd;
#[cfg(not(windows))]
struct Bridge;

impl Link {
    fn new() -> Arc<Self> {
        Arc::new(Self { end: Mutex::new(None), stop: AtomicBool::new(false), blocks: AtomicU32::new(0), well: AtomicBool::new(false) })
    }

    /// Keeps trying to open the app's bridge (twice a second) and lets go of it once the app
    /// is gone, until `stop` (and an unpark: no waiting out the pause).
    #[cfg(windows)]
    fn run(self: Arc<Self>, id: u64) {
        let name = gachi_bridge::name();
        let mut gone_since: Option<Instant> = None;
        let mut seen_blocks = self.blocks.load(Relaxed);
        while !self.stop.load(Relaxed) {
            // the audio thread is exchanging blocks and all is well: nothing to do, and no
            // reason to take the lock it needs
            let blocks = self.blocks.load(Relaxed);
            let busy = blocks != seen_blocks;
            seen_blocks = blocks;
            if busy && self.well.load(Relaxed) {
                gone_since = None;
                std::thread::park_timeout(Duration::from_millis(500));
                continue;
            }
            let mut guard = self.end.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                None => {
                    if let Ok(end) = gachi_bridge::PluginEnd::open(&name, id) {
                        *guard = Some(end);
                        gone_since = None;
                    }
                }
                // another instance took over while we were not processing (bypassed):
                // let go, and ask again like any second instance
                Some(end) if !end.owns() => {
                    *guard = None;
                    gone_since = None;
                }
                Some(end) if !end.app_alive() => {
                    // the app quit (or left DAW mode): let the bridge go after a moment, so a
                    // new app (or another instance) finds it free
                    let since = *gone_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > Duration::from_secs(2) {
                        *guard = None;
                        gone_since = None;
                    }
                }
                Some(_) => gone_since = None,
            }
            drop(guard);
            std::thread::park_timeout(Duration::from_millis(500));
        }
        *self.end.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[cfg(not(windows))]
    fn run(self: Arc<Self>, _id: u64) {}
}

/// A number no other plugin instance in this or another DAW picks.
fn instance_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(std::process::id() as u64);
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish() | 1 // never 0 (= no owner)
}

/* ───────────────────────── processor ───────────────────────── */

/// What only the audio thread touches (VST3 does not call `process` concurrently with
/// itself or with setup): preallocated buffers and the stream format.
struct Audio {
    sample_rate: u32,
    /// blocks in a row the app's answer did not come in time: then we stop waiting for it
    /// (the app is gone or stuck) until it answers in time again
    missed: u32,
    /// the level your own sound went out at in the last block
    own: f32,
    up: Vec<f32>,
    down: Vec<f32>,
}

struct Processor {
    bypass: AtomicBool,
    own: AtomicBool,
    link: Arc<Link>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    audio: Mutex<Audio>,
    max_block: AtomicU32,
    /// the bus widths agreed with the host (2 = stereo, the default; 1 on a mono track)
    in_channels: AtomicU32,
    out_channels: AtomicU32,
}

impl Class for Processor {
    type Interfaces = (IComponent, IAudioProcessor, IProcessContextRequirements);
}

impl Processor {
    const CID: TUID = uid(0x6A3C_1E52, 0x8F0B_4D7A, 0x9C21_5B7E, 0x0A61_C0DE);

    fn new() -> Self {
        Self {
            bypass: AtomicBool::new(false),
            own: AtomicBool::new(true),
            link: Link::new(),
            worker: Mutex::new(None),
            audio: Mutex::new(Audio { sample_rate: 48_000, missed: 0, own: f32::NAN, up: Vec::new(), down: Vec::new() }),
            max_block: AtomicU32::new(0),
            in_channels: AtomicU32::new(2),
            out_channels: AtomicU32::new(2),
        }
    }

    /// Starts looking for the app (when the DAW activates us).
    fn connect(&self) {
        let mut w = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if w.is_none() {
            self.link.stop.store(false, Relaxed);
            let link = self.link.clone();
            let id = instance_id();
            *w = std::thread::Builder::new().name("gachi-plugin-link".into()).spawn(move || link.run(id)).ok();
        }
    }

    /// Lets go of the bridge (deactivated or removed): another instance may take it.
    fn disconnect(&self) {
        self.link.stop.store(true, Relaxed);
        if let Some(t) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }

    /// Applies the last value of each parameter change in this block.
    unsafe fn read_params(&self, changes: *mut IParameterChanges) {
        let Some(changes) = ComRef::from_raw(changes) else { return };
        for i in 0..changes.getParameterCount() {
            let Some(q) = ComRef::from_raw(changes.getParameterData(i)) else { continue };
            let n = q.getPointCount();
            if n <= 0 {
                continue;
            }
            let (mut offset, mut value) = (0, 0.0);
            if q.getPoint(n - 1, &mut offset, &mut value) != kResultTrue {
                continue;
            }
            match q.getParameterId() {
                P_BYPASS => self.bypass.store(value >= 0.5, Relaxed),
                P_OWN => self.own.store(value >= 0.5, Relaxed),
                _ => {}
            }
        }
    }

}

/// Sends `a.up[..n * 2]` (the block, already copied in) and leaves the app's mix in
/// `a.down[..n * 2]` (silence for what did not come back in time).
#[cfg(windows)]
fn exchange(a: &mut Audio, end: &mut Bridge, n: usize) {
    // every block, not only on a change: the bridge may be a new one (the app restarted
    // and started on a fresh region) that has never heard it. Two stores into the line
    // the heartbeat already writes.
    end.set_format(a.sample_rate, n as u32);
    // what is waiting already was mixed for an earlier block (the app answered late once):
    // this block's mix comes after it. Waiting only for "a block's worth" took that old
    // mix for this one, and from then on every block played the mix of the block before:
    // a block of delay for good, from a single late answer
    let stale = end.down_available();
    let want = stale + n;
    // 1. send (before anything of the others is added: it never goes back to them)
    let t = Instant::now();
    end.write_up(&a.up[..n * 2]);
    // 2. the app answers within microseconds: wait for it, a little. An app that keeps
    // missing (gone, stuck, or slower than the wait) is not waited for on every block (the
    // wait is a quarter of the block, spent on the DAW's audio thread), but on every 8th:
    // after a stall of a few blocks it is quick again, and back in step within 8 blocks
    let wait = a.missed < 2 || a.missed.is_multiple_of(8);
    let budget = if wait { Duration::from_secs_f64(n as f64 / a.sample_rate.max(1) as f64 / 4.0).min(MAX_WAIT) } else { Duration::ZERO };
    while end.down_available() < want && t.elapsed() < budget && end.app_alive() {
        std::hint::spin_loop();
    }
    a.missed = if end.down_available() >= want { 0 } else { a.missed.saturating_add(1) };
    // 3. the newest block's worth: what is older goes (play current sound, not old). If
    // this block's mix is not back, that is the last one's: a block late beats silence
    let mut extra = end.down_available().saturating_sub(n);
    while extra > 0 {
        let k = extra.min(n);
        if end.read_down(&mut a.down[..k * 2]) == 0 {
            break;
        }
        extra -= k;
    }
    let got = end.read_down(&mut a.down[..n * 2]);
    a.down[got * 2..n * 2].fill(0.0);
}

#[cfg(not(windows))]
fn exchange(a: &mut Audio, _: &mut Bridge, n: usize) {
    a.down[..n * 2].fill(0.0);
}

#[cfg(windows)]
fn own_gain(end: &Bridge) -> f32 {
    end.own_gain()
}

#[cfg(not(windows))]
fn own_gain(_: &Bridge) -> f32 {
    1.0
}

/// Still ours and the app is there: the background thread need not look.
#[cfg(windows)]
fn bridge_well(end: &Bridge) -> bool {
    end.owns() && end.app_alive()
}

#[cfg(not(windows))]
fn bridge_well(_: &Bridge) -> bool {
    false
}

impl Processor {
    /// One block (see the module docs). Called through `process`, which keeps a panic in.
    unsafe fn process_block(&self, data: *mut ProcessData) -> tresult {
        let Some(data) = data.as_ref() else { return kInvalidArgument };
        // 32-bit float only (canProcessSampleSize); a host that sends 64-bit anyway would
        // have us read its doubles as floats
        if data.symbolicSampleSize != SymbolicSampleSizes_::kSample32 {
            return kResultFalse;
        }
        self.read_params(data.inputParameterChanges);
        let n = data.numSamples.max(0) as usize;
        if n == 0 || data.numInputs < 1 || data.numOutputs < 1 || data.inputs.is_null() || data.outputs.is_null() {
            return kResultOk;
        }
        let ins = &*data.inputs;
        let outs = &*data.outputs;
        // a mono track hands over one channel (whatever arrangement was agreed): take what
        // is there. Leaving the output unwritten would play whatever was in the buffer.
        let (ic, oc) = (ins.numChannels.clamp(0, 2) as usize, outs.numChannels.clamp(0, 2) as usize);
        if ic == 0 || oc == 0 || ins.__field0.channelBuffers32.is_null() || outs.__field0.channelBuffers32.is_null() {
            return kResultOk;
        }
        let in_ch = slice::from_raw_parts(ins.__field0.channelBuffers32, ic);
        let out_ch = slice::from_raw_parts(outs.__field0.channelBuffers32, oc);
        if in_ch.iter().chain(out_ch).any(|p| p.is_null()) {
            return kResultOk;
        }
        // raw pointers only: the host may process in place (input and output the same
        // memory), so no input slice may live while the output is written
        let (li, ri) = (in_ch[0] as *const f32, in_ch[ic - 1] as *const f32);
        let (lo, ro) = (out_ch[0], (oc == 2).then(|| out_ch[1]));
        let in_silent = ins.silenceFlags;
        let out_bus = data.outputs;
        let pass = || {
            match ro {
                Some(ro) => {
                    // (the right first: with a mono input processed in place, writing the
                    // left would not change what the right is copied from, either way)
                    ptr::copy(ri, ro, n);
                    ptr::copy(li, lo, n);
                }
                None if ic == 2 => {
                    for k in 0..n {
                        *lo.add(k) = (*li.add(k) + *ri.add(k)) * 0.5;
                    }
                }
                None => ptr::copy(li, lo, n),
            }
            // (one flag bit per channel)
            (*out_bus).silenceFlags = if in_silent != 0 { (1u64 << oc) - 1 } else { 0 };
        };
        // a render to a file runs faster than time: nothing to send to the room, and
        // nothing of the room to print into the file
        if data.processMode == ProcessModes_::kOffline {
            pass();
            return kResultOk;
        }
        let Ok(mut a) = self.audio.try_lock() else {
            pass();
            return kResultOk;
        };
        if self.bypass.load(Relaxed) || a.up.len() < n * 2 {
            a.own = 1.0;
            pass();
            return kResultOk;
        }
        // the bridge is being (re)opened right now, or there is no app: pass through
        // (at full level: when the app is back, the level moves from there)
        let Ok(mut guard) = self.link.end.try_lock() else {
            a.own = 1.0;
            pass();
            return kResultOk;
        };
        let Some(end) = guard.as_mut() else {
            a.own = 1.0;
            pass();
            return kResultOk;
        };
        for k in 0..n {
            a.up[k * 2] = *li.add(k);
            a.up[k * 2 + 1] = *ri.add(k);
        }
        exchange(&mut a, end, n);
        let own = if self.own.load(Relaxed) { own_gain(end) } else { 0.0 };
        self.link.well.store(bridge_well(end), Relaxed);
        self.link.blocks.fetch_add(1, Relaxed);
        drop(guard);
        // (the level moves there over the block: a jump would click)
        let from = if a.own.is_nan() { own } else { a.own };
        a.own = own;
        let step = (own - from) / n as f32;
        match ro {
            Some(ro) => {
                for k in 0..n {
                    let g = from + step * (k + 1) as f32;
                    *lo.add(k) = a.up[k * 2] * g + a.down[k * 2];
                    *ro.add(k) = a.up[k * 2 + 1] * g + a.down[k * 2 + 1];
                }
            }
            // a mono output: both sides of your sound and of the others, halved
            None => {
                for k in 0..n {
                    let g = from + step * (k + 1) as f32;
                    *lo.add(k) = ((a.up[k * 2] + a.up[k * 2 + 1]) * g + a.down[k * 2] + a.down[k * 2 + 1]) * 0.5;
                }
            }
        }
        // the others' sound is in it even when the input was silent: a host that skips
        // silent buffers further down must not skip this one
        (*out_bus).silenceFlags = 0;
        kResultOk
    }
}

impl Drop for Processor {
    fn drop(&mut self) {
        self.disconnect();
    }
}

impl IPluginBaseTrait for Processor {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        self.disconnect();
        kResultOk
    }
}

/// The main bus, either way.
unsafe fn main_bus(dir: BusDirection, bus: *mut BusInfo, channels: i32) {
    let bus = &mut *bus;
    bus.mediaType = MediaTypes_::kAudio as MediaType;
    bus.direction = dir;
    bus.channelCount = channels;
    copy_wstring(if dir == BusDirections_::kInput as BusDirection { "Input" } else { "Output" }, &mut bus.name);
    bus.busType = BusTypes_::kMain as BusType;
    bus.flags = BusInfo_::BusFlags_::kDefaultActive as u32;
}

/// Saved state: [bypass, my sound], one byte each.
unsafe fn write_state(stream: *mut IBStream, bypass: bool, own: bool) -> tresult {
    let Some(s) = ComRef::from_raw(stream) else { return kResultFalse };
    let mut bytes = [bypass as u8, own as u8];
    let mut written = 0;
    s.write(bytes.as_mut_ptr() as *mut c_void, 2, &mut written)
}

unsafe fn read_state(stream: *mut IBStream) -> Option<(bool, bool)> {
    let s = ComRef::from_raw(stream)?;
    let mut bytes = [0u8, 1u8];
    let mut read = 0;
    (s.read(bytes.as_mut_ptr() as *mut c_void, 2, &mut read) == kResultOk && read == 2).then_some((bytes[0] != 0, bytes[1] != 0))
}

impl IComponentTrait for Processor {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        if class_id.is_null() {
            return kInvalidArgument;
        }
        *class_id = Controller::CID;
        kResultOk
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, media: MediaType, _dir: BusDirection) -> i32 {
        (media == MediaTypes_::kAudio as MediaType) as i32
    }

    unsafe fn getBusInfo(&self, media: MediaType, dir: BusDirection, index: i32, bus: *mut BusInfo) -> tresult {
        if media != MediaTypes_::kAudio as MediaType || index != 0 || bus.is_null() {
            return kInvalidArgument;
        }
        let input = dir == BusDirections_::kInput as BusDirection;
        main_bus(dir, bus, if input { &self.in_channels } else { &self.out_channels }.load(Relaxed) as i32);
        kResultOk
    }

    unsafe fn getRoutingInfo(&self, _in_info: *mut RoutingInfo, _out_info: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(&self, _media: MediaType, _dir: BusDirection, _index: i32, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        if state != 0 {
            self.connect();
        } else {
            self.disconnect();
        }
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        if let Some((bypass, own)) = read_state(state) {
            self.bypass.store(bypass, Relaxed);
            self.own.store(own, Relaxed);
        }
        kResultOk
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        write_state(state, self.bypass.load(Relaxed), self.own.load(Relaxed))
    }
}

impl IAudioProcessorTrait for Processor {
    unsafe fn setBusArrangements(&self, inputs: *mut SpeakerArrangement, num_ins: i32, outputs: *mut SpeakerArrangement, num_outs: i32) -> tresult {
        if num_ins != 1 || num_outs != 1 || inputs.is_null() || outputs.is_null() {
            return kResultFalse;
        }
        // stereo, or mono in (a mono track: a microphone, a guitar) with a mono or stereo
        // output. Some hosts refuse to insert a stereo-only plugin on a mono track.
        let width = |a: SpeakerArrangement| match a {
            SpeakerArr::kStereo => Some(2),
            SpeakerArr::kMono => Some(1),
            _ => None,
        };
        match (width(*inputs), width(*outputs)) {
            (Some(i), Some(o)) if i <= o => {
                self.in_channels.store(i, Relaxed);
                self.out_channels.store(o, Relaxed);
                kResultTrue
            }
            _ => kResultFalse,
        }
    }

    unsafe fn getBusArrangement(&self, dir: BusDirection, index: i32, arr: *mut SpeakerArrangement) -> tresult {
        if index != 0 || arr.is_null() {
            return kInvalidArgument;
        }
        let input = dir == BusDirections_::kInput as BusDirection;
        let channels = if input { &self.in_channels } else { &self.out_channels }.load(Relaxed);
        *arr = if channels == 1 { SpeakerArr::kMono } else { SpeakerArr::kStereo };
        kResultOk
    }

    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        if size == SymbolicSampleSizes_::kSample32 {
            kResultOk
        } else {
            kNotImplemented
        }
    }

    unsafe fn getLatencySamples(&self) -> u32 {
        0 // the mix comes back within the block: nothing for the DAW to compensate
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        let Some(setup) = setup.as_ref() else { return kInvalidArgument };
        let max = setup.maxSamplesPerBlock.max(1) as usize;
        self.max_block.store(max as u32, Relaxed);
        // buffers are sized here, never on the audio thread
        let mut a = self.audio.lock().unwrap_or_else(|e| e.into_inner());
        a.sample_rate = setup.sampleRate.round() as u32;
        a.up = vec![0.0; max * 2];
        a.down = vec![0.0; max * 2];
        kResultOk
    }

    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        // a panic must not unwind into the host (it would take the DAW down with it): the
        // block is then left as the host gave it
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { self.process_block(data) })).unwrap_or(kResultFalse)
    }

    unsafe fn getTailSamples(&self) -> u32 {
        INFINITE_TAIL
    }
}

impl IProcessContextRequirementsTrait for Processor {
    unsafe fn getProcessContextRequirements(&self) -> u32 {
        0
    }
}

/* ───────────────────────── controller (parameters, no editor) ───────────────────────── */

struct Controller {
    bypass: Cell<f64>,
    own: Cell<f64>,
}

impl Class for Controller {
    type Interfaces = (IEditController,);
}

impl Controller {
    const CID: TUID = uid(0x6A3C_1E52, 0x8F0B_4D7A, 0x9C21_5B7E, 0x0A61_C0DF);

    fn new() -> Self {
        Self { bypass: Cell::new(0.0), own: Cell::new(1.0) }
    }

    fn cell(&self, id: u32) -> Option<&Cell<f64>> {
        match id {
            P_BYPASS => Some(&self.bypass),
            P_OWN => Some(&self.own),
            _ => None,
        }
    }
}

impl IPluginBaseTrait for Controller {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IEditControllerTrait for Controller {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        if let Some((bypass, own)) = read_state(state) {
            self.bypass.set(bypass as u8 as f64);
            self.own.set(own as u8 as f64);
        }
        kResultOk
    }

    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> i32 {
        2
    }

    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        let Some(info) = info.as_mut() else { return kInvalidArgument };
        let (id, title, short, default, flags) = match index {
            0 => (P_BYPASS, "Bypass", "Bypass", 0.0, ParameterInfo_::ParameterFlags_::kIsBypass | ParameterInfo_::ParameterFlags_::kCanAutomate),
            1 => (P_OWN, "My sound", "Mine", 1.0, ParameterInfo_::ParameterFlags_::kCanAutomate),
            _ => return kInvalidArgument,
        };
        info.id = id;
        copy_wstring(title, &mut info.title);
        copy_wstring(short, &mut info.shortTitle);
        copy_wstring("", &mut info.units);
        info.stepCount = 1;
        info.defaultNormalizedValue = default;
        info.unitId = 0;
        info.flags = flags;
        kResultOk
    }

    unsafe fn getParamStringByValue(&self, id: u32, value: f64, string: *mut String128) -> tresult {
        let text = match (id, value >= 0.5) {
            (P_BYPASS, true) => "Bypassed",
            (P_BYPASS, false) => "On",
            (P_OWN, true) => "On",
            (P_OWN, false) => "Off",
            _ => return kInvalidArgument,
        };
        let Some(string) = string.as_mut() else { return kInvalidArgument };
        copy_wstring(text, string);
        kResultOk
    }

    unsafe fn getParamValueByString(&self, id: u32, string: *mut TChar, value: *mut f64) -> tresult {
        if self.cell(id).is_none() || string.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        let mut len = 0;
        while *string.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(slice::from_raw_parts(string as *const u16, len)).to_lowercase();
        *value = match text.trim() {
            "on" | "1" | "bypassed" => 1.0,
            "off" | "0" => 0.0,
            _ => return kInvalidArgument,
        };
        kResultOk
    }

    unsafe fn normalizedParamToPlain(&self, _id: u32, value: f64) -> f64 {
        value
    }

    unsafe fn plainParamToNormalized(&self, _id: u32, plain: f64) -> f64 {
        plain
    }

    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        self.cell(id).map_or(0.0, Cell::get)
    }

    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        match self.cell(id) {
            Some(c) => {
                c.set(value.clamp(0.0, 1.0));
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn setComponentHandler(&self, _handler: *mut IComponentHandler) -> tresult {
        kResultOk
    }

    unsafe fn createView(&self, _name: *const c_char) -> *mut IPlugView {
        ptr::null_mut() // no editor: the DAW's generic one shows the two switches
    }
}

/* ───────────────────────── factory ───────────────────────── */

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory2,);
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let Some(info) = info.as_mut() else { return kInvalidArgument };
        copy_cstring(VENDOR, &mut info.vendor);
        copy_cstring("", &mut info.url);
        copy_cstring("", &mut info.email);
        info.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
        kResultOk
    }

    unsafe fn countClasses(&self) -> i32 {
        2
    }

    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        let Some(info) = info.as_mut() else { return kInvalidArgument };
        let (cid, category) = match index {
            0 => (Processor::CID, "Audio Module Class"),
            1 => (Controller::CID, "Component Controller Class"),
            _ => return kInvalidArgument,
        };
        info.cid = cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_cstring(category, &mut info.category);
        copy_cstring(NAME, &mut info.name);
        kResultOk
    }

    unsafe fn createInstance(&self, cid: FIDString, iid: FIDString, obj: *mut *mut c_void) -> tresult {
        if cid.is_null() || iid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        let instance = match *(cid as *const TUID) {
            Processor::CID => ComWrapper::new(Processor::new()).to_com_ptr::<FUnknown>(),
            Controller::CID => ComWrapper::new(Controller::new()).to_com_ptr::<FUnknown>(),
            _ => None,
        };
        match instance {
            Some(instance) => {
                let p = instance.as_ptr();
                ((*(*p).vtbl).queryInterface)(p, iid as *mut TUID, obj)
            }
            None => kInvalidArgument,
        }
    }
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: i32, info: *mut PClassInfo2) -> tresult {
        let Some(info) = info.as_mut() else { return kInvalidArgument };
        let (cid, category, sub) = match index {
            0 => (Processor::CID, "Audio Module Class", "Fx"),
            1 => (Controller::CID, "Component Controller Class", ""),
            _ => return kInvalidArgument,
        };
        info.cid = cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_cstring(category, &mut info.category);
        copy_cstring(NAME, &mut info.name);
        info.classFlags = 0;
        copy_cstring(sub, &mut info.subCategories);
        copy_cstring(VENDOR, &mut info.vendor);
        copy_cstring(env!("CARGO_PKG_VERSION"), &mut info.version);
        copy_cstring("VST 3.7", &mut info.sdkVersion);
        kResultOk
    }
}

#[cfg(windows)]
#[no_mangle]
extern "system" fn InitDll() -> bool {
    true
}

#[cfg(windows)]
#[no_mangle]
extern "system" fn ExitDll() -> bool {
    true
}

#[no_mangle]
extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    ComWrapper::new(Factory).to_com_ptr::<IPluginFactory>().map_or(ptr::null_mut(), |p| p.into_raw())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// The app answers one block late once (its thread was held up); after that it answers
    /// at once again. The plugin must go back to playing each block's own mix, not stay a
    /// block behind for the rest of the session.
    #[test]
    fn one_late_answer_does_not_leave_the_mix_a_block_behind() {
        let name = format!("gachi-plugin-test-{}", std::process::id());
        let app = gachi_bridge::AppEnd::create(&name).expect("bridge");
        let mut end = gachi_bridge::PluginEnd::open(&name, 7).expect("plugin end");
        let stop = Arc::new(AtomicBool::new(false));
        let quit = stop.clone();
        // the app: hands each block straight back as the mix (so a mix says which block it
        // was made for); the first answer comes 5 ms late
        let worker = std::thread::spawn(move || {
            let mut up = vec![0f32; 8192];
            let mut first = true;
            while !quit.load(Ordering::Relaxed) {
                if !app.wait(20) && app.up_available() == 0 {
                    continue;
                }
                let n = app.read_up(&mut up);
                if n == 0 {
                    continue;
                }
                if std::mem::take(&mut first) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                app.write_down(&up[..n * 2]);
            }
        });
        let n = 256;
        let mut a = Audio { sample_rate: 48_000, missed: 0, own: f32::NAN, up: vec![0.0; n * 2], down: vec![0.0; n * 2] };
        // (the stall spans a few blocks, then the plugin waits again within 8 blocks)
        let mut current = 0;
        let blocks = 40;
        for k in 1..=blocks {
            a.up.fill(k as f32);
            exchange(&mut a, &mut end, n);
            if k > blocks - 20 && a.down.iter().all(|&s| s == k as f32) {
                current += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(current >= 18, "{current} of the last 20 blocks played their own mix");
    }
}
