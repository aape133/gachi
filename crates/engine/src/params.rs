//! Parameters the UI changes while audio runs: plain atomics, read once per callback.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

/// i64 with relaxed load/store (timestamps).
#[derive(Default)]
pub struct AtomicI64(std::sync::atomic::AtomicI64);

impl AtomicI64 {
    pub fn new(v: i64) -> Self {
        Self(std::sync::atomic::AtomicI64::new(v))
    }
    pub fn load(&self) -> i64 {
        self.0.load(Relaxed)
    }
    pub fn store(&self, v: i64) {
        self.0.store(v, Relaxed)
    }
}

#[derive(Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Relaxed))
    }
    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Relaxed)
    }
}

/// UI fader value (dB) → linear gain. -40 dB and below is treated as off.
pub fn db_to_gain(db: f32) -> f32 {
    if db <= -40.0 { 0.0 } else { 10f32.powf(db / 20.0) }
}

pub struct PeerParams {
    pub gain: AtomicF32,
    /// -1 (left) ..= 1 (right)
    pub pan: AtomicF32,
    pub mute: AtomicBool,
    pub solo: AtomicBool,
    /// jitter buffer target in hundredths of a packet (450 = 4.5 packets)
    pub jitter_target: AtomicU32,
    /// the peer's sample rate / ours (resampling when they differ)
    pub rate_ratio: AtomicF32,
}

impl Default for PeerParams {
    fn default() -> Self {
        Self {
            gain: AtomicF32::new(1.0),
            pan: AtomicF32::new(0.0),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            jitter_target: AtomicU32::new(300),
            rate_ratio: AtomicF32::new(1.0),
        }
    }
}

pub struct SelfParams {
    pub input_gain: AtomicF32,
    pub muted: AtomicBool,
    /// software monitoring of your own input (hardware direct monitoring is still lower latency)
    pub monitor: AtomicBool,
    /// DAW mode: how loud the plugin passes your own sound through (it does the monitoring,
    /// with no delay; `monitor` above does not apply there)
    pub monitor_gain: AtomicF32,
    /// DAW mode: the plugin passes your own sound through at all (the app's monitor button
    /// there; on unless turned off)
    pub own_on: AtomicBool,
    pub master: AtomicF32,
    /// frames of test tone left to play (store to start one)
    pub test_tone: AtomicU32,
    /// which input channels are sent (`InputChannels::encode`); changeable while running
    pub input_map: AtomicU64,
}

impl Default for SelfParams {
    fn default() -> Self {
        Self {
            input_gain: AtomicF32::new(1.0),
            muted: AtomicBool::new(false),
            monitor: AtomicBool::new(false),
            monitor_gain: AtomicF32::new(1.0),
            own_on: AtomicBool::new(true),
            master: AtomicF32::new(1.0),
            test_tone: AtomicU32::new(0),
            input_map: AtomicU64::new(0),
        }
    }
}
