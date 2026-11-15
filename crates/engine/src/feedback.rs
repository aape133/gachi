//! Finds the others' sound coming back in what we send ("되돌아감"): a DAW routing that feeds
//! gachi's output (or a multi-out track) back into the plugin's input, an interface loopback
//! channel picked as input, or speakers a microphone hears. The others then hear themselves
//! again, a little later (and it can howl).
//!
//! What we send is compared, sample by sample, with the others' mix we played (without our
//! own sound or the metronome) at every delay from 0 to `MAX_LAG_MS`. A copy of that mix in
//! our input correlates strongly at one fixed delay; a player playing along does not (no
//! instrument matches a recording sample for sample). Both streams are averaged down to
//! 12 kHz mono and checked once a second, off the audio thread.

/// Both streams are decimated to this rate for the check.
pub const RATE: u32 = 12_000;
/// Longest loop we look for (an acoustic path from speakers is ~3 ms per metre, a DAW or
/// interface loop a few buffers).
const MAX_LAG_MS: u32 = 200;
/// Samples (at `RATE`) compared at each delay: 0.66 s.
const WINDOW: usize = 8000;
/// Correlation that means "this is the others' sound" (after the second differences): a
/// copy at half level under a louder own sound measures ~0.56; two players on the same note
/// a few cents apart ~0.09. It must also repeat at the same delay (see `Verdict`).
const ON: f32 = 0.3;
/// Below this the warning goes away again.
const OFF: f32 = 0.15;
/// Quieter than this (RMS of the sample-to-sample differences, ~-66 dBFS) is too little to
/// tell anything.
const QUIET: f32 = 0.0005;

pub const MAX_LAG: usize = (RATE * MAX_LAG_MS / 1000) as usize;
/// History each check needs, per stream (two more samples for the differences).
pub const HISTORY: usize = WINDOW + MAX_LAG + 2;

/// Best match of `sent` against a delayed `played`: (correlation, delay in samples).
/// `sent` is the last `WINDOW` samples; `played` the `HISTORY` samples ending at the same
/// moment. `None` when the others are too quiet to judge; (0, 0) when they are heard and we
/// send (almost) nothing: nothing of theirs comes back.
pub fn best_match(sent: &[f32], played: &[f32]) -> Option<(f32, usize)> {
    debug_assert!(sent.len() >= WINDOW && played.len() >= HISTORY);
    // second differences, not levels: sustained tones (two players holding the same note,
    // a few cents apart) all but vanish, attacks and noise (which only a copy shares sample
    // for sample) stay
    let diff = |x: &[f32]| x.windows(3).map(|w| w[2] - 2.0 * w[1] + w[0]).collect::<Vec<f32>>();
    let sent = diff(&sent[sent.len() - WINDOW - 2..]);
    let played = diff(&played[played.len() - HISTORY..]);
    let (sent, played) = (&sent[..], &played[..]);
    let energy = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>();
    let e_sent = energy(sent);
    if (e_sent / WINDOW as f32).sqrt() < QUIET {
        // we send silence (stopped playing, muted, the loop unplugged): if the others are
        // playing, their sound plainly does not come back
        let heard = played.chunks(WINDOW).any(|w| (energy(w) / w.len() as f32).sqrt() >= QUIET);
        return heard.then_some((0.0, 0));
    }
    // played[lag..lag + WINDOW] lines up with sent when the loop takes `MAX_LAG - lag` samples
    let mut e_played = energy(&played[..WINDOW]);
    let mut best = (0f32, 0usize);
    let mut any = false;
    for start in 0..=MAX_LAG {
        if start > 0 {
            let out = played[start - 1];
            let inn = played[start + WINDOW - 1];
            e_played = (e_played - out * out + inn * inn).max(0.0);
        }
        if (e_played / WINDOW as f32).sqrt() < QUIET {
            continue;
        }
        any = true;
        let seg = &played[start..start + WINDOW];
        let dot: f32 = sent.iter().zip(seg).map(|(a, b)| a * b).sum();
        let c = dot / (e_sent * e_played).sqrt();
        if c > best.0 {
            best = (c, MAX_LAG - start);
        }
    }
    any.then_some(best)
}

/// Turns the once-a-second matches into an on/off warning: on after two checks in a row
/// match at (about) the same delay; off after three clearly clean ones (under `OFF`), or
/// five in a row under `ON` (a little bleed, e.g. from headphones, sits in between and
/// used to keep it on for good).
#[derive(Default)]
pub struct Verdict {
    pub on: bool,
    last_lag: Option<usize>,
    hits: u32,
    /// checks in a row under `OFF`, and under `ON`
    clean: u32,
    below: u32,
}

impl Verdict {
    pub fn update(&mut self, m: Option<(f32, usize)>) -> bool {
        match m {
            Some((c, lag)) if c >= ON => {
                let same = self.last_lag.is_some_and(|l| l.abs_diff(lag) <= 2);
                self.hits = if same { self.hits + 1 } else { 1 };
                self.last_lag = Some(lag);
                self.clean = 0;
                self.below = 0;
                if self.hits >= 2 {
                    self.on = true;
                }
            }
            // too quiet to tell: keep what we had
            None => {}
            Some((c, _)) => {
                self.hits = 0;
                self.last_lag = None;
                self.below += 1;
                self.clean = if c < OFF { self.clean + 1 } else { 0 };
                if self.clean >= 3 || self.below >= 5 {
                    self.on = false;
                }
            }
        }
        self.on
    }
}

