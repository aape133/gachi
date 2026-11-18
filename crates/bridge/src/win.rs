//! The shared memory region and the wake-up event, on Windows.

use crate::{ring, Error, CHANNELS, MAGIC, RING_FRAMES, STALE_MS, VERSION};
use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READWRITE,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};

const SAMPLES: usize = RING_FRAMES * CHANNELS;

/// One counter per cache line: the two processes write different ones on every block.
#[repr(C, align(64))]
struct Line(AtomicU64);

/// The region both processes map. Zeroed by the system when first created, which is a valid
/// state for every field (magic 0 = not set up yet).
#[repr(C)]
struct Shared {
    magic: AtomicU32,
    version: AtomicU32,
    app_pid: AtomicU32,
    sample_rate: AtomicU32,
    block_frames: AtomicU32,
    /// how loud the plugin passes the player's own sound through (f32 bits; the app's
    /// "own sound" fader). The plugin's own on/off switch comes on top.
    own_gain: AtomicU32,
    /// the plugin instance that owns the bridge (0 = none)
    owner: AtomicU64,
    /// last time another plugin instance found the bridge taken (it keeps asking twice a
    /// second): the app tells the player only one gachi plugin works at a time
    waiting_ms: AtomicU64,
    /// last block the plugin processed / the app looked in (system-wide ms clock)
    plugin_beat_ms: AtomicU64,
    app_beat_ms: AtomicU64,
    up_write: Line,
    up_read: Line,
    down_write: Line,
    down_read: Line,
    up: UnsafeCell<[f32; SAMPLES]>,
    down: UnsafeCell<[f32; SAMPLES]>,
}

const SIZE: usize = std::mem::size_of::<Shared>();

fn now_ms() -> u64 {
    // SAFETY: no arguments; reads the system tick count (same clock in every process).
    unsafe { GetTickCount64() }
}

fn last_error() -> u32 {
    // SAFETY: reads the calling thread's last-error value.
    unsafe { GetLastError() }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The mapped region and its handle; unmapped and closed on drop.
struct Mapping {
    handle: HANDLE,
    view: *const Shared,
}

// SAFETY: the view is shared memory accessed only through atomics and the ring protocol
// (each ring has one writer and one reader); the handle may be closed from any thread.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn shared(&self) -> &Shared {
        // SAFETY: `view` points at SIZE mapped bytes for as long as `self` lives; every field
        // is valid for any bit pattern the other side can leave there (atomics and floats).
        unsafe { &*self.view }
    }

    /// Maps `handle`; on failure closes it.
    fn map(handle: HANDLE) -> Result<Self, Error> {
        // SAFETY: `handle` is a file mapping handle we own; the view covers the whole region.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, SIZE) };
        if view.Value.is_null() {
            let e = last_error();
            // SAFETY: we own `handle` and give up on it here.
            unsafe { CloseHandle(handle) };
            return Err(Error::Os(e));
        }
        Ok(Self { handle, view: view.Value as *const Shared })
    }

    /// The layout check both sides make before using the region.
    fn check(&self) -> Result<(), Error> {
        let s = self.shared();
        if s.magic.load(Ordering::Acquire) != MAGIC {
            return Err(Error::BadMagic);
        }
        let theirs = s.version.load(Ordering::Relaxed);
        if theirs != VERSION {
            return Err(Error::Version { theirs, ours: VERSION });
        }
        Ok(())
    }

    fn up(&self) -> *mut f32 {
        self.shared().up.get().cast()
    }

    fn down(&self) -> *mut f32 {
        self.shared().down.get().cast()
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the view and handle came from MapViewOfFile / Create/OpenFileMappingW and are
        // released exactly once.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as *mut c_void });
            CloseHandle(self.handle);
        }
    }
}

/// A named auto-reset event: the plugin sets it after each block, the app waits on it.
struct Event(HANDLE);

