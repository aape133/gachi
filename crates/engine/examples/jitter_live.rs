//! The whole automatic jitter chain over time: arrival measurement (as gachi-net's
//! `StreamTracker`), the app's target (`auto_target_ms`, `hold_auto` in app/src/session.rs)
//! and the real `JitterBuffer`, against links whose quality changes while playing.
//!
//! cargo run -p gachi-engine --release --example jitter_live
//!
//! `jitter_sim` finds the smallest fixed target that plays clean; this one shows what the
//! moving target does: gaps (each underrun, skip or deliberate refill is one), and how the
//! real delay follows the target when it rises and when it comes back down.
//!
//! Mirrors of the app's code are marked; keep them in step when that code changes.

use gachi_engine::{Frame, JitterBuffer};

const FS: f64 = 48_000.0;

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

/// A moment of `len` ms in which nothing arrives; what was sent meanwhile arrives at its end.
type Stall = (f64, f64);

/// Mirror of gachi-net `StreamTracker` (timing part) and `everyday`.
struct Tracker {
    pm: f64,
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
    fn new(pm: f64) -> Self {
        Self {
            pm,
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
        }
    }

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
                self.seconds.push(self.slot_max);
                if self.seconds.len() > 30 {
                    self.seconds.remove(0);
                }
                self.slot_max = 0.0;
                self.everyday = everyday(&self.seconds);
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
        self.everyday.unwrap_or(self.spread_now.max(self.spread_prev))
    }

    fn settled(&self) -> bool {
        self.seconds.len() >= 2
    }
}

/// Mirror of gachi-net `everyday`.
fn everyday(seconds: &[f64]) -> Option<f64> {
    if seconds.len() < 2 {
        return None;
    }
    // the cost rule (gachi-net `buffer_level` at WORTH_BALANCED = 4)
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
    let per_30s = 30.0 / seconds.len().max(10) as f64;
    let cost = |l: f64| l + 4.0 * moments(l) as f64 * per_30s;
    let mut level = seconds.iter().fold(f64::MAX, |m, &s| m.min(s)).min(40.0);
    for &l in seconds.iter().filter(|&&l| l <= 40.0) {
        if cost(l) < cost(level) {
            level = l;
        }
    }
    let lately = if seconds.len() >= 4 { seconds.iter().rev().take(4).fold(f64::MAX, |m, &s| m.min(s)).min(40.0) } else { 0.0 };
    Some(level.max(lately))
}

/// Mirror of app/src/session.rs `auto_target_ms` (without the averaged-jitter term when
/// `use_jitter` is off).
fn auto_target_ms(spread: f64, jitter: f64, pm: f64, out_ms: f64, use_jitter: bool) -> f64 {
    let by_spread = spread + 1.6 * pm + 0.25 * out_ms + 0.15;
    let by_jitter = if use_jitter { jitter * 2.5 } else { 0.0 };
    let by_output = out_ms + pm;
    by_spread.max(by_jitter).max(by_output).clamp(pm, 40f64.max(out_ms + 4.0 * pm))
}

fn to_packets(ms: f64, pm: f64) -> f64 {
    ((ms / pm * 4.0).ceil() / 4.0).clamp(1.0, 128.0)
}

/// Mirror of `hold_auto` + the settle step in `update_jitter_targets`.
struct Hold {
    auto_ms: f64,
    raised_at: f64,
    lowered_at: f64,
    settled: bool,
}

impl Hold {
    fn update(&mut self, want: f64, settled: bool, now: f64) {
        if settled && !self.settled {
            self.settled = true;
            self.auto_ms = want;
            self.raised_at = now;
            self.lowered_at = now;
        } else if self.settled {
            if want >= self.auto_ms {
                self.auto_ms = want;
                self.raised_at = now;
            } else if now - self.raised_at >= 5000.0 && now - self.lowered_at >= 1000.0 {
                let next = want + (self.auto_ms - want) / 2.0;
                self.auto_ms = if next - want < 0.25 { want } else { next };
                self.lowered_at = now;
            }
        } else if want > self.auto_ms {
            self.auto_ms = want;
        }
    }
}

struct Case {
    name: &'static str,
    secs: f64,
    /// mean of the exponential everyday network jitter
    jitter: f64,
    stalls: Vec<Stall>,
}

struct Result {
    /// per second: (target ms, median delay ms, drift correction ppm, buffered packets)
    per_sec: Vec<(f64, f64, f64, f64)>,
    gaps: u32,
    underruns: u32,
    skipped: u32,
    concealed: u32,
    median: f64,
}

