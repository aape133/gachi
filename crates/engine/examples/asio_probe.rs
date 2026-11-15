//! What the ASIO driver offers and what each input channel actually receives.
//!
//! cargo run -p gachi-engine --example asio_probe --features asio [-- seconds]
//!
//! Prints the default/supported configs, then records every input channel for a few
//! seconds and prints its peak and RMS. Exact 0 means the driver delivers digital silence
//! (wrong channel, or the device is not running); a noise floor around -90…-120 dBFS means
//! the converter is alive.

#[cfg(feature = "asio")]
fn main() {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::{Arc, Mutex};

    let secs: f32 = std::env::args().skip(1).find_map(|s| s.parse().ok()).unwrap_or(4.0);
    let host = cpal::host_from_id(cpal::HostId::Asio).expect("ASIO host");
    let dev = host.default_input_device().or_else(|| host.input_devices().ok()?.next()).expect("an ASIO device");
    println!("device: {}", dev.name().unwrap_or_default());
    match dev.default_input_config() {
        Ok(c) => println!("default input : {c:?}"),
        Err(e) => println!("default input : error {e}"),
    }
    match dev.default_output_config() {
        Ok(c) => println!("default output: {c:?}"),
        Err(e) => println!("default output: error {e}"),
    }
    if let Ok(it) = dev.supported_input_configs() {
        for c in it {
            println!("  in  {c:?}");
        }
    }
    if let Ok(it) = dev.supported_output_configs() {
        for c in it.take(3) {
            println!("  out {c:?}");
        }
    }

    let def = dev.default_input_config().expect("input config");
    let ch = def.channels() as usize;
    let fmt = def.sample_format();
    let mut cfg = def.config();
    // --fixed=N: ask for a buffer size like the engine does
    cfg.buffer_size = std::env::args()
        .find_map(|a| a.strip_prefix("--fixed=").and_then(|n| n.parse().ok()))
        .map_or(cpal::BufferSize::Default, cpal::BufferSize::Fixed);
    println!("\nopening input: {cfg:?} {fmt:?}");

    #[derive(Default)]
    struct Acc {
        peak: Vec<f32>,
        sq: Vec<f64>,
        nonzero: Vec<u64>,
        frames: u64,
        calls: u64,
        min_len: usize,
        max_len: usize,
    }
    let acc = Arc::new(Mutex::new(Acc {
        peak: vec![0.0; ch],
        sq: vec![0.0; ch],
        nonzero: vec![0; ch],
        min_len: usize::MAX,
        ..Default::default()
    }));

    fn feed(acc: &Mutex<Acc>, ch: usize, data: &[f32]) {
        let mut a = acc.lock().unwrap();
        a.calls += 1;
        a.min_len = a.min_len.min(data.len() / ch);
        a.max_len = a.max_len.max(data.len() / ch);
        for f in data.chunks_exact(ch) {
            for (c, &x) in f.iter().enumerate() {
                a.peak[c] = a.peak[c].max(x.abs());
                a.sq[c] += (x as f64) * (x as f64);
                if x != 0.0 {
                    a.nonzero[c] += 1;
                }
            }
            a.frames += 1;
        }
    }

    let err = |e| eprintln!("stream error: {e}");
    let stream = {
        let acc = acc.clone();
        match fmt {
            cpal::SampleFormat::F32 => dev.build_input_stream(&cfg, move |d: &[f32], _: &_| feed(&acc, ch, d), err, None),
            cpal::SampleFormat::I32 => {
                let mut buf = Vec::new();
                dev.build_input_stream(
                    &cfg,
                    move |d: &[i32], _: &_| {
                        buf.clear();
                        buf.extend(d.iter().map(|&s| s as f32 / 2_147_483_648.0));
                        feed(&acc, ch, &buf)
                    },
                    err,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let mut buf = Vec::new();
                dev.build_input_stream(
                    &cfg,
                    move |d: &[i16], _: &_| {
                        buf.clear();
                        buf.extend(d.iter().map(|&s| s as f32 / 32768.0));
                        feed(&acc, ch, &buf)
                    },
                    err,
                    None,
                )
            }
            f => panic!("format {f:?} not handled by the probe"),
        }
        .expect("build input stream")
    };
    // --with-output: also open an output stream, like the engine does
    let out = std::env::args().any(|a| a == "--with-output").then(|| {
        let oc = dev.default_output_config().expect("output config");
        let mut ocfg = oc.config();
        ocfg.buffer_size = cfg.buffer_size;
        println!("also opening output: {ocfg:?} {:?}", oc.sample_format());
        let s = dev
            .build_output_stream(&ocfg, |d: &mut [i32], _: &_| d.fill(0), |e| eprintln!("out error: {e}"), None)
            .expect("build output stream");
        s.play().expect("play out");
        s
    });
    stream.play().expect("play");
    println!("recording {secs} s — make some sound into the inputs now");
    std::thread::sleep(std::time::Duration::from_secs_f32(secs));
    drop(stream);
    drop(out);

    let a = acc.lock().unwrap();
    let db = |x: f64| if x > 1e-12 { 20.0 * x.log10() } else { f64::NEG_INFINITY };
    println!(
        "\ncallbacks {} · frames {} ({:.2} s at {} Hz) · frames per callback {}..{}",
        a.calls,
        a.frames,
        a.frames as f64 / cfg.sample_rate.0 as f64,
        cfg.sample_rate.0,
        a.min_len,
        a.max_len
    );
    for c in 0..ch {
        let rms = (a.sq[c] / a.frames.max(1) as f64).sqrt();
        println!(
            "  in {:>2}: peak {:>7.1} dBFS  rms {:>7.1} dBFS  non-zero {:>5.1}%",
            c + 1,
            db(a.peak[c] as f64),
            db(rms),
            100.0 * a.nonzero[c] as f64 / a.frames.max(1) as f64
        );
    }
}

#[cfg(not(feature = "asio"))]
fn main() {
    eprintln!("build with --features asio");
}
