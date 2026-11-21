//! A stand-in for the gachi DAW plugin, to try DAW mode before the real plugin exists.
//!
//! cargo run -p gachi-bridge --release --example fake_plugin [seconds] [block]
//!
//! Start gachi in DAW mode (settings → 연결 방식 → DAW 연결), join a room, then run this.
//! It acts like a DAW audio thread at 48 kHz: every `block` frames (default 64) it hands
//! gachi a guitar-like plucked tone (A3, one note a second: what the other players hear
//! from you) and takes back the mix of the other players. It plays nothing itself; every
//! 2 s it prints how loud the mix was, how often it came back within the same block, and
//! the own-sound level gachi asks for.

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}

#[cfg(windows)]
fn main() {
    use gachi_bridge::{Error, PluginEnd, CHANNELS};
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().collect();
    let secs: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60.0);
    let block: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(64);
    let name = gachi_bridge::name();
    let id = std::process::id() as u64;

    let mut end = loop {
        match PluginEnd::open(&name, id) {
            Ok(e) => break e,
            Err(Error::NotRunning) => {
                eprintln!("gachi가 DAW 연결 모드로 실행 중이 아니에요. 기다리는 중…");
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => {
                eprintln!("{e}");
                return;
            }
        }
    };
    end.set_format(48_000, block as u32);
    gachi_bridge::pro_audio();
    println!(
        "연결됨: {block} 프레임 블록 ({:.2} ms), {secs} 초",
        block as f64 / 48.0
    );

    let period = Duration::from_secs_f64(block as f64 / 48_000.0);
    let blocks = (secs * 48_000.0 / block as f64) as u64;
    let mut up = vec![0f32; block * CHANNELS];
    let mut down = vec![0f32; block * CHANNELS];
    let (mut same_block, mut late, mut peak, mut sum_sq, mut n) = (0u32, 0u32, 0f32, 0f64, 0u64);
    let mut frame = 0u64;
    let start = Instant::now();
    let mut next = start;
    let mut report = start + Duration::from_secs(2);
    for _ in 0..blocks {
        // wait for the next "DAW callback" (spin: sleep is too coarse for 1.3 ms)
        while Instant::now() < next {
            std::hint::spin_loop();
        }
        next += period;
        // a plucked A3 every second
        for k in 0..block {
            let t = (frame % 48_000) as f32 / 48_000.0;
            let s = 0.3 * (-6.0 * t).exp() * (std::f32::consts::TAU * 220.0 * t).sin();
            up[k * 2] = s;
            up[k * 2 + 1] = s;
            frame += 1;
        }
        let t = Instant::now();
        end.write_up(&up);
        // the real plugin waits at most a fraction of a block for the mix
        while end.down_available() < block && t.elapsed() < period / 4 {
            std::hint::spin_loop();
        }
        if end.down_available() >= block {
            same_block += 1;
        } else {
            late += 1;
        }
        if end.read_down(&mut down) > 0 {
            for s in &down {
                peak = peak.max(s.abs());
                sum_sq += (*s as f64) * (*s as f64);
                n += 1;
            }
        }
        if Instant::now() >= report {
            let rms = if n > 0 {
                (sum_sq / n as f64).sqrt()
            } else {
                0.0
            };
            let db = |v: f64| {
                if v > 0.0 {
                    format!("{:6.1} dBFS", 20.0 * v.log10())
                } else {
                    "   무음   ".into()
                }
            };
            println!(
                "{:5.0} s  믹스 최대 {} 평균 {}  같은 블록 {same_block} / 늦음 {late}  내 소리 크기 {:.2}  앱 {}",
                start.elapsed().as_secs_f64(),
                db(peak as f64),
                db(rms),
                end.own_gain(),
                if end.app_alive() { "연결" } else { "응답 없음" }
            );
            (same_block, late, peak, sum_sq, n) = (0, 0, 0.0, 0.0, 0);
            report += Duration::from_secs(2);
        }
    }
    println!("끝 (버린 프레임 {})", end.overruns);
}
