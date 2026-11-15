//! What concealment sounds like, measured: the old way (repeat the last packet at half
//! volume, silence when the buffer runs dry) against the pitch-based one, on a few kinds
//! of sound, for single lost packets and for the bursty arrivals seen on a real PC (no
//! packet for ~15 ms, then all of them at once).
//!
//! cargo run -p gachi-engine --release --example plc_quality
//!
//! - single losses: SNR of what plays where the packet was missing (and the crossfade after
//!   it) against the real signal. 0 dB = no better than silence.
//! - bursts: milliseconds that play near-silent (under 10% of the signal's level) and the
//!   biggest jump between neighbouring samples against the signal's own (clicks).

use gachi_engine::{Frame, JitterBuffer};

const FS: f64 = 48_000.0;
const PF: usize = 32;
const READ: usize = 64;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Sample `n` of one of the test sounds.
fn signal(kind: usize, n: usize, noise: &mut Rng) -> f32 {
    let t = n as f64 / FS;
    let saw = |f: f64, h: usize| -> f64 { (1..=h).map(|k| (std::f64::consts::TAU * f * k as f64 * t).sin() / k as f64).sum::<f64>() };
    (match kind {
        // bass / guitar string: 110 Hz, ten harmonics
        0 => 0.3 * saw(110.0, 10),
        // a chord: C4 E4 G4, five harmonics each
        1 => 0.12 * (saw(261.63, 5) + saw(329.63, 5) + saw(392.0, 5)),
        // voice / violin: 220 Hz with 5 Hz vibrato
        2 => {
            let phase = std::f64::consts::TAU * (220.0 * t - 220.0 * 0.01 / (std::f64::consts::TAU * 5.0) * (std::f64::consts::TAU * 5.0 * t).cos());
            0.3 * (1..=6).map(|k| (phase * k as f64).sin() / k as f64).sum::<f64>()
        }
        // cymbals / noise
        3 => 0.3 * (noise.next() * 2.0 - 1.0),
        // a ramp: each sample says where it came from (to measure the buffer's delay)
        _ => n as f64 * 1e-5,
    }) as f32
}

const KINDS: [&str; 4] = ["bass 110 Hz", "chord C-E-G", "vibrato 220 Hz", "noise"];

/// Plays `secs` of sound `kind` through a jitter buffer (target 3 packets, reads of 64);
/// `arrival(seq)` = the read index at which packet `seq` arrives (`None` = lost).
fn play(kind: usize, pitch: bool, secs: f64, arrival: &dyn Fn(u32) -> Option<u64>) -> (Vec<f32>, Vec<f32>) {
    let mut jb = JitterBuffer::new(3);
    jb.set_drift_correction(false);
    jb.set_concealment(pitch);
    let mut noise = Rng(0x1234_5678_9ABC_DEF1);
    let total_packets = (secs * FS / PF as f64) as u32;
    let original: Vec<f32> = (0..total_packets as usize * PF).map(|n| signal(kind, n, &mut noise)).collect();
    let mut pending: Vec<(u64, u32)> = (0..total_packets).filter_map(|s| arrival(s).map(|a| (a, s))).collect();
    pending.sort();
    let mut out = Vec::new();
    let mut buf = vec![0f32; READ * 2];
    let reads = total_packets as u64 * PF as u64 / READ as u64;
    let mut p = 0;
    for r in 0..reads {
        while p < pending.len() && pending[p].0 <= r {
            let s = pending[p].1;
            let mut f = Frame { seq: s, channels: 1, frames: PF as u16, ..Frame::EMPTY };
            f.data[..PF].copy_from_slice(&original[s as usize * PF..(s as usize + 1) * PF]);
            jb.push(f);
            p += 1;
        }
        jb.read_stereo(&mut buf);
        out.extend(buf.iter().step_by(2));
    }
    (original, out)
}

/// The buffer's delay (frames) with these arrivals and nothing lost: a ramp played through
/// it tells (each sample is its own index).
fn delay(arrival: &dyn Fn(u32) -> Option<u64>) -> usize {
    let all = |s: u32| arrival(s).or(Some((s as u64 * PF as u64) / READ as u64));
    let (_, out) = play(4, true, 1.0, &all);
    let i = 20_000;
    i - (out[i] as f64 / 1e-5).round() as usize
}

