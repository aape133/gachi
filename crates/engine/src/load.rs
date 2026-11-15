//! How long the audio callbacks take, and how evenly the driver calls them: the numbers
//! that say whether there is room to lower the buffer, and where the time goes.
//!
//! Written by the audio thread (two clock reads and a few relaxed atomics per callback, no
//! locks), read by whoever logs: [`CallbackLoad::take`] returns the window since the last
//! take and starts a new one.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Instant;

/// Histogram resolution and reach: 2 µs steps up to 4 ms (longer goes in the last step;
/// `max` keeps the exact figure).
const STEP_NS: u64 = 2_000;
const STEPS: usize = 2_000;

fn origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// Nanoseconds since the process's first look at the clock (never 0 once running).
fn now_ns() -> u64 {
    origin().elapsed().as_nanos() as u64 + 1
}

pub struct CallbackLoad {
    hist: Box<[AtomicU32]>,
    max_ns: AtomicU64,
    /// longest time between the starts of two callbacks (the driver's evenness)
    gap_max_ns: AtomicU64,
    gap_min_ns: AtomicU64,
    last_start_ns: AtomicU64,
}

impl Default for CallbackLoad {
    fn default() -> Self {
        origin();
        Self {
            hist: (0..STEPS).map(|_| AtomicU32::new(0)).collect(),
            max_ns: AtomicU64::new(0),
            gap_max_ns: AtomicU64::new(0),
            gap_min_ns: AtomicU64::new(u64::MAX),
            last_start_ns: AtomicU64::new(0),
        }
    }
}

/// One window of callbacks: durations in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LoadWindow {
    pub count: u32,
    pub p50_us: f32,
    pub p99_us: f32,
    pub p999_us: f32,
    pub max_us: f32,
    /// longest gap between two callback starts (0 with fewer than two callbacks)
    pub gap_max_us: f32,
    /// shortest such gap (0 with fewer than two callbacks): with `gap_max_us`, how unevenly
    /// the driver calls (USB interfaces move audio in 1 ms steps)
    pub gap_min_us: f32,
}

impl CallbackLoad {
    /// Call first thing in the callback; hand the result to [`end`](Self::end).
    #[inline]
    pub fn begin(&self) -> u64 {
        let now = now_ns();
        let last = self.last_start_ns.swap(now, Relaxed);
        if last != 0 {
            self.gap_max_ns.fetch_max(now - last, Relaxed);
            self.gap_min_ns.fetch_min(now - last, Relaxed);
        }
        now
    }

    #[inline]
    pub fn end(&self, start: u64) {
        let took = now_ns().saturating_sub(start);
        let step = ((took / STEP_NS) as usize).min(STEPS - 1);
        self.hist[step].fetch_add(1, Relaxed);
        self.max_ns.fetch_max(took, Relaxed);
    }

    /// The callbacks since the last take; starts a new window. One reader at a time.
    pub fn take(&self) -> LoadWindow {
        let mut counts = [0u32; STEPS];
        let mut total = 0u64;
        for (c, h) in counts.iter_mut().zip(self.hist.iter()) {
            *c = h.swap(0, Relaxed);
            total += *c as u64;
        }
        let max_ns = self.max_ns.swap(0, Relaxed);
        let gap_ns = self.gap_max_ns.swap(0, Relaxed);
        let gap_min = match self.gap_min_ns.swap(u64::MAX, Relaxed) {
            u64::MAX => 0.0,
            g => g as f32 / 1000.0,
        };
        if total == 0 {
            return LoadWindow { gap_max_us: gap_ns as f32 / 1000.0, gap_min_us: gap_min, ..LoadWindow::default() };
        }
        let at = |q: f64| {
            let want = ((total as f64 * q).ceil() as u64).max(1);
            let mut seen = 0u64;
            for (i, &c) in counts.iter().enumerate() {
                seen += c as u64;
                if seen >= want {
                    // the step's upper edge, but never past the true maximum
                    return (((i as u64 + 1) * STEP_NS).min(max_ns)) as f32 / 1000.0;
                }
            }
            max_ns as f32 / 1000.0
        };
        LoadWindow {
            count: total.min(u32::MAX as u64) as u32,
            p50_us: at(0.5),
            p99_us: at(0.99),
            p999_us: at(0.999),
            max_us: max_ns as f32 / 1000.0,
            gap_max_us: gap_ns as f32 / 1000.0,
            gap_min_us: gap_min,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_come_from_the_window_and_the_window_restarts() {
        let load = CallbackLoad::default();
        // 990 short calls (~3 µs), 10 long ones (~1 ms): p50 short, p99.9 long
        for k in 0..1000u64 {
            let took = if k % 100 == 99 { 1_000_000 } else { 3_000 };
            let step = ((took / STEP_NS) as usize).min(STEPS - 1);
            load.hist[step].fetch_add(1, Relaxed);
            load.max_ns.fetch_max(took, Relaxed);
        }
        let w = load.take();
        assert_eq!(w.count, 1000);
        assert_eq!(w.p50_us, 4.0, "upper edge of the 2-4 µs step");
        assert_eq!(w.p99_us, 4.0);
        assert_eq!(w.p999_us, 1000.0, "capped at the true maximum");
        assert_eq!(w.max_us, 1000.0);
        assert_eq!(load.take().count, 0, "a new window");
    }

    #[test]
    fn begin_and_end_measure_a_callback_and_the_gap() {
        let load = CallbackLoad::default();
        for _ in 0..3 {
            let t = load.begin();
            std::thread::sleep(std::time::Duration::from_millis(2));
            load.end(t);
        }
        let w = load.take();
        assert_eq!(w.count, 3);
        assert!(w.max_us >= 2000.0 && w.p50_us >= 2000.0, "{w:?}");
        assert!(w.gap_max_us >= 2000.0, "{w:?}");
    }
}
