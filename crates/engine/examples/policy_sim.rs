//! Which automatic jitter policy is fast *and* steady? The whole receive chain (the ring
//! from the network thread, arrival measurement, the automatic target with its hold, the
//! real `JitterBuffer` with its concealment) against the links seen in practice, with the
//! policy's knobs open: ring size, how long the target is held before it comes down, how
//! many bad moments per 30 s the buffer leaves to concealment.
//!
//! cargo run -p gachi-engine --release --example policy_sim
//! cargo run -p gachi-engine --release --example policy_sim -- window
//!
//! The second asks how long the automatic target should remember lateness (its window of
//! seconds) and hold a raise, with links that go rough in patches (a busy PC: a build, a
//! game loading), over runs of several minutes. What it showed (2026-11): remembering 60
//! or 90 s changes little at "balanced" and makes "steady" break more (a longer window
//! thins a burst of moments out over more time); holding a raise 20-60 s cuts the gaps of
//! a busy PC only as far as "steady" already does, at more delay, and adds delay on the
//! links that stall now and then. So the window stays 30 s and the hold 5 s; on a busy
//! PC, "steady" is the answer.
//!
//! Per case and policy (6 seeds × 85 s): the delay gachi adds (median and 95th percentile
//! of every played sample), how much of the time something other than the real sound
//! played (concealed or silent), and how often that happened.
//!
//! Mirrors of gachi-net's `StreamTracker`/`everyday` and the app's `auto_target_ms` /
//! `hold_auto` are marked; keep them in step.

use gachi_engine::{Frame, JitterBuffer};

const FS: f64 = 48_000.0;
/// One run (s), unless asked for longer.
const SECS: f64 = 85.0;
/// The test ramp (sample numbers) wraps here: below it f32 holds a quarter sample.
const WRAP: f64 = 4_194_304.0;
/// Real sound moves on by at most this much at once (samples, 200 ms: packets skipped
/// after a stall); a bigger leap is a crossfade across the wrap, not sound.
const LEAP: f64 = 9600.0;
/// Not the real sound for this many samples in a row (0.5 ms) is a gap one can hear.
const GAP_SAMPLES: u64 = 24;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
    fn exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.next()).ln()
    }
}

#[derive(Clone, Copy)]
struct Policy {
    name: &'static str,
    /// packets the ring between the network thread and the output callback holds
    ring: usize,
    /// the target comes down only after this long without needing more (ms)…
    hold_ms: f64,
    /// …then halfway every this often (ms)
    step_ms: f64,
    /// bad moments per 30 s left to concealment
    rare: usize,
    /// no automatic target: this many ms, always (to see what the moving target costs)
    fixed_ms: Option<f64>,
    /// the cost rule instead of the count: ms of delay worth paying, all the time, to be
    /// rid of one bad moment per 30 s
    worth_ms: Option<f64>,
    /// catch up by playing faster (else drop what is over the target)
    scale: bool,
    /// seconds of lateness remembered (gachi-net `SLOTS`)
    slots: usize,
}

/// A link: everyday jitter (exponential mean, ms) and stalls (nothing arrives for a while,
/// then everything at once): mean seconds between them, and their lengths as
/// (share, shortest ms, longest ms).
struct Link {
    name: &'static str,
    jitter: f64,
    every_s: f64,
    lengths: &'static [(f64, f64, f64)],
    patches: Option<Patches>,
}

/// Rough patches on top: one every `every_s` (mean), each `len_s` long, with stalls every
/// `stall_every_ms` (mean) inside, of `lengths` as for [`Link`].
#[derive(Clone, Copy)]
struct Patches {
    every_s: f64,
    len_s: (f64, f64),
    stall_every_ms: f64,
    lengths: &'static [(f64, f64, f64)],
}