/// Averages interleaved stereo at `rate` down to mono at `RATE` (a crude low-pass is enough:
/// both streams go through the same one). Keeps its partial sum between calls.
pub struct Decimator {
    step: f64,
    acc: f32,
    n: f64,
}

impl Decimator {
    pub fn new(rate: u32) -> Self {
        Self { step: rate as f64 / RATE as f64, acc: 0.0, n: 0.0 }
    }

    /// Feeds one stereo frame; returns a 12 kHz sample when one is complete.
    #[inline]
    pub fn push(&mut self, l: f32, r: f32) -> Option<f32> {
        self.acc += (l + r) * 0.5;
        self.n += 1.0;
        if self.n >= self.step {
            let v = self.acc / self.n as f32;
            self.acc = 0.0;
            self.n -= self.step;
            Some(v)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A player at 12 kHz: a held note (A3, detuned by `cents`, own phase) loud enough to
    /// dominate, plus attack noise on the beat (every 0.25 s). Two players with the same
    /// `cents` and phase would be one recording; real ones differ a little.
    fn music(seed: u64, n: usize, cents: f32) -> Vec<f32> {
        let mut s = seed;
        let w = std::f32::consts::TAU * 220.0 * 2f32.powf(cents / 1200.0) / RATE as f32;
        let phase = seed as f32 * 0.7;
        (0..n)
            .map(|i| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let noise = ((s >> 11) as f32 / (1u64 << 53) as f32 - 0.5) * 0.2;
                let attack = (-((i % 3000) as f32) / 300.0).exp();
                0.4 * (w * i as f32 + phase).sin() + noise * attack
            })
            .collect()
    }

    #[test]
    fn the_others_coming_back_are_found_at_their_delay() {
        let played = music(1, HISTORY + 600, 0.0);
        // the loop: 37 samples (3 ms) later, at half level, over the player's own sound
        let own = music(99, HISTORY + 600, 4.0);
        let sent: Vec<f32> = (0..played.len()).map(|i| own[i] * 0.7 + if i >= 37 { played[i - 37] * 0.5 } else { 0.0 }).collect();
        let (c, lag) = best_match(&sent, &played).unwrap();
        println!("feedback copy: correlation {c:.2} at {lag} samples");
        assert!(c > ON, "correlation {c}");
        assert_eq!(lag, 37);
    }

    #[test]
    fn playing_along_is_not_feedback() {
        // same note on the same beat, a few cents apart (as two real players are)
        let played = music(1, HISTORY, 0.0);
        let sent: Vec<f32> = music(7, HISTORY, 5.0).iter().map(|v| v * 1.3).collect();
        let (c, _) = best_match(&sent, &played).unwrap();
        println!("playing along: correlation {c:.2}");
        assert!(c < OFF, "correlation {c}");
    }

    #[test]
    fn silence_tells_nothing() {
        let played = music(1, HISTORY, 0.0);
        assert!(best_match(&played, &vec![0.0; HISTORY]).is_none(), "the others silent");
        assert!(best_match(&vec![0.0; HISTORY], &vec![0.0; HISTORY]).is_none());
    }

    /// We stopped sending (or the loop is unplugged) while the others play: a clean check,
    /// so a warning from before can clear.
    #[test]
    fn sending_nothing_while_they_play_is_clean() {
        let played = music(1, HISTORY, 0.0);
        assert_eq!(best_match(&vec![0.0; HISTORY], &played), Some((0.0, 0)));
    }

    #[test]
    fn a_little_bleed_clears_after_five_seconds() {
        let mut v = Verdict::default();
        v.update(Some((0.8, 40)));
        assert!(v.update(Some((0.8, 40))));
        for k in 0..4 {
            assert!(v.update(Some((0.2, 40))), "still on after {} weak checks", k + 1);
        }
        assert!(!v.update(Some((0.2, 40))), "five under ON clear it");
    }

    #[test]
    fn the_warning_needs_two_checks_and_clears_after_three() {
        let mut v = Verdict::default();
        assert!(!v.update(Some((0.8, 37))), "one check is not enough");
        assert!(v.update(Some((0.8, 38))), "again at the same delay");
        assert!(v.update(None), "silence keeps it");
        assert!(v.update(Some((0.1, 5))));
        assert!(v.update(Some((0.1, 5))));
        assert!(!v.update(Some((0.1, 5))), "three clean checks clear it");
        // matches at changing delays (a coincidence) never turn it on
        let mut v = Verdict::default();
        for lag in [10, 400, 900, 1500] {
            assert!(!v.update(Some((0.6, lag))));
        }
    }

    #[test]
    fn decimation_keeps_the_rate() {
        let mut d = Decimator::new(48_000);
        let n = (0..48_000).filter(|_| d.push(0.5, 0.5).is_some()).count();
        assert_eq!(n, 12_000);
        let mut d = Decimator::new(44_100);
        let n = (0..44_100).filter(|_| d.push(0.5, 0.5).is_some()).count();
        assert!((11_999..=12_001).contains(&n), "{n}");
    }
}
