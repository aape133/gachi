//! Simulates one peer → us through the real `JitterBuffer`, to see how small the buffer
//! can be before it runs dry, and what that costs in latency.
//!
//! cargo run -p gachi-engine --release --example jitter_sim
//!
//! For each scenario: the smallest target that plays clean on three runs, and what the
//! app's formula (`auto_ms` in app/src/session.rs) asks for. Packet sizes 16/32/64.
//!
//! Model (times in ms):
//! - sender: ASIO callbacks every `bs` frames on its own clock (+drift); each callback's
//!   delivery is late by a little OS jitter; its audio leaves as `bs/32` packets at once
//! - network: base delay + small jitter + rare spikes (or a Wi-Fi-like profile)
//! - receiver: output callbacks every `br` frames; each first takes the packets that have
//!   arrived, then reads `br` frames from the jitter buffer
//!
//! The sent audio is a ramp (sample index), so every played sample tells exactly when it
//! was captured: latency = play time − capture time (both sender input and receiver output
//! buffers included, like on a real setup).

use gachi_engine::{Frame, JitterBuffer};

const FS: f64 = 48_000.0;
const SECONDS: f64 = 60.0;

/// xorshift64*: small, deterministic
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
struct Net {
    base: f64,
    /// mean of the exponential everyday jitter
    jitter: f64,
    /// chance per packet of a spike, and its size range
    spike_p: f64,
    spike: (f64, f64),
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    bs: usize,
    br: usize,
    /// frames per packet (the sender's packet size)
    pf: usize,
    /// OS jitter on the sender's callback delivery (uniform 0..x)
    cb_jitter: f64,
    net: Net,
}

#[allow(dead_code)] // latency figures, for a look in a debugger
struct Outcome {
    underruns: u32,
    concealed: u32,
    mean_ms: f64,
    p99_ms: f64,
    spread_p: f64,
}