// SAFETY: event handles may be used and closed from any thread.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    fn set(&self) {
        // SAFETY: a valid event handle we own.
        unsafe { SetEvent(self.0) };
    }

    /// Waits until set or `timeout_ms` passed; true if it was set.
    fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: a valid event handle we own.
        unsafe { WaitForSingleObject(self.0, timeout_ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

fn names(name: &str) -> (Vec<u16>, Vec<u16>) {
    (wide(&format!("Local\\{name}")), wide(&format!("Local\\{name}-up")))
}

/// Copies interleaved frames into `ring` from frame number `at` (wrapping at the end).
///
/// # Safety
/// `ring` points at a ring of `SAMPLES` floats, and frames `at..at+frames` are ours to write
/// (the reader is past them).
unsafe fn copy_in(ring: *mut f32, at: u64, src: &[f32]) {
    let frames = src.len() / CHANNELS;
    let (start, first) = ring::slot(at, frames);
    std::ptr::copy_nonoverlapping(src.as_ptr(), ring.add(start * CHANNELS), first * CHANNELS);
    std::ptr::copy_nonoverlapping(src.as_ptr().add(first * CHANNELS), ring, (frames - first) * CHANNELS);
}

/// Copies interleaved frames out of `ring` from frame number `at`.
///
/// # Safety
/// As `copy_in`, for frames the writer has published.
unsafe fn copy_out(ring: *const f32, at: u64, dst: &mut [f32]) {
    let frames = dst.len() / CHANNELS;
    let (start, first) = ring::slot(at, frames);
    std::ptr::copy_nonoverlapping(ring.add(start * CHANNELS), dst.as_mut_ptr(), first * CHANNELS);
    std::ptr::copy_nonoverlapping(ring, dst.as_mut_ptr().add(first * CHANNELS), (frames - first) * CHANNELS);
}

/// Writes as many whole frames of `src` as fit; returns the frames written.
fn produce(ring: *mut f32, write: &Line, read: &Line, src: &[f32]) -> usize {
    let w = write.0.load(Ordering::Relaxed);
    let r = read.0.load(Ordering::Acquire);
    let room = ring::writable(w, r).unwrap_or(0);
    let n = (src.len() / CHANNELS).min(room);
    if n > 0 {
        // SAFETY: frames w..w+n are behind the reader's position + RING_FRAMES (checked above)
        // and only this side writes this ring.
        unsafe { copy_in(ring, w, &src[..n * CHANNELS]) };
        write.0.store(w + n as u64, Ordering::Release);
    }
    n
}

/// Reads up to `dst.len() / 2` frames; returns the frames read. Broken counters (the other
/// side was replaced) start over at the writer's position.
fn consume(ring: *const f32, write: &Line, read: &Line, dst: &mut [f32]) -> usize {
    let w = write.0.load(Ordering::Acquire);
    let r = read.0.load(Ordering::Relaxed);
    let Some(avail) = ring::readable(w, r) else {
        read.0.store(w, Ordering::Release);
        return 0;
    };
    let n = (dst.len() / CHANNELS).min(avail);
    if n > 0 {
        // SAFETY: frames r..r+n were published by the writer (Acquire above) and it will not
        // touch them until we move `read` past them.
        unsafe { copy_out(ring, r, &mut dst[..n * CHANNELS]) };
        read.0.store(r + n as u64, Ordering::Release);
    }
    n
}

/// Raises the calling thread to MMCSS "Pro Audio" (the app's bridge thread waits on the
/// plugin's blocks and must wake at once). Best effort; returns whether it worked.
pub fn pro_audio() -> bool {
    use windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let task = wide("Pro Audio");
    let mut index = 0u32;
    // SAFETY: NUL-terminated task name and a valid out-pointer; the registration lasts for
    // the thread's life.
    !unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) }.is_null()
}

/// The app's end: creates the bridge, waits for blocks from the plugin, sends the mix back.
pub struct AppEnd {
    map: Mapping,
    event: Event,
}