/// A stall length from `lengths` as (share, shortest ms, longest ms).
fn pick_len(rng: &mut Rng, lengths: &[(f64, f64, f64)]) -> f64 {
    let mut pick = rng.next();
    for &(share, lo, hi) in lengths {
        if pick < share {
            return lo + rng.next() * (hi - lo);
        }
        pick -= share;
    }
    0.0
}

/// Mirror of gachi-net `StreamTracker` (timing part).
struct Tracker {
    pm: f64,
    slots: usize,
    rare: usize,
    worth_ms: Option<f64>,
    min_transit: Option<f64>,
    last_transit: Option<f64>,
    jitter: f64,
    spread_now: f64,
    spread_prev: f64,
    window_start: f64,
    slot_start: f64,
    slot_max: f64,
    slot_min: f64,
    seconds: Vec<f64>,
    everyday: Option<f64>,
}

impl Tracker {
    fn on_arrival(&mut self, seq: u32, arrival: f64) {
        let transit = arrival - seq as f64 * self.pm;
        let min = self.min_transit.map_or(transit, |m: f64| transit.min(m));
        self.min_transit = Some(min);
        let late = transit - min;
        if arrival - self.window_start >= 2000.0 {
            if let Some(m) = self.min_transit.as_mut() {
                *m += (arrival - self.window_start) * 0.001;
            }
            self.spread_prev = self.spread_now;
            self.spread_now = 0.0;
            self.window_start = arrival;
        }
        self.spread_now = self.spread_now.max(late);
        if arrival - self.slot_start >= 1000.0 {
            if self.slot_start > 0.0 {
                if self.slot_min != f64::MAX && self.slot_min > 3.0 {
                    if let Some(m) = self.min_transit.as_mut() {
                        *m += self.slot_min;
                    }
                }
                self.slot_min = f64::MAX;
                // a stop (over 200 ms late) stays out, as in gachi-net
                if self.slot_max <= 200.0 {
                    self.seconds.push(self.slot_max);
                    if self.seconds.len() > self.slots {
                        self.seconds.remove(0);
                    }
                    self.everyday = match self.worth_ms {
                        Some(w) => worth_covering(&self.seconds, w),
                        None => everyday(&self.seconds, self.rare),
                    };
                }
                self.slot_max = 0.0;
            }
            self.slot_start = arrival;
        }
        self.slot_max = self.slot_max.max(late);
        self.slot_min = self.slot_min.min(late);
        if let Some(prev) = self.last_transit {
            self.jitter += ((transit - prev).abs() - self.jitter) / 16.0;
        }
        self.last_transit = Some(transit);
    }

    fn spread_p(&self) -> f64 {
        self.everyday
            .unwrap_or(self.spread_now.max(self.spread_prev))
    }
}

/// Mirror of gachi-net `everyday`, with the number of rare moments open.
fn everyday(seconds: &[f64], rare: usize) -> Option<f64> {
    if seconds.len() < 2 {
        return None;
    }
    let allowed = (seconds.len() * rare / 30).min(rare);
    let moments = |level: f64| {
        let (mut m, mut run) = (0usize, 0usize);
        for &s in seconds.iter().chain([0.0].iter()) {
            if s > level {
                run += 1;
            } else if run > 0 {
                m += run.div_ceil(3);
                run = 0;
            }
        }
        m
    };
    let mut levels = seconds.to_vec();
    levels.sort_by(|a, b| a.total_cmp(b));
    let level = levels.into_iter().find(|&l| moments(l) <= allowed)?;
    let lately = if seconds.len() >= 4 {
        seconds
            .iter()
            .rev()
            .take(4)
            .fold(f64::MAX, |m, &s| m.min(s))
    } else {
        0.0
    };
    Some(level.max(lately))
}

