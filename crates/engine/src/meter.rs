use std::sync::atomic::{AtomicU32, Ordering};

/// Stereo peak meter. The audio thread folds in peaks with `fetch_max`
/// (f32 bit patterns order correctly for non-negative values); the UI `take()`s
/// the peak since its last read.
#[derive(Default)]
pub struct Meter {
    l: AtomicU32,
    r: AtomicU32,
}

impl Meter {
    pub fn update_stereo(&self, interleaved: &[f32]) {
        let (mut l, mut r) = (0f32, 0f32);
        for c in interleaved.as_chunks::<2>().0 {
            l = l.max(c[0].abs());
            r = r.max(c[1].abs());
        }
        // NaN/inf would pin the meter at the top: skip them
        if l.is_finite() {
            self.l.fetch_max(l.to_bits(), Ordering::Relaxed);
        }
        if r.is_finite() {
            self.r.fetch_max(r.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn take(&self) -> [f32; 2] {
        [
            f32::from_bits(self.l.swap(0, Ordering::Relaxed)).min(1.0),
            f32::from_bits(self.r.swap(0, Ordering::Relaxed)).min(1.0),
        ]
    }
}

/// Max input channels we meter individually.
pub const MAX_INPUT_METERS: usize = 32;

/// Peak per device input channel, so the settings screen can show which channels carry
/// signal (e.g. where an interface's loopback lands). Same fetch_max/take scheme as [`Meter`].
pub struct ChannelMeters {
    peaks: [AtomicU32; MAX_INPUT_METERS],
}

impl Default for ChannelMeters {
    fn default() -> Self {
        Self { peaks: std::array::from_fn(|_| AtomicU32::new(0)) }
    }
}

impl ChannelMeters {
    pub fn update(&self, channel: usize, peak: f32) {
        if let (Some(p), true) = (self.peaks.get(channel), peak.is_finite()) {
            p.fetch_max(peak.abs().to_bits(), Ordering::Relaxed);
        }
    }

    /// Peaks of the first `n` channels since the last call (allocates: not for the audio thread).
    pub fn take(&self, n: usize) -> Vec<f32> {
        self.peaks[..n.min(MAX_INPUT_METERS)]
            .iter()
            .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)).min(1.0))
            .collect()
    }
}