fn run(c: &Case, use_jitter: bool, seed: u64) -> Result {
    let (bs, br, pf) = (64usize, 64usize, 32usize);
    let mut rng = Rng(seed);
    let drift = 1.0 + 60e-6;
    let ts = bs as f64 / FS * 1000.0 / drift;
    let tr = br as f64 / FS * 1000.0;
    let pm = pf as f64 / FS * 1000.0 / drift;
    let out_ms = tr;
    let mut flight: Vec<(f64, u32)> = Vec::new();
    let (mut seq, mut next_send) = (0u32, 0.0f64);
    let mut jb = JitterBuffer::new(1);
    let mut tracker = Tracker::new(pm);
    let mut hold = Hold { auto_ms: 10.7, raised_at: 0.0, lowered_at: 0.0, settled: false };
    jb.set_target_f(to_packets(10.7, pm));
    let mut out = vec![0f32; br * 2];
    let mut all: Vec<f64> = Vec::new();
    let mut sec: Vec<f64> = Vec::new();
    let mut per_sec = Vec::new();
    let mut next_ctl = 500.0;
    let mut next_sec = 1000.0;
    let mut gaps = 0u32;
    let mut playing = false;
    let mut now = 5.0;
    while now < c.secs * 1000.0 {
        while next_send <= now {
            let t = next_send + rng.next() * 0.15;
            for _ in 0..bs / pf {
                let mut at = t + 0.1 + rng.exp(c.jitter);
                if let Some(&(s, len)) = c.stalls.iter().find(|&&(s, len)| at >= s * 1000.0 && at < s * 1000.0 + len) {
                    at = s * 1000.0 + len + 0.05 * rng.next();
                }
                flight.push((at, seq));
                seq += 1;
            }
            next_send += ts;
        }
        flight.sort_by(|a, b| a.0.total_cmp(&b.0));
        let n = flight.iter().take_while(|p| p.0 <= now).count();
        for (arr, s) in flight.drain(..n) {
            let mut f = Frame { seq: s, channels: 1, frames: pf as u16, ..Frame::EMPTY };
            for i in 0..pf {
                f.data[i] = (s as usize * pf + i) as f32;
            }
            jb.push(f);
            tracker.on_arrival(s, arr);
        }
        if now >= next_ctl {
            let want = auto_target_ms(tracker.spread_p(), tracker.jitter, pm, out_ms, use_jitter);
            hold.update(want, tracker.settled(), now);
            jb.set_target_f(to_packets(hold.auto_ms, pm));
            next_ctl += 500.0;
        }
        jb.read_stereo(&mut out);
        let mut last = f64::MIN;
        let mut any = false;
        for (j, ch) in out.as_chunks::<2>().0.iter().enumerate() {
            let v = ch[0] as f64;
            if v > 1000.0 && v > last && v.fract().abs() < 0.999 {
                last = v;
                any = true;
                let d = now + j as f64 / FS * 1000.0 + tr - v / FS * 1000.0 / drift;
                sec.push(d);
                all.push(d);
            }
        }
        // a gap: real sound stopped for a callback after it had been playing
        if playing && !any {
            gaps += 1;
        }
        playing = any;
        if now >= next_sec {
            sec.sort_by(|a, b| a.total_cmp(b));
            let st = jb.stats();
            per_sec.push((hold.auto_ms, sec.get(sec.len() / 2).copied().unwrap_or(f64::NAN), st.drift_ppm as f64, st.depth as f64));
            sec.clear();
            next_sec += 1000.0;
        }
        now += tr;
    }
    all.sort_by(|a, b| a.total_cmp(b));
    let st = jb.stats();
    Result {
        per_sec,
        gaps,
        underruns: st.underruns,
        skipped: st.skipped,
        concealed: st.concealed,
        median: all.get(all.len() / 2).copied().unwrap_or(0.0),
    }
}

/// A bad moment as the logs show it: a few stalls within ~1.5 s.
fn cluster(at: f64, len: f64) -> Vec<Stall> {
    vec![(at, len), (at + 0.3, len * 0.6), (at + 0.8, len), (at + 1.4, len * 0.5)]
}

fn main() {
    let use_jitter = !std::env::args().any(|a| a == "--no-jitter-term");
    let cases = [
        Case { name: "wired, clean", secs: 90.0, jitter: 0.03, stalls: vec![] },
        Case {
            name: "Wi-Fi, a 40 ms stall twice a minute",
            secs: 120.0,
            jitter: 0.7,
            stalls: [cluster(25.0, 40.0), cluster(80.0, 35.0)].concat(),
        },
        Case {
            name: "wired, busy receiver (15 ms every ~12 s)",
            secs: 120.0,
            jitter: 0.05,
            stalls: (1..10).flat_map(|k| cluster(k as f64 * 12.0 + 0.4 * k as f64, 15.0)).collect(),
        },
        Case {
            name: "wired, bad 30-40 s (12 ms every 0.5 s), then fine",
            secs: 120.0,
            jitter: 0.05,
            stalls: (0..20).map(|k| (30.0 + k as f64 * 0.5, 12.0)).collect(),
        },
    ];
    /// one second of a run, as `per_sec` holds it
    type Second = (f64, f64, f64, f64);
    for c in &cases {
        let r = run(c, use_jitter, 0x9E3779B97F4A7C15);
        println!(
            "\n{}: gaps {} (underruns {}, skipped {}, concealed {}), median delay {:.1} ms",
            c.name, r.gaps, r.underruns, r.skipped, r.concealed, r.median
        );
        let line = |label: &str, f: &dyn Fn(&Second) -> f64| {
            let v: Vec<String> = r.per_sec.iter().map(|x| format!("{:>3.0}", f(x))).collect();
            println!("  {label:<7}{}", v.join(""));
        };
        line("target", &|x| x.0);
        line("delay", &|x| x.1);
        if std::env::args().any(|a| a == "-v") {
            line("ppm/10", &|x| x.2 / 10.0);
            line("depth", &|x| x.3);
        }
    }
}