/// The proposed rule. Each level the buffer could cover costs its own length in delay, all
/// the time; each bad moment it leaves (a run of seconds later than it, one per started 3)
/// costs `worth` ms, counted per 30 s. The cheapest level wins. Lateness no buffer covers
/// (over 40 ms) costs the same at every level, so it never makes the buffer bigger.
fn worth_covering(seconds: &[f64], worth: f64) -> Option<f64> {
    if seconds.len() < 2 {
        return None;
    }
    const CAP: f64 = 40.0;
    let moments = |level: f64| {
        let (mut m, mut run) = (0usize, 0usize);
        for &s in seconds.iter().chain([0.0].iter()) {
            if s > level {
                run += 1;
            } else if run > 0 {
                m += run.div_ceil(3);
                run = 0;
            }
        }
        m
    };
    // a short history counts as if the rest of the 30 s went the same way
    let scale = 30.0 / seconds.len().max(10) as f64;
    let mut levels: Vec<f64> = seconds.iter().copied().filter(|&l| l <= CAP).collect();
    levels.push(seconds.iter().fold(f64::MAX, |m, &s| m.min(s)).min(CAP));
    levels.sort_by(|a, b| a.total_cmp(b));
    let cost = |l: f64| l + worth * moments(l) as f64 * scale;
    let level = levels
        .into_iter()
        .min_by(|a, b| cost(*a).total_cmp(&cost(*b)))?;
    let lately = if seconds.len() >= 4 {
        seconds
            .iter()
            .rev()
            .take(4)
            .fold(f64::MAX, |m, &s| m.min(s))
            .min(CAP)
    } else {
        0.0
    };
    Some(level.max(lately))
}

/// Mirror of app/src/session.rs `auto_target_ms`.
fn auto_target_ms(spread: f64, jitter: f64, pm: f64, out_ms: f64) -> f64 {
    let by_spread = spread + 1.6 * pm + 0.25 * out_ms + 0.15;
    by_spread
        .max(jitter * 2.5)
        .max(out_ms + pm)
        .clamp(pm, 40f64.max(out_ms + 4.0 * pm))
}

fn to_packets(ms: f64, pm: f64) -> f64 {
    ((ms / pm * 4.0).ceil() / 4.0).clamp(1.0, 128.0)
}

struct Outcome {
    median: f64,
    p95: f64,
    /// ms per minute in which something other than the real sound played
    broken_ms: f64,
    /// times per minute the real sound broke off
    breaks: f64,
    /// packets the ring had no room for
    overflowed: u32,
    /// per minute: the buffer ran dry / packets it skipped
    underruns: f64,
    skipped: f64,
    target_ms: f64,
}

