//! Safety limiter on the output: several loud players (and the metronome) can add up past
//! full scale, and the driver would then hard-clip (harsh distortion). This keeps every
//! sample under the ceiling with no lookahead, so it adds no delay.
//!
//! Below the ceiling it does nothing (gain exactly 1). A peak above it turns the gain down
//! at once for that sample; the gain stays there for `HOLD` seconds after the last such
//! peak and then comes back over `RELEASE` seconds. Both channels share the gain, so the
//! stereo image does not shift.
//!
//! The hold is what keeps a sustained loud passage clean: without it the gain crept back
//! up within every cycle of a low note and each crest was flattened again (a 60 Hz tone
//! 6 dB over: distortion 31 dB under the tone; with the hold only the very first crest is
//! touched). See `examples/quality.rs`.

/// Highest sample level that leaves (≈ -0.3 dBFS).
pub const CEILING: f32 = 0.966;
/// Time for the gain to recover most of the way (s).
const RELEASE: f32 = 0.15;
/// Time the gain stays down after a peak (s): longer than half a cycle of the lowest notes.
const HOLD: f32 = 0.03;
/// A peak this close to the ceiling (at the gain in use) starts the hold again.
const NEAR: f32 = 0.9;

pub struct Limiter {
    gain: f32,
    /// per-sample recovery coefficient
    release: f32,
    hold_frames: u32,
    /// frames left before the gain may recover
    hold: u32,
    /// frames it had to turn down since the counter was last read
    pub limited: u32,
}

impl Limiter {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate.max(1) as f32;
        Self { gain: 1.0, release: 1.0 - (-1.0 / (RELEASE * sr)).exp(), hold_frames: (HOLD * sr) as u32, hold: 0, limited: 0 }
    }

    /// Interleaved stereo, in place.
    pub fn process(&mut self, stereo: &mut [f32]) {
        for f in stereo.as_chunks_mut::<2>().0 {
            // a broken input (NaN, inf) must not reach the speakers, nor upset the gain
            if !f[0].is_finite() || !f[1].is_finite() {
                *f = [0.0, 0.0];
                continue;
            }
            // recover towards 1 (once the hold is over), then make sure this frame fits
            if self.hold > 0 {
                self.hold -= 1;
            } else if self.gain < 1.0 {
                self.gain += (1.0 - self.gain) * self.release;
                if self.gain > 0.9999 {
                    self.gain = 1.0;
                }
            }
            let peak = f[0].abs().max(f[1].abs());
            if peak * self.gain > CEILING {
                self.gain = CEILING / peak;
                self.limited = self.limited.saturating_add(1);
            }
            // still near the ceiling at this gain: it is still needed
            if self.gain < 1.0 && peak * self.gain > CEILING * NEAR {
                self.hold = self.hold_frames;
            }
            if self.gain < 1.0 {
                f[0] *= self.gain;
                f[1] *= self.gain;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let s = amp * (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin();
                [s, s]
            })
            .collect()
    }

    #[test]
    fn leaves_normal_levels_untouched() {
        let mut l = Limiter::new(48_000);
        let input = sine(0.9, 4800);
        let mut out = input.clone();
        l.process(&mut out);
        assert_eq!(out, input, "bit for bit");
        assert_eq!(l.limited, 0);
    }

    #[test]
    fn keeps_a_loud_mix_under_the_ceiling() {
        // four players at 0.7 each, in phase: 2.8 peak
        let mut l = Limiter::new(48_000);
        let mut out = sine(2.8, 4800);
        l.process(&mut out);
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak <= CEILING + 1e-6, "peak {peak}");
        assert!(l.limited > 0);
        // and comes back once it is quiet again
        let mut quiet = sine(0.5, 48_000);
        l.process(&mut quiet);
        let tail = &quiet[quiet.len() - 2000..];
        let peak = tail.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01, "gain recovered: {peak}");
    }

    /// A low note held over full scale: only its first crest is flattened, the rest plays
    /// at the lower gain undistorted.
    #[test]
    fn a_held_loud_note_stays_clean() {
        let mut l = Limiter::new(48_000);
        let tone = |i: usize| 2.0 * (std::f32::consts::TAU * 60.0 * i as f32 / 48_000.0).sin();
        let mut out: Vec<f32> = (0..48_000).flat_map(|i| [tone(i), tone(i)]).collect();
        l.process(&mut out);
        // after the first cycles: the same tone, scaled
        let k = CEILING / 2.0;
        let worst = (9600..48_000).map(|i| (out[i * 2] - tone(i) * k).abs()).fold(0f32, f32::max);
        assert!(worst < 0.002, "distorted: off by {worst}");
    }

    #[test]
    fn silences_broken_samples() {
        let mut l = Limiter::new(48_000);
        let mut out = vec![f32::NAN, 0.1, f32::INFINITY, 0.2, 0.3, 0.3];
        l.process(&mut out);
        assert!(out.iter().all(|s| s.is_finite()));
        assert_eq!(&out[4..], &[0.3, 0.3]);
    }
}