fn main() {
    println!("single lost packets (one in ~50), SNR where the packet was missing (+ 2 ms after):");
    println!("{:<16} {:>10} {:>10}", "sound", "old", "pitch");
    for (kind, name) in KINDS.iter().enumerate() {
        let mut row = Vec::new();
        for pitch in [false, true] {
            let mut rng = Rng(0xC0FF_EE00 + kind as u64);
            let lost: Vec<bool> = (0..20_000).map(|_| rng.next() < 0.02).collect();
            // steady arrivals two packets per read, one read ahead
            // two packets per read, as they are due; the lost ones never come
            let arrival = |s: u32| (!lost.get(s as usize).copied().unwrap_or(false) || s < 20).then_some((s as u64 * PF as u64) / READ as u64);
            let (orig, out) = play(kind, pitch, 10.0, &arrival);
            let d = delay(&arrival);
            let (mut sig, mut err) = (0f64, 0f64);
            for (s, &l) in lost.iter().enumerate() {
                if !l || s < 20 || (s + 5) * PF + d >= out.len() {
                    continue;
                }
                for k in s * PF..(s * PF + PF + 96) {
                    let (o, y) = (orig[k] as f64, out[k + d] as f64);
                    sig += o * o;
                    err += (y - o) * (y - o);
                }
            }
            row.push(10.0 * (sig / err.max(1e-12)).log10());
        }
        println!("{:<16} {:>8.1} dB {:>7.1} dB", name, row[0], row[1]);
    }

    println!("\nbursts: every 3 s no packet arrives for 15 ms, then all at once (10 s):");
    println!("{:<16} {:>18} {:>18}", "sound", "old: quiet ms / jump", "pitch: quiet ms / jump");
    for (kind, name) in KINDS.iter().enumerate() {
        let mut row = Vec::new();
        for pitch in [false, true] {
            let stall = 15.0 / 1000.0 * FS / READ as f64; // reads
            let every = 3.0 * FS / READ as f64;
            let arrival = |s: u32| {
                let due = (s as u64 * PF as u64) / READ as u64;
                let phase = (due as f64) % every;
                // packets due inside the stall arrive together at its end
                Some(if due as f64 > every && phase < stall { due + (stall - phase).ceil() as u64 } else { due })
            };
            let (orig, out) = play(kind, pitch, 10.0, &arrival);
            // level of the signal, then 1 ms windows of what played
            let level = (orig.iter().map(|x| (x * x) as f64).sum::<f64>() / orig.len() as f64).sqrt();
            let quiet = out[4800..]
                .chunks(48)
                .filter(|w| (w.iter().map(|x| (x * x) as f64).sum::<f64>() / w.len() as f64).sqrt() < 0.1 * level)
                .count();
            let own_jump = orig.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
            let jump = out[4800..].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
            row.push((quiet, jump / own_jump));
        }
        println!("{:<16} {:>8} ms / {:>4.2}× {:>11} ms / {:>4.2}×", name, row[0].0, row[0].1, row[1].0, row[1].1);
    }

    // cost of starting the concealment (the pitch search), on this machine
    let mut jb = JitterBuffer::new(1);
    jb.set_drift_correction(false);
    let mut noise = Rng(1);
    let mut buf = vec![0f32; READ * 2];
    let mut worst = std::time::Duration::ZERO;
    let mut s = 0u32;
    let mut push = |jb: &mut JitterBuffer, s: u32| {
        let mut f = Frame { seq: s, channels: 1, frames: PF as u16, ..Frame::EMPTY };
        for i in 0..PF {
            f.data[i] = signal(0, s as usize * PF + i, &mut noise);
        }
        jb.push(f);
    };
    for _ in 0..200 {
        // a steady stream (two packets per read of 64)...
        for _ in 0..60 {
            push(&mut jb, s);
            push(&mut jb, s + 1);
            s += 2;
            jb.read_stereo(&mut buf);
        }
        // ...then nothing comes for a read: this one runs dry and starts the concealment
        let t = std::time::Instant::now();
        jb.read_stereo(&mut buf);
        jb.read_stereo(&mut buf);
        worst = worst.max(t.elapsed());
        s += 4;
    }
    println!("\nread that starts the concealment (pitch search included): worst {worst:?} of 200");
}