fn run(link: &Link, p: Policy, seed: u64, secs: f64) -> Outcome {
    let (bs, br, pf) = (64usize, 64usize, 32usize);
    let mut rng = Rng(seed);
    let drift = 1.0 + 60e-6;
    let ts = bs as f64 / FS * 1000.0 / drift;
    let tr = br as f64 / FS * 1000.0;
    let pm = pf as f64 / FS * 1000.0 / drift;
    // stalls: (start ms, length ms)
    let mut stalls: Vec<(f64, f64)> = Vec::new();
    if link.every_s > 0.0 {
        let mut t = 3000.0;
        while t < secs * 1000.0 {
            t += rng.exp(link.every_s * 1000.0).max(60.0);
            let len = pick_len(&mut rng, link.lengths);
            stalls.push((t, len));
            t += len;
        }
    }
    if let Some(pt) = link.patches {
        let mut t = 3000.0;
        loop {
            t += rng.exp(pt.every_s * 1000.0);
            if t >= secs * 1000.0 {
                break;
            }
            let end = t + (pt.len_s.0 + rng.next() * (pt.len_s.1 - pt.len_s.0)) * 1000.0;
            let mut u = t;
            loop {
                u += rng.exp(pt.stall_every_ms).max(20.0);
                if u >= end {
                    break;
                }
                let len = pick_len(&mut rng, pt.lengths);
                stalls.push((u, len));
                u += len;
            }
            t = end;
        }
        // one list in time order, none inside another
        stalls.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut kept: Vec<(f64, f64)> = Vec::new();
        for (s, l) in stalls {
            if kept.last().is_none_or(|&(ps, pl)| s >= ps + pl) {
                kept.push((s, l));
            }
        }
        stalls = kept;
    }
    let mut stall_i = 0;
    let mut flight: Vec<(f64, u32)> = Vec::new();
    let (mut seq, mut next_send) = (0u32, 0.0f64);
    let mut jb = JitterBuffer::new(1);
    jb.set_time_scaling(p.scale);
    let mut tracker = Tracker {
        pm,
        slots: p.slots,
        rare: p.rare,
        worth_ms: p.worth_ms,
        min_transit: None,
        last_transit: None,
        jitter: 0.0,
        spread_now: 0.0,
        spread_prev: 0.0,
        window_start: 0.0,
        slot_start: 0.0,
        slot_max: 0.0,
        slot_min: f64::MAX,
        seconds: Vec::new(),
        everyday: None,
    };
    // mirror of `hold_auto` + the settle step in `update_jitter_targets`
    let (mut auto_ms, mut raised_at, mut lowered_at, mut settled) =
        (10.7f64, 0.0f64, 0.0f64, false);
    jb.set_target_f(to_packets(auto_ms, pm));
    let mut out = vec![0f32; br * 2];
    let mut delays: Vec<f32> = Vec::new();
    let (mut broken, mut breaks) = (0u64, 0u32);
    let mut gap = 0u64; // samples in a row that were not the real sound
    let mut top = 0.0f64; // highest real sample played so far
    let mut next_ctl = 500.0;
    let mut overflowed = 0u32;
    let (mut target_sum, mut target_n) = (0.0f64, 0u32);
    let mut now = 5.0;
    while now < secs * 1000.0 {
        while next_send <= now {
            let t = next_send + rng.next() * 0.15;
            for _ in 0..bs / pf {
                let mut at = t + 0.1 + rng.exp(link.jitter);
                while stall_i < stalls.len() && stalls[stall_i].0 + stalls[stall_i].1 < at {
                    stall_i += 1;
                }
                if let Some(&(s, len)) = stalls.get(stall_i) {
                    if at >= s {
                        at = s + len + 0.05 * rng.next();
                    }
                }
                flight.push((at, seq));
                seq += 1;
            }
            next_send += ts;
        }
        flight.sort_by(|a, b| a.0.total_cmp(&b.0));
        let n = flight.iter().take_while(|f| f.0 <= now).count();
        // the ring between the network thread and this callback: what does not fit is lost
        for (k, (arr, s)) in flight.drain(..n).enumerate() {
            tracker.on_arrival(s, arr);
            if k >= p.ring {
                overflowed += 1;
                continue;
            }
            let mut f = Frame {
                seq: s,
                channels: 1,
                frames: pf as u16,
                ..Frame::EMPTY
            };
            for i in 0..pf {
                f.data[i] = ((s as usize * pf + i) as f64 % WRAP) as f32;
            }
            jb.push(f);
        }
        if now >= next_ctl {
            let want = auto_target_ms(tracker.spread_p(), tracker.jitter, pm, tr);
            let now_settled = tracker.seconds.len() >= 2;
            if now_settled && !settled {
                settled = true;
                auto_ms = want;
                raised_at = now;
                lowered_at = now;
            } else if settled {
                if want >= auto_ms {
                    auto_ms = want;
                    raised_at = now;
                } else if now - raised_at >= p.hold_ms && now - lowered_at >= p.step_ms {
                    let next = want + (auto_ms - want) / 2.0;
                    auto_ms = if next - want < 0.25 { want } else { next };
                    lowered_at = now;
                }
            } else if want > auto_ms {
                auto_ms = want;
            }
            if let Some(f) = p.fixed_ms {
                auto_ms = f;
            }
            jb.set_target_f(to_packets(auto_ms, pm));
            next_ctl += 500.0;
            if now > 10_000.0 {
                target_sum += to_packets(auto_ms, pm) * pm;
                target_n += 1;
            }
        }
        jb.read_stereo(&mut out);
        if now > 10_000.0 {
            for (j, ch) in out.as_chunks::<2>().0.iter().enumerate() {
                // the ramp unwrapped next to the highest sample played; around the wrap
                // (±43 ms) what plays is not judged (a crossfade there mixes both ends)
                let raw = ch[0] as f64;
                let v = raw + WRAP * ((top - raw) / WRAP).round();
                let at = top.rem_euclid(WRAP);
                let near_wrap = at < 2048.0 || at > WRAP - 2048.0;
                if near_wrap || v > top + LEAP {
                    // (sound moves on a sample at a time, a packet at most: a crossfade
                    // across the wrap must not carry the top along)
                    if v <= top + if near_wrap { 40.0 } else { LEAP } {
                        top = top.max(v);
                    }
                    continue;
                }
                // real sound moves on; anything made up repeats what was played before
                // (at least a pitch period back)
                // (a gap counts from `GAP_SAMPLES` on: drift correction now and then plays a
                // single sample a fraction behind, which nobody hears)
                let real = v > top - 0.5 && v > 0.0;
                if real {
                    top = top.max(v);
                    delays
                        .push((now + j as f64 / FS * 1000.0 + tr - v / FS * 1000.0 / drift) as f32);
                    if gap >= GAP_SAMPLES {
                        broken += gap;
                        breaks += 1;
                    }
                    gap = 0;
                } else {
                    gap += 1;
                }
            }
        } else {
            for ch in out.as_chunks::<2>().0 {
                let raw = ch[0] as f64;
                let v = raw + WRAP * ((top - raw) / WRAP).round();
                if v <= top + LEAP {
                    top = top.max(v);
                }
            }
        }
        now += tr;
    }
    if gap >= GAP_SAMPLES {
        broken += gap;
        breaks += 1;
    }
    delays.sort_by(|a, b| a.total_cmp(b));
    let minutes = (secs - 10.0) / 60.0;
    Outcome {
        median: delays.get(delays.len() / 2).copied().unwrap_or(f32::NAN) as f64,
        p95: delays
            .get(delays.len() * 95 / 100)
            .copied()
            .unwrap_or(f32::NAN) as f64,
        broken_ms: broken as f64 / FS * 1000.0 / minutes,
        breaks: breaks as f64 / minutes,
        overflowed,
        underruns: jb.stats().underruns as f64 / minutes,
        skipped: jb.stats().skipped as f64 / minutes,
        target_ms: target_sum / target_n.max(1) as f64,
    }
}

