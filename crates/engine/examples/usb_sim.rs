//! Buffer and packet sizes on USB audio interfaces: does a buffer that fits the USB's 1 ms
//! steps (48 frames at 48 kHz) beat the usual 64 once the uneven callbacks are counted?
//!
//! cargo run -p gachi-engine --release --example usb_sim [-- seeds]
//!
//! Measured on a USB audio interface at 64 frames (`asio_load`): callbacks come 0.95 to 2.03 ms
//! apart, never in between: the driver runs once per 1 ms USB frame and calls us for each
//! buffer that has filled since. Model (ms):
//! - a buffer that fills at time `t` is handed over at the next 1 ms tick after `t` (ticks at
//!   a random phase per run, plus ~20 µs of OS jitter). PCIe/Thunderbolt-like interfaces
//!   ("even") hand it over at `t` plus OS jitter instead.
//! - sender: packets are cut from the stream as in the engine (an accumulator: a 48-frame
//!   buffer with 32-frame packets sends one packet, then two, ...) and leave when the
//!   callback that completes them runs. Wired network as in `jitter_sim`.
//! - receiver: at each output callback, takes what has arrived and reads its buffer from the
//!   real `JitterBuffer`. When that buffer is heard is fixed by the device clock, not by when
//!   the callback ran: buffer k plays from (k + 2) buffers on.
//! - latency per sample = when it is heard − when it was captured: everything gachi adds
//!   (input wait, packet cutting, network, jitter buffer, output buffer). The converters and
//!   the driver's own safety margin come on top and are not in it.
//!
//! For each case: the smallest target that plays clean on every seed, the latency there, and
//! what the app's formula picks (from the spread it would measure) with its latency and
//! whether it plays clean.

use gachi_engine::{Frame, JitterBuffer};

const FS: f64 = 48_000.0;
const SECONDS: f64 = 60.0;
const USB_TICK: f64 = 1.0;

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

#[derive(Clone, Copy, PartialEq)]
enum Bus {
    Usb,
    Even,
}

#[derive(Clone, Copy)]
struct Case {
    bus: Bus,
    /// buffer frames (both machines)
    buf: usize,
    /// packet frames
    pkt: usize,
}

/// When the callback for buffer `k` runs: the buffer has filled at `(k+1)·buf` frames on
/// this machine's clock (`period` ms per frame); USB waits for the next tick.
fn callback_at(bus: Bus, k: u64, buf: usize, frame_ms: f64, phase: f64, os: f64) -> f64 {
    let filled = (k + 1) as f64 * buf as f64 * frame_ms;
    let t = match bus {
        Bus::Usb => phase + ((filled - phase) / USB_TICK).ceil() * USB_TICK,
        Bus::Even => filled,
    };
    t + os
}

struct Outcome {
    bad: u32,
    median: f64,
    spread_p: f64,
}