impl AppEnd {
    /// Creates (or, after an app restart, takes back) the bridge called `name`.
    pub fn create(name: &str) -> Result<Self, Error> {
        let (region, event_name) = names(name);
        // SAFETY: a pagefile-backed mapping of SIZE bytes with a NUL-terminated name.
        let handle = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, SIZE as u32, region.as_ptr())
        };
        if handle.is_null() {
            return Err(Error::Os(last_error()));
        }
        let existed = last_error() == ERROR_ALREADY_EXISTS;
        let map = Mapping::map(handle)?;
        let s = map.shared();
        if existed && s.magic.load(Ordering::Acquire) != 0 {
            // a plugin kept the region of our previous run open: same layout, or refuse
            map.check()?;
        } else {
            s.version.store(VERSION, Ordering::Relaxed);
            s.magic.store(MAGIC, Ordering::Release);
        }
        // SAFETY: GetCurrentProcessId takes no arguments.
        s.app_pid.store(unsafe { GetCurrentProcessId() }, Ordering::Relaxed);
        // whatever the plugin sent while we were away is stale
        let w = s.up_write.0.load(Ordering::Acquire);
        s.up_read.0.store(w, Ordering::Release);
        s.app_beat_ms.store(now_ms(), Ordering::Release);
        s.own_gain.store(1.0f32.to_bits(), Ordering::Release);
        // SAFETY: an unnamed-security auto-reset event, initially unset, NUL-terminated name.
        let ev = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
        if ev.is_null() {
            return Err(Error::Os(last_error()));
        }
        Ok(Self { map, event: Event(ev) })
    }

    /// Waits up to `timeout_ms` for the plugin to hand over a block. Also counts as our
    /// heartbeat (the plugin sees whether the app is still there).
    pub fn wait(&self, timeout_ms: u32) -> bool {
        self.beat();
        self.event.wait(timeout_ms)
    }

    /// Marks the app as alive (call at least every few hundred ms).
    pub fn beat(&self) {
        self.map.shared().app_beat_ms.store(now_ms(), Ordering::Release);
    }

    /// Frames the plugin has handed over and we have not read yet.
    pub fn up_available(&self) -> usize {
        let s = self.map.shared();
        ring::readable(s.up_write.0.load(Ordering::Acquire), s.up_read.0.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// Reads the plugin's sound (interleaved stereo); returns the frames read.
    pub fn read_up(&self, dst: &mut [f32]) -> usize {
        let s = self.map.shared();
        consume(self.map.up(), &s.up_write, &s.up_read, dst)
    }

    /// Drops all but the newest `keep` frames from the plugin (after a stall: play current
    /// sound, not old). Returns the frames dropped.
    pub fn skip_up_to_latest(&self, keep: usize) -> usize {
        let s = self.map.shared();
        let w = s.up_write.0.load(Ordering::Acquire);
        let r = s.up_read.0.load(Ordering::Relaxed);
        let avail = ring::readable(w, r).unwrap_or(RING_FRAMES);
        let drop = avail.saturating_sub(keep);
        s.up_read.0.store(w - (avail - drop) as u64, Ordering::Release);
        drop
    }

    /// Hands the mix to the plugin; returns the frames that fit.
    pub fn write_down(&self, src: &[f32]) -> usize {
        let s = self.map.shared();
        produce(self.map.down(), &s.down_write, &s.down_read, src)
    }

    /// Frames we sent that the plugin has not taken yet.
    pub fn down_queued(&self) -> usize {
        let s = self.map.shared();
        ring::readable(s.down_write.0.load(Ordering::Relaxed), s.down_read.0.load(Ordering::Acquire)).unwrap_or(0)
    }

    /// Another gachi plugin instance (a second track) asked for the bridge within
    /// `STALE_MS` and is waiting: it passes its sound through and sends nothing.
    pub fn plugin_waiting(&self) -> bool {
        let at = self.map.shared().waiting_ms.load(Ordering::Acquire);
        at != 0 && now_ms().saturating_sub(at) <= STALE_MS
    }

    /// A plugin owns the bridge and processed a block within `STALE_MS`.
    pub fn plugin_alive(&self) -> bool {
        let s = self.map.shared();
        s.owner.load(Ordering::Acquire) != 0 && now_ms().saturating_sub(s.plugin_beat_ms.load(Ordering::Acquire)) <= STALE_MS
    }

    /// How loud the plugin passes the player's own sound through (0 = not at all).
    pub fn set_own_gain(&self, gain: f32) {
        let g = if gain.is_finite() { gain.clamp(0.0, 4.0) } else { 0.0 };
        self.map.shared().own_gain.store(g.to_bits(), Ordering::Release);
    }

    /// The DAW's sample rate and block size as the plugin last reported them (0 = unknown).
    pub fn format(&self) -> (u32, u32) {
        let s = self.map.shared();
        (s.sample_rate.load(Ordering::Acquire), s.block_frames.load(Ordering::Acquire))
    }
}

/// The plugin's end: hands each block to the app and takes the mix back. Every method is
/// safe on the DAW's audio thread (no blocking, no allocation, one SetEvent per block).
pub struct PluginEnd {
    map: Mapping,
    event: Event,
    id: u64,
    /// frames dropped because the app was not reading (it stalled or quit)
    pub overruns: u64,
}

impl PluginEnd {
    /// Opens the app's bridge and claims it for plugin instance `id` (non-zero, unique per
    /// instance). Not for the audio thread: it makes system calls that may take a while.
    pub fn open(name: &str, id: u64) -> Result<Self, Error> {
        assert_ne!(id, 0, "0 means no owner");
        let (region, event_name) = names(name);
        // SAFETY: NUL-terminated name; we ask for read/write access.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, region.as_ptr()) };
        if handle.is_null() {
            let e = last_error();
            return Err(if e == ERROR_FILE_NOT_FOUND { Error::NotRunning } else { Error::Os(e) });
        }
        let map = Mapping::map(handle)?;
        map.check()?;
        let s = map.shared();
        // claim it: free, ours already, or the owner went quiet
        loop {
            let cur = s.owner.load(Ordering::Acquire);
            let stale = now_ms().saturating_sub(s.plugin_beat_ms.load(Ordering::Acquire)) > STALE_MS;
            if cur != 0 && cur != id && !stale {
                // let the app know: the player may think this track is sent too
                s.waiting_ms.store(now_ms(), Ordering::Release);
                return Err(Error::Busy);
            }
            if s.owner.compare_exchange(cur, id, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                break;
            }
        }
        s.plugin_beat_ms.store(now_ms(), Ordering::Release);
        // what the app mixed for an earlier owner is stale
        let w = s.down_write.0.load(Ordering::Acquire);
        s.down_read.0.store(w, Ordering::Release);
        // SAFETY: NUL-terminated name; rights to signal it.
        let ev = unsafe { OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, 0, event_name.as_ptr()) };
        if ev.is_null() {
            let e = last_error();
            s.owner.store(0, Ordering::Release);
            return Err(Error::Os(e));
        }
        Ok(Self { map, event: Event(ev), id, overruns: 0 })
    }

    /// Tells the app the DAW's sample rate and block size (call when they change).
    pub fn set_format(&self, sample_rate: u32, block_frames: u32) {
        let s = self.map.shared();
        s.sample_rate.store(sample_rate, Ordering::Release);
        s.block_frames.store(block_frames, Ordering::Release);
    }

    /// Still the owner. Another instance takes the bridge over when this one stops
    /// processing for `STALE_MS` (bypassed, track muted, DAW stopped): from then on this one
    /// must not send (two senders would interleave in one ring) and should let it go.
    pub fn owns(&self) -> bool {
        self.map.shared().owner.load(Ordering::Acquire) == self.id
    }

    /// Hands one block (interleaved stereo) to the app and wakes it. Frames that do not fit
    /// (the app stopped reading) are dropped and counted in `overruns`. Sends nothing once
    /// another instance took the bridge over (see [`owns`](Self::owns)).
    pub fn write_up(&mut self, src: &[f32]) -> usize {
        if !self.owns() {
            return 0;
        }
        let s = self.map.shared();
        s.plugin_beat_ms.store(now_ms(), Ordering::Release);
        let n = produce(self.map.up(), &s.up_write, &s.up_read, src);
        self.overruns += (src.len() / CHANNELS - n) as u64;
        self.event.set();
        n
    }

    /// Takes up to `dst.len() / 2` frames of the app's mix; returns the frames read (the
    /// rest of `dst` is left as it was).
    pub fn read_down(&mut self, dst: &mut [f32]) -> usize {
        if !self.owns() {
            return 0;
        }
        let s = self.map.shared();
        consume(self.map.down(), &s.down_write, &s.down_read, dst)
    }

    /// Frames of the app's mix waiting for us.
    pub fn down_available(&self) -> usize {
        let s = self.map.shared();
        ring::readable(s.down_write.0.load(Ordering::Acquire), s.down_read.0.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// How loud to pass the player's own sound through, as the app asks (see
    /// [`AppEnd::set_own_gain`]). Checked here too: it goes into the DAW's mix, and the
    /// region is anyone's on this desktop to write.
    pub fn own_gain(&self) -> f32 {
        let g = f32::from_bits(self.map.shared().own_gain.load(Ordering::Acquire));
        if g.is_finite() { g.clamp(0.0, 4.0) } else { 0.0 }
    }

    /// The app is running and looked in within `STALE_MS`.
    pub fn app_alive(&self) -> bool {
        let s = self.map.shared();
        now_ms().saturating_sub(s.app_beat_ms.load(Ordering::Acquire)) <= STALE_MS
    }
}

impl Drop for PluginEnd {
    fn drop(&mut self) {
        // give the bridge back (if still ours) so another instance can take it at once
        let _ = self.map.shared().owner.compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A name no other test (or a running gachi) uses.
    fn unique() -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        format!("gachi-bridge-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn the_plugin_finds_no_app_when_none_runs() {
        assert_eq!(PluginEnd::open(&unique(), 1).err(), Some(Error::NotRunning));
    }

    #[test]
    fn blocks_go_up_and_down_in_order_across_threads() {
        let name = unique();
        let app = AppEnd::create(&name).unwrap();
        let mut plugin = PluginEnd::open(&name, 7).unwrap();
        plugin.set_format(48_000, 64);
        assert_eq!(app.format(), (48_000, 64));
        const BLOCKS: u32 = 20_000; // ~150 rings' worth: plenty of wrap-arounds
        let app_thread = std::thread::spawn(move || {
            // echo every frame back, checking the order on the way
            let mut buf = [0f32; 64 * CHANNELS];
            let mut expect = 0u32;
            while expect < BLOCKS * 64 {
                if !app.wait(2000) && app.up_available() == 0 {
                    panic!("no block from the plugin at frame {expect}");
                }
                loop {
                    let n = app.read_up(&mut buf);
                    if n == 0 {
                        break;
                    }
                    for f in buf[..n * CHANNELS].chunks(CHANNELS) {
                        assert_eq!(f, [expect as f32, -(expect as f32)], "frame {expect}");
                        expect += 1;
                    }
                    // the way down can be full for a moment (this thread was late and moved a
                    // whole ring's worth at once): wait for the plugin to take some
                    let mut sent = 0;
                    while sent < n {
                        sent += app.write_down(&buf[sent * CHANNELS..n * CHANNELS]);
                        std::thread::yield_now();
                    }
                }
            }
            app
        });
        let mut back = 0u32;
        let mut out = [0f32; 64 * CHANNELS];
        let mut take_back = |plugin: &mut PluginEnd, back: &mut u32| {
            let n = plugin.read_down(&mut out);
            for f in out[..n * CHANNELS].chunks(CHANNELS) {
                assert_eq!(f[0], *back as f32);
                *back += 1;
            }
        };
        for b in 0..BLOCKS {
            let block: Vec<f32> = (0..64).flat_map(|i| {
                let v = (b * 64 + i) as f32;
                [v, -v]
            }).collect();
            // a real app keeps pace; here the app thread may be descheduled for a while, so
            // keep at most half a ring in flight (neither way can then fill) instead of
            // counting overruns. Fail rather than hang if the app side gave up.
            while (b * 64 - back) as usize > RING_FRAMES / 2 {
                take_back(&mut plugin, &mut back);
                assert!(!app_thread.is_finished(), "the app side stopped at block {b}");
                std::thread::yield_now();
            }
            assert_eq!(plugin.write_up(&block), 64);
            take_back(&mut plugin, &mut back);
        }
        // the app may still be pushing its last frames down: drain alongside it
        while !app_thread.is_finished() {
            take_back(&mut plugin, &mut back);
            std::thread::yield_now();
        }
        let app = app_thread.join().unwrap();
        while back < BLOCKS * 64 {
            take_back(&mut plugin, &mut back);
        }
        assert_eq!(plugin.overruns, 0);
        assert!(app.plugin_alive() && plugin.app_alive());
    }

    #[test]
    fn one_plugin_instance_at_a_time() {
        let name = unique();
        let _app = AppEnd::create(&name).unwrap();
        let first = PluginEnd::open(&name, 1).unwrap();
        assert_eq!(PluginEnd::open(&name, 2).err(), Some(Error::Busy), "the first is alive");
        drop(first);
        let second = PluginEnd::open(&name, 2).expect("free again once the first is gone");
        // the owner crashed (never dropped): its claim lapses with its heartbeat
        std::mem::forget(second);
        let s = _app.map.shared();
        s.plugin_beat_ms.store(now_ms() - STALE_MS - 1, Ordering::Release);
        assert!(!_app.plugin_alive());
        PluginEnd::open(&name, 3).expect("a stale owner is taken over");
    }

    #[test]
    fn a_waiting_instance_is_shown_and_a_replaced_owner_goes_quiet() {
        let name = unique();
        let app = AppEnd::create(&name).unwrap();
        let mut first = PluginEnd::open(&name, 1).unwrap();
        assert!(!app.plugin_waiting());
        assert_eq!(PluginEnd::open(&name, 2).err(), Some(Error::Busy));
        assert!(app.plugin_waiting(), "the app can tell the player");
        // the first one is bypassed for a while: the second takes over
        app.map.shared().plugin_beat_ms.store(now_ms() - STALE_MS - 1, Ordering::Release);
        let mut second = PluginEnd::open(&name, 2).unwrap();
        assert!(!first.owns() && second.owns());
        // the first comes back from bypass: it must not send into the second's stream
        assert_eq!(first.write_up(&[1.0; 64 * CHANNELS]), 0);
        assert_eq!(second.write_up(&[2.0; 64 * CHANNELS]), 64);
        let mut buf = [0f32; 128 * CHANNELS];
        assert_eq!(app.read_up(&mut buf), 64, "only the owner's block");
        assert_eq!(buf[0], 2.0);
        drop(first); // and leaving does not free the bridge under the second
        assert!(second.owns());
    }

    #[test]
    fn the_app_sets_how_loud_the_plugin_passes_own_sound() {
        let name = unique();
        let app = AppEnd::create(&name).unwrap();
        let plugin = PluginEnd::open(&name, 1).unwrap();
        assert_eq!(plugin.own_gain(), 1.0, "unity until the app says otherwise");
        app.set_own_gain(0.5);
        assert_eq!(plugin.own_gain(), 0.5);
        app.set_own_gain(f32::NAN);
        assert_eq!(plugin.own_gain(), 0.0);
    }

    #[test]
    fn another_layout_version_is_refused() {
        let name = unique();
        let app = AppEnd::create(&name).unwrap();
        app.map.shared().version.store(VERSION + 1, Ordering::Relaxed);
        assert_eq!(PluginEnd::open(&name, 1).err(), Some(Error::Version { theirs: VERSION + 1, ours: VERSION }));
    }

    #[test]
    fn a_stalled_app_costs_dropped_frames_not_a_blocked_plugin() {
        let name = unique();
        let app = AppEnd::create(&name).unwrap();
        let mut plugin = PluginEnd::open(&name, 1).unwrap();
        let block = [0.5f32; 64 * CHANNELS];
        // the app reads nothing for longer than the ring holds
        let blocks = RING_FRAMES / 64 + 10;
        for _ in 0..blocks {
            plugin.write_up(&block);
        }
        assert_eq!(plugin.overruns, 10 * 64);
        // when it comes back it skips to the newest sound instead of playing 170 ms late
        assert_eq!(app.skip_up_to_latest(128), RING_FRAMES - 128);
        assert_eq!(app.up_available(), 128);
    }

    #[test]
    fn an_app_restart_takes_the_region_back() {
        let name = unique();
        let app = AppEnd::create(&name).unwrap();
        let mut plugin = PluginEnd::open(&name, 1).unwrap();
        plugin.write_up(&[1.0; 64 * CHANNELS]);
        drop(app); // the plugin still holds the region open
        let app = AppEnd::create(&name).expect("same layout: taken back");
        assert_eq!(app.up_available(), 0, "what was sent while away is stale");
        plugin.write_up(&[2.0; 64 * CHANNELS]);
        let mut buf = [0f32; 64 * CHANNELS];
        assert_eq!(app.read_up(&mut buf), 64);
        assert_eq!(buf[0], 2.0);
    }
}