fn run(sc: Scenario, target: f64, seed: u64) -> Outcome {
    #[allow(non_snake_case)]
    let PF = sc.pf;
    let mut rng = Rng(seed);
    let drift = 1.0 + 60e-6; // sender's clock runs 60 ppm fast
    let ts = sc.bs as f64 / FS * 1000.0 / drift;
    let tr = sc.br as f64 / FS * 1000.0;
    // packets in flight: (arrival ms, seq, capture start index)
    let mut flight: Vec<(f64, u32)> = Vec::new();
    let mut seq = 0u32;
    let mut next_send = 0.0f64;
    let mut jb = JitterBuffer::new(1);
    jb.set_target_f(target);
    let mut out = vec![0f32; sc.br * 2];
    let mut lat: Vec<f64> = Vec::new();
    // spread: lateness vs the earliest transit, worst per second (as in gachi-net)
    let mut min_transit = f64::MAX;
    let mut slot_max = 0.0f64;
    let mut slot_start = 0.0f64;
    let mut seconds: Vec<f64> = Vec::new();
    let pm = PF as f64 / FS * 1000.0 / drift;

    let mut now = 5.0;
    while now < SECONDS * 1000.0 {
        // sender callbacks up to now
        while next_send <= now {
            let t = next_send + rng.next() * sc.cb_jitter;
            for _ in 0..sc.bs / PF {
                let mut net = sc.net.base + rng.exp(sc.net.jitter);
                if rng.next() < sc.net.spike_p {
                    net += sc.net.spike.0 + rng.next() * (sc.net.spike.1 - sc.net.spike.0);
                }
                flight.push((t + net, seq));
                seq += 1;
            }
            next_send += ts;
        }
        // arrivals up to now, in arrival order
        flight.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let n = flight.iter().take_while(|p| p.0 <= now).count();
        for (arr, s) in flight.drain(..n) {
            let mut f = Frame {
                seq: s,
                channels: 1,
                frames: PF as u16,
                ..Frame::EMPTY
            };
            for i in 0..PF {
                f.data[i] = (s as usize * PF + i) as f32;
            }
            jb.push(f);
            let transit = arr - s as f64 * pm;
            min_transit = min_transit.min(transit);
            slot_max = slot_max.max(transit - min_transit);
            if arr - slot_start >= 1000.0 {
                seconds.push(slot_max);
                slot_max = 0.0;
                slot_start = arr;
            }
        }
        jb.read_stereo(&mut out);
        // latency of each played sample (skip silence/concealment before the stream ran)
        let mut last = f64::MIN;
        for (j, c) in out.as_chunks::<2>().0.iter().enumerate() {
            let v = c[0] as f64;
            // only real, advancing audio (concealment repeats old samples at lower level)
            if v > 1000.0 && v > last && v.fract().abs() < 0.999 {
                last = v;
                let captured = v / FS * 1000.0 / drift;
                lat.push(now + j as f64 / FS * 1000.0 + tr - captured);
            }
        }
        now += tr;
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // what gachi sizes for (gachi-net stats.rs `everyday`): the lowest level at most 3 bad
    // moments in 30 s go beyond (a moment = a run of late seconds, one per 3), the worst
    // 30 s window of the run
    let spread_p = seconds.windows(30).map(everyday).fold(0.0, f64::max);
    let st = jb.stats();
    Outcome {
        underruns: st.underruns,
        concealed: st.concealed,
        mean_ms: lat.get(lat.len() / 2).copied().unwrap_or(0.0), // median
        p99_ms: lat.get(lat.len() * 99 / 100).copied().unwrap_or(0.0),
        spread_p,
    }
}

/// Mirror of `everyday` in crates/net/src/stats.rs (30 measured seconds).
fn everyday(seconds: &[f64]) -> f64 {
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
    let lately = if seconds.len() >= 4 {
        seconds
            .iter()
            .rev()
            .take(4)
            .fold(f64::MAX, |m, &s| m.min(s))
            .min(40.0)
    } else {
        0.0
    };
    level.max(lately)
}

fn main() {
    let wired = Net {
        base: 0.08,
        jitter: 0.03,
        spike_p: 0.002,
        spike: (0.3, 1.2),
    };
    let wifi = Net {
        base: 1.0,
        jitter: 1.2,
        spike_p: 0.01,
        spike: (3.0, 15.0),
    };
    let scenarios = [
        Scenario {
            name: "wired · ASIO 64/64 · packet 32",
            bs: 64,
            br: 64,
            pf: 32,
            cb_jitter: 0.15,
            net: wired,
        },
        Scenario {
            name: "wired · ASIO 32/32 · packet 32",
            bs: 32,
            br: 32,
            pf: 32,
            cb_jitter: 0.15,
            net: wired,
        },
        Scenario {
            name: "wired · ASIO 128/128 · packet 32",
            bs: 128,
            br: 128,
            pf: 32,
            cb_jitter: 0.2,
            net: wired,
        },
        Scenario {
            name: "wired · send 64, play 128 · packet 32",
            bs: 64,
            br: 128,
            pf: 32,
            cb_jitter: 0.15,
            net: wired,
        },
        Scenario {
            name: "Wi-Fi · ASIO 64/64 · packet 32",
            bs: 64,
            br: 64,
            pf: 32,
            cb_jitter: 0.15,
            net: wifi,
        },
        Scenario {
            name: "wired · ASIO 64/64 · packet 16",
            bs: 64,
            br: 64,
            pf: 16,
            cb_jitter: 0.15,
            net: wired,
        },
        Scenario {
            name: "wired · ASIO 32/32 · packet 16",
            bs: 32,
            br: 32,
            pf: 16,
            cb_jitter: 0.15,
            net: wired,
        },
        Scenario {
            name: "wired · ASIO 64/64 · packet 64",
            bs: 64,
            br: 64,
            pf: 64,
            cb_jitter: 0.15,
            net: wired,
        },
        Scenario {
            name: "wired · ASIO 128/128 · packet 64",
            bs: 128,
            br: 128,
            pf: 64,
            cb_jitter: 0.2,
            net: wired,
        },
        Scenario {
            name: "wired · send 64, play 32 · packet 64",
            bs: 64,
            br: 32,
            pf: 64,
            cb_jitter: 0.15,
            net: wired,
        },
        Scenario {
            name: "Wi-Fi · ASIO 64/64 · packet 64",
            bs: 64,
            br: 64,
            pf: 64,
            cb_jitter: 0.15,
            net: wifi,
        },
        Scenario {
            name: "Wi-Fi · ASIO 64/64 · packet 16",
            bs: 64,
            br: 64,
            pf: 16,
            cb_jitter: 0.15,
            net: wifi,
        },
    ];
    println!("scenario                                   clean target       spread_p  packet  output   app asks");
    for sc in scenarios {
        let pm = sc.pf as f64 / FS * 1000.0;
        let out_ms = sc.br as f64 / FS * 1000.0;
        // the smallest target (quarter packets) that plays clean on three different runs
        let mut t = 1.0f64;
        let mut spread = 0.0f64;
        let mut found = None;
        while t <= 128.0 {
            let mut clean = true;
            for seed in [
                0x9E3779B97F4A7C15u64,
                0xD1B54A32D192ED03,
                0x8CB92BA72F3D8DD7,
            ] {
                let o = run(sc, t, seed);
                spread = spread.max(o.spread_p);
                if o.underruns > 0 || o.concealed > 0 {
                    clean = false;
                    break;
                }
            }
            if clean {
                found = Some(t);
                break;
            }
            t += 0.25;
        }
        let t = found.unwrap_or(f64::NAN);
        // the app's formula (app/src/session.rs: auto_ms + to_packets)
        let want_ms = (spread + 1.6 * pm + 0.25 * out_ms + 0.15)
            .max(out_ms + pm)
            .min(40f64.max(out_ms + 4.0 * pm));
        let asks = ((want_ms / pm * 4.0).ceil() / 4.0).min(128.0);
        let verdict = if t.is_nan() {
            "never clean"
        } else if asks >= t {
            "ok"
        } else {
            "TOO SMALL"
        };
        println!(
            "{:<42} {:>6.2} pk = {:>5.2} ms  {:>5.2} ms  {:>4.2} ms  {:>4.2} ms  {:>6.2} pk = {:>5.2} ms  {verdict}",
            sc.name, t, t * pm, spread, pm, out_ms, asks, asks * pm
        );
    }
}
