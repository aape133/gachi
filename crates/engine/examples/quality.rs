//! Sound quality of what the engine does to the signal, measured.
//!
//! cargo run -p gachi-engine --release --example quality
//!
//! 1. the jitter buffer's resampler: a pure tone read at a fixed ratio (clock drift: a few
//!    ppm off 1; 44.1 ↔ 48 kHz), fitted against the ideal tone: signal to error (dB), and
//!    how much the level wobbles as the read position slides between samples
//! 2. the output limiter: distortion of a tone that goes over full scale
//! 3. 16-bit packets: what rounding does to a quiet tone

use gachi_engine::limiter::Limiter;
use gachi_engine::{Frame, JitterBuffer};
use std::f64::consts::TAU;

/// Least-squares fit of `x` to a sine at `w` rad/sample: (amplitude, signal/error dB).
fn fit(x: &[f64], w: f64) -> (f64, f64) {
    let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (k, &v) in x.iter().enumerate() {
        let (s, c) = (w * k as f64).sin_cos();
        ss += s * s;
        cc += c * c;
        sc += s * c;
        xs += v * s;
        xc += v * c;
    }
    let det = ss * cc - sc * sc;
    let (a, b) = ((xs * cc - xc * sc) / det, (xc * ss - xs * sc) / det);
    let (mut sig, mut err) = (0.0, 0.0);
    for (k, &v) in x.iter().enumerate() {
        let (s, c) = (w * k as f64).sin_cos();
        let m = a * s + b * c;
        sig += m * m;
        err += (v - m) * (v - m);
    }
    ((a * a + b * b).sqrt(), 10.0 * (sig / err.max(1e-300)).log10())
}

/// A tone at `f` Hz sampled at `src` through the jitter buffer read at `ratio`.
fn resampled(f: f64, src: f64, ratio: f64, out_frames: usize) -> Vec<f64> {
    let mut jb = JitterBuffer::new(4);
    jb.set_drift_correction(false);
    jb.set_base_ratio(ratio);
    let pf = 32usize;
    let (mut n, mut seq) = (0usize, 0u32);
    let mut out = Vec::with_capacity(out_frames);
    let mut buf = [0f32; 128];
    while out.len() < out_frames {
        // keep it fed: as many source frames as this block reads
        while (n as f64) < (out.len() + 64) as f64 * ratio + 6.0 * pf as f64 {
            let mut fr = Frame { seq, channels: 1, frames: pf as u16, ..Frame::EMPTY };
            for k in 0..pf {
                fr.data[k] = (0.5 * (TAU * f * (n + k) as f64 / src).sin()) as f32;
            }
            n += pf;
            seq += 1;
            jb.push(fr);
        }
        jb.read_stereo(&mut buf);
        out.extend(buf.iter().step_by(2).map(|&s| s as f64));
    }
    let st = jb.stats();
    assert_eq!(st.underruns + st.concealed + st.skipped + st.scaled, 0, "{st:?}");
    out.split_off(4800)
}

fn main() {
    println!("1. resampler (tone at -6 dBFS): signal/error dB, level wobble dB");
    println!("   {:<22} {:>14} {:>14} {:>14} {:>14} {:>14}", "", "1 kHz", "5 kHz", "10 kHz", "15 kHz", "18 kHz");
    for (name, src, dst, ratio) in [
        ("drift +50 ppm", 48_000.0, 48_000.0, 1.00005),
        ("drift -200 ppm", 48_000.0, 48_000.0, 0.9998),
        ("44.1 -> 48 kHz", 44_100.0, 48_000.0, 44_100.0 / 48_000.0),
        ("48 -> 44.1 kHz", 48_000.0, 44_100.0, 48_000.0 / 44_100.0),
    ] {
        print!("   {name:<22}");
        for f in [1000.0, 5000.0, 10_000.0, 15_000.0, 18_000.0] {
            let x = resampled(f, src, ratio, 48_000 * 4);
            // (the tone's frequency as heard: f × (src/dst) / ratio × ... = f when ratio = src/dst)
            let w = TAU * f / src * ratio;
            let (_, snr) = fit(&x, w);
            // level over 10 ms windows
            let (mut lo, mut hi) = (f64::MAX, 0.0f64);
            for win in x.as_chunks::<480>().0 {
                let a = fit(win, w).0;
                lo = lo.min(a);
                hi = hi.max(a);
            }
            print!(" {snr:>7.1} {:>6.2}", 20.0 * (hi / lo).log10());
        }
        println!();
        let _ = dst;
    }

    println!("2. limiter: tone over full scale, harmonics+noise below the tone (dB)");
    for over_db in [1.0f64, 3.0, 6.0, 12.0] {
        print!("   +{over_db:>4.1} dB:");
        for f in [60.0, 220.0, 1000.0, 5000.0] {
            let mut lim = Limiter::new(48_000);
            let amp = 10f64.powf(over_db / 20.0);
            let mut x: Vec<f32> = (0..48_000 * 2).flat_map(|k| [(amp * (TAU * f * k as f64 / 48_000.0).sin()) as f32; 2]).collect();
            lim.process(&mut x);
            let y: Vec<f64> = x.iter().step_by(2).skip(48_000).map(|&s| s as f64).collect();
            let (a, snr) = fit(&y, TAU * f / 48_000.0);
            let peak = y.iter().fold(0f64, |m, v| m.max(v.abs()));
            print!("  {f:>5.0} Hz {snr:>5.1} (level {:>5.2}, peak {peak:.3})", 20.0 * a.log10());
        }
        println!();
    }

    println!("3. 16-bit packets: 1 kHz tone, error below the tone (dB) and its worst harmonic");
    for level_db in [-20.0f64, -60.0, -80.0, -90.0] {
        let amp = 10f64.powf(level_db / 20.0);
        let n = 48_000 * 4;
        let w = TAU * 1000.0 / 48_000.0;
        let y: Vec<f64> = (0..n).map(|k| (amp * (w * k as f64).sin() * 32767.0).round() / 32768.0).collect();
        let (_, snr) = fit(&y, w);
        // harmonics 2..9 of the tone in the error
        let mut worst = f64::MIN;
        for h in 2..10 {
            let (a, _) = fit(&y, w * h as f64);
            worst = worst.max(20.0 * (a / amp).max(1e-12).log10());
        }
        println!("   {level_db:>5.0} dBFS: {snr:>5.1} dB, worst harmonic {worst:>6.1} dB below... (relative to tone)");
    }
}