fn run(c: Case, target: f64, seed: u64) -> Outcome {
    let mut rng = Rng(seed);
    let drift = 1.0 + 60e-6; // the sender's clock runs 60 ppm fast
    let s_frame = 1000.0 / FS / drift;
    let r_frame = 1000.0 / FS;
    let (s_phase, r_phase) = (rng.next() * USB_TICK, rng.next() * USB_TICK);
    let os = |rng: &mut Rng| rng.next() * 0.02;
    let net = |rng: &mut Rng| {
        let mut d = 0.08 + rng.exp(0.03);
        if rng.next() < 0.002 {
            d += 0.3 + rng.next() * 0.9;
        }
        d
    };

    let mut jb = JitterBuffer::new(1);
    jb.set_target_f(target);
    let mut out = vec![0f32; c.buf * 2];
    let mut flight: Vec<(f64, u32)> = Vec::new();
    let (mut sk, mut seq) = (0u64, 0u32);
    let mut next_send = callback_at(c.bus, 0, c.buf, s_frame, s_phase, os(&mut rng));
    let mut lat: Vec<f64> = Vec::new();
    // what gachi-net measures: lateness vs the earliest transit, worst per second
    let (mut min_transit, mut slot_max, mut slot_start) = (f64::MAX, 0.0f64, 0.0f64);
    let mut seconds = Vec::new();

    let mut rk = 0u64;
    loop {
        let now = callback_at(c.bus, rk, c.buf, r_frame, r_phase, os(&mut rng));
        if now > SECONDS * 1000.0 {
            break;
        }
        // the sender's callbacks up to now: every packet its buffer completes leaves
        while next_send <= now {
            let captured = (sk + 1) * c.buf as u64;
            while (seq as u64 + 1) * c.pkt as u64 <= captured {
                flight.push((next_send + net(&mut rng), seq));
                seq += 1;
            }
            sk += 1;
            next_send = callback_at(c.bus, sk, c.buf, s_frame, s_phase, os(&mut rng));
        }
        flight.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let n = flight.iter().take_while(|p| p.0 <= now).count();
        for (arr, s) in flight.drain(..n) {
            let mut f = Frame {
                seq: s,
                channels: 1,
                frames: c.pkt as u16,
                ..Frame::EMPTY
            };
            for i in 0..c.pkt {
                f.data[i] = (s as usize * c.pkt + i) as f32;
            }
            jb.push(f);
            // the earliest a packet could arrive: when its last frame was captured
            let transit = arr - (s as f64 + 1.0) * c.pkt as f64 * s_frame;
            min_transit = min_transit.min(transit);
            slot_max = slot_max.max(transit - min_transit);
            if arr - slot_start >= 1000.0 {
                seconds.push(slot_max);
                slot_max = 0.0;
                slot_start = arr;
            }
        }
        jb.read_stereo(&mut out);
        // heard from (rk + 2) buffers on, by the device clock
        let heard_at = (rk + 2) as f64 * c.buf as f64 * r_frame;
        let mut last = f64::MIN;
        for (j, ch) in out.as_chunks::<2>().0.iter().enumerate() {
            let v = ch[0] as f64;
            if v > 1000.0 && v > last && v.fract().abs() < 0.999 {
                last = v;
                lat.push(heard_at + j as f64 * r_frame - v * s_frame);
            }
        }
        rk += 1;
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let st = jb.stats();
    Outcome {
        bad: st.underruns + st.concealed,
        median: lat.get(lat.len() / 2).copied().unwrap_or(f64::NAN),
        spread_p: seconds.windows(30).map(everyday).fold(0.0, f64::max),
    }
}

/// Mirror of `everyday` in crates/net/src/stats.rs.
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

/// The app's pick (app/src/session.rs `auto_target_ms` + `to_packets`, wired: no jitter term).
fn app_target(spread: f64, pkt_ms: f64, out_ms: f64) -> f64 {
    let want = (spread + 1.6 * pkt_ms + 0.25 * out_ms + 0.15)
        .max(out_ms + pkt_ms)
        .min(40f64.max(out_ms + 4.0 * pkt_ms));
    ((want / pkt_ms * 4.0).ceil() / 4.0).min(128.0)
}

fn main() {
    let n_seeds: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let mut seeds = Vec::new();
    let mut g = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..n_seeds {
        seeds.push((g.next() * u64::MAX as f64) as u64 | 1);
    }
    let cases = [
        (Bus::Usb, 64, 32),
        (Bus::Usb, 64, 64),
        (Bus::Usb, 64, 16),
        (Bus::Usb, 48, 48),
        (Bus::Usb, 48, 24),
        (Bus::Usb, 48, 16),
        (Bus::Usb, 48, 32),
        (Bus::Usb, 32, 32),
        (Bus::Usb, 32, 16),
        (Bus::Usb, 96, 48),
        (Bus::Usb, 96, 32),
        (Bus::Usb, 128, 32),
        (Bus::Even, 64, 32),
        (Bus::Even, 48, 48),
        (Bus::Even, 48, 16),
        (Bus::Even, 32, 32),
    ];
    println!(
        "{n_seeds} seeds × {SECONDS:.0} s, wired network; ms = what gachi adds end to end (median)"
    );
    println!(
        "bus   buf pkt | clean from        latency | spread  app picks         latency  clean"
    );
    for (bus, buf, pkt) in cases {
        let c = Case { bus, buf, pkt };
        let pkt_ms = pkt as f64 / FS * 1000.0;
        let out_ms = buf as f64 / FS * 1000.0;
        let all = |t: f64| -> (bool, f64, f64) {
            let (mut clean, mut med, mut spread) = (true, 0.0, 0.0f64);
            for &s in &seeds {
                let o = run(c, t, s);
                clean &= o.bad == 0;
                med += o.median / seeds.len() as f64;
                spread = spread.max(o.spread_p);
            }
            (clean, med, spread)
        };
        let mut t = 0.5f64;
        let mut found = None;
        let mut spread = 0.0f64;
        while t <= 64.0 {
            let (clean, med, sp) = all(t);
            spread = spread.max(sp);
            if clean {
                found = Some((t, med));
                break;
            }
            t += 0.25;
        }
        let (ct, cmed) = found.unwrap_or((f64::NAN, f64::NAN));
        let at = app_target(spread, pkt_ms, out_ms);
        let (aclean, amed, _) = all(at);
        println!(
            "{:<5} {buf:>3} {pkt:>3} | {ct:>5.2} pk {:>5.2} ms  {cmed:>6.2} | {spread:>5.2}  {at:>5.2} pk {:>5.2} ms  {amed:>6.2}  {}",
            if bus == Bus::Usb { "USB" } else { "even" },
            ct * pkt_ms,
            at * pkt_ms,
            if aclean { "yes" } else { "NO" },
        );
    }
}