fn main() {
    let links = [
        Link {
            name: "wired, clean",
            jitter: 0.03,
            every_s: 0.0,
            lengths: &[],
            patches: None,
        },
        Link {
            name: "wired, a 15 ms hiccup a minute",
            jitter: 0.03,
            every_s: 60.0,
            lengths: &[(1.0, 12.0, 18.0)],
            patches: None,
        },
        // a receiver seen in testing: all packets held for 5-40 ms every few seconds
        Link {
            name: "bursty receiver (stalls every ~3 s)",
            jitter: 0.03,
            every_s: 3.0,
            lengths: &[(0.70, 5.0, 12.0), (0.25, 12.0, 22.0), (0.05, 22.0, 40.0)],
            patches: None,
        },
        Link {
            name: "Wi-Fi (40-120 ms stalls every ~20 s)",
            jitter: 1.0,
            every_s: 20.0,
            lengths: &[(0.7, 30.0, 60.0), (0.3, 60.0, 120.0)],
            patches: None,
        },
        Link {
            name: "busy link (5-9 ms stalls every ~0.7 s)",
            jitter: 0.3,
            every_s: 0.7,
            lengths: &[(1.0, 5.0, 9.0)],
            patches: None,
        },
        Link {
            name: "mostly fine (8-14 ms stall every ~12 s)",
            jitter: 0.03,
            every_s: 12.0,
            lengths: &[(1.0, 8.0, 14.0)],
            patches: None,
        },
    ];
    let window = std::env::args().any(|a| a == "window");
    // a busy PC (seen in a log during a release build): every ~25 s a rough patch of
    // 2-12 s, stalls every ~0.4 s inside it, mostly 5-25 ms, some up to 50
    let build = Link {
        name: "PC busy: build (2-12 s rough patches every ~25 s)",
        jitter: 0.03,
        every_s: 0.0,
        lengths: &[],
        patches: Some(Patches {
            every_s: 25.0,
            len_s: (2.0, 12.0),
            stall_every_ms: 400.0,
            lengths: &[(0.6, 5.0, 12.0), (0.3, 12.0, 25.0), (0.1, 25.0, 50.0)],
        }),
    };
    // lighter: a game or a page loading now and then
    let light = Link {
        name: "PC busy: light (1-3 s rough patches every ~60 s)",
        jitter: 0.03,
        every_s: 0.0,
        lengths: &[],
        patches: Some(Patches {
            every_s: 60.0,
            len_s: (1.0, 3.0),
            stall_every_ms: 600.0,
            lengths: &[(0.8, 4.0, 10.0), (0.2, 10.0, 18.0)],
        }),
    };
    let links: Vec<Link> = if window {
        links.into_iter().chain([light, build]).collect()
    } else {
        links.into_iter().collect()
    };
    let secs = if window { 400.0 } else { SECS };
    let now = Policy {
        name: "now",
        ring: 64,
        hold_ms: 5000.0,
        step_ms: 1000.0,
        rare: 3,
        fixed_ms: None,
        worth_ms: None,
        scale: false,
        slots: 30,
    };
    // the app as it is: ring 256, time-scaling, the cost rule at the chosen leaning
    let app = Policy {
        name: "app",
        ring: 256,
        worth_ms: Some(4.0),
        scale: true,
        ..now
    };
    let windows = [
        Policy {
            name: "balanced, 30 s (now)",
            ..app
        },
        Policy {
            name: "balanced, 60 s",
            slots: 60,
            ..app
        },
        Policy {
            name: "balanced, 90 s",
            slots: 90,
            ..app
        },
        Policy {
            name: "balanced, hold 20 s",
            hold_ms: 20_000.0,
            ..app
        },
        Policy {
            name: "balanced, hold 60 s",
            hold_ms: 60_000.0,
            ..app
        },
        Policy {
            name: "steady, 30 s (now)",
            worth_ms: Some(8.0),
            ..app
        },
        Policy {
            name: "steady, 60 s",
            worth_ms: Some(8.0),
            slots: 60,
            ..app
        },
        Policy {
            name: "steady, 90 s",
            worth_ms: Some(8.0),
            slots: 90,
            ..app
        },
        Policy {
            name: "fast, 30 s (now)",
            worth_ms: Some(2.0),
            ..app
        },
    ];
    let policies = [
        Policy {
            name: "before (count rule, ring 64)",
            ..now
        },
        Policy {
            name: "drop: balanced",
            ring: 256,
            worth_ms: Some(4.0),
            ..now
        },
        Policy {
            name: "SCALE: balanced",
            ring: 256,
            worth_ms: Some(4.0),
            scale: true,
            ..now
        },
        Policy {
            name: "drop: fast",
            ring: 256,
            worth_ms: Some(2.0),
            ..now
        },
        Policy {
            name: "SCALE: fast",
            ring: 256,
            worth_ms: Some(2.0),
            scale: true,
            ..now
        },
        Policy {
            name: "drop: steady",
            ring: 256,
            worth_ms: Some(8.0),
            ..now
        },
        Policy {
            name: "SCALE: steady",
            ring: 256,
            worth_ms: Some(8.0),
            scale: true,
            ..now
        },
        Policy {
            name: "ring 512",
            ring: 512,
            ..now
        },
        Policy {
            name: "ring 512, hold 20 s",
            ring: 512,
            hold_ms: 20_000.0,
            ..now
        },
        Policy {
            name: "ring 512, hold 30 s, step 3 s",
            ring: 512,
            hold_ms: 30_000.0,
            step_ms: 3000.0,
            ..now
        },
        Policy {
            name: "ring 512, rare 6",
            ring: 512,
            rare: 6,
            ..now
        },
        Policy {
            name: "ring 512, rare 10",
            ring: 512,
            rare: 10,
            ..now
        },
        Policy {
            name: "ring 512, rare 0 (cover all)",
            ring: 512,
            rare: 0,
            hold_ms: 30_000.0,
            ..now
        },
        Policy {
            name: "worth 0.5 ms",
            ring: 512,
            worth_ms: Some(0.5),
            ..now
        },
        Policy {
            name: "worth 1 ms",
            ring: 512,
            worth_ms: Some(1.0),
            ..now
        },
        Policy {
            name: "worth 2 ms",
            ring: 512,
            worth_ms: Some(2.0),
            ..now
        },
        Policy {
            name: "worth 4 ms",
            ring: 512,
            worth_ms: Some(4.0),
            ..now
        },
        Policy {
            name: "worth 8 ms",
            ring: 512,
            worth_ms: Some(8.0),
            ..now
        },
        Policy {
            name: "fixed 4 ms",
            ring: 512,
            fixed_ms: Some(4.0),
            ..now
        },
        Policy {
            name: "fixed 8 ms",
            ring: 512,
            fixed_ms: Some(8.0),
            ..now
        },
        Policy {
            name: "fixed 14 ms",
            ring: 512,
            fixed_ms: Some(14.0),
            ..now
        },
        Policy {
            name: "fixed 24 ms",
            ring: 512,
            fixed_ms: Some(24.0),
            ..now
        },
    ];
    let policies: &[Policy] = if window { &windows } else { &policies };
    for link in &links {
        println!("\n{}", link.name);
        println!(
            "  {:<30} {:>7} {:>7} {:>9} {:>11} {:>9} {:>8} {:>9} {:>9}",
            "policy", "median", "p95", "target", "not real", "breaks", "ring", "ran dry", "skipped"
        );
        println!(
            "  {:<30} {:>7} {:>7} {:>9} {:>11} {:>9} {:>8} {:>9} {:>9}",
            "", "ms", "ms", "ms", "ms/min", "/min", "lost", "/min", "pk/min"
        );
        for &p in policies {
            let seeds = [
                0x9E37_79B9_7F4A_7C15u64,
                0xD1B5_4A32_D192_ED03,
                0x8CB9_2BA7_2F3D_8DD7,
                0x2545_F491_4F6C_DD1D,
                0x1234_5678_9ABC_DEF1,
                0xC0FF_EE00_DEAD_BEEF,
            ];
            let runs: Vec<Outcome> = seeds.iter().map(|&s| run(link, p, s, secs)).collect();
            let avg =
                |f: &dyn Fn(&Outcome) -> f64| runs.iter().map(f).sum::<f64>() / runs.len() as f64;
            println!(
                "  {:<30} {:>7.1} {:>7.1} {:>9.1} {:>11.0} {:>9.1} {:>8} {:>9.1} {:>9.0}",
                p.name,
                avg(&|o| o.median),
                avg(&|o| o.p95),
                avg(&|o| o.target_ms),
                avg(&|o| o.broken_ms),
                avg(&|o| o.breaks),
                runs.iter().map(|o| o.overflowed).sum::<u32>(),
                avg(&|o| o.underruns),
                avg(&|o| o.skipped),
            );
        }
    }
}
