//! The real ASIO driver: how evenly it calls us (the longest gap between two callbacks says
//! how much the system holds audio threads off the CPU) and how long our processing takes on
//! it, with 7 others mixed. Plays silence (master at 0).
//!
//! cargo run -p gachi-engine --release --example asio_load --features asio [-- seconds]

#[cfg(feature = "asio")]
fn main() {
    use gachi_engine::{
        Engine, EngineConfig, Frame, HostChoice, InputChannels, LoadWindow, PacketSink,
    };
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    struct Drop_;
    impl PacketSink for Drop_ {
        fn send(&mut self, _: &Frame) {}
    }

    let secs: u64 = std::env::args()
        .skip(1)
        .find_map(|s| s.parse().ok())
        .unwrap_or(60);
    let cfg = EngineConfig {
        host: HostChoice::Asio,
        input_device: None,
        output_device: None,
        sample_rate: 48_000,
        buffer_size: 64,
        packet_frames: 32,
        input: InputChannels::Stereo(0, 1),
    };
    let mut engine = Engine::start(cfg, Box::new(Drop_)).expect("ASIO engine");
    engine.self_params.master.store(0.0);
    engine.self_params.monitor.store(false, Relaxed);
    let info = engine.info.clone();
    let (inb, outb) = engine.real_buffers();
    {
        use asio_sys::bindings::asio_import as ai;
        let (mut min, mut max, mut pref, mut gran) = (0, 0, 0, 0);
        // SAFETY: a plain query to the driver cpal loaded; out-pointers to locals
        let ok = unsafe { ai::ASIOGetBufferSize(&mut min, &mut max, &mut pref, &mut gran) } == 0;
        let (li, lo) = engine.device_latency();
        println!(
            "driver buffer sizes: {}  reported latency in {li} / out {lo} frames",
            if !ok {
                "(no answer)".to_string()
            } else if gran == -1 {
                format!("{min}..{max}, powers of two only, preferred {pref}")
            } else if gran == 0 {
                format!("{pref} only")
            } else {
                format!("{min}..{max} in steps of {gran}, preferred {pref}")
            }
        );
    }
    println!(
        "{} @ {} Hz, buffers in {inb} / out {outb} frames ({:.0} µs)",
        info.output_device,
        info.sample_rate,
        outb as f64 * 1e6 / info.sample_rate as f64
    );

    // seven others, their packets arriving at the rate they are played
    let stop = Arc::new(AtomicBool::new(false));
    let mut txs: Vec<_> = (0..7u16)
        .map(|i| {
            let (tx, h) = engine.add_peer(10 + i);
            h.params.jitter_target.store(300, Relaxed);
            tx
        })
        .collect();
    let feeder = {
        let stop = stop.clone();
        let rate = info.sample_rate;
        std::thread::spawn(move || {
            let period = Duration::from_secs_f64(32.0 / rate as f64);
            let start = Instant::now();
            let mut seq = 0u32;
            while !stop.load(Relaxed) {
                for tx in &mut txs {
                    let mut f = Frame {
                        seq,
                        channels: 2,
                        frames: 32,
                        ..Frame::EMPTY
                    };
                    f.data[..64]
                        .iter_mut()
                        .enumerate()
                        .for_each(|(i, s)| *s = ((i as f32) * 0.37).sin() * 0.1);
                    let _ = tx.push(f);
                }
                seq += 1;
                let next = start + period * seq;
                while Instant::now() < next {
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
        })
    };

    let show = |w: &LoadWindow| {
        format!(
            "{:>4.0} {:>4.0} {:>5.0} {:>5.0} | gap {:>5.0}..{:<5.0}",
            w.p50_us, w.p99_us, w.p999_us, w.max_us, w.gap_min_us, w.gap_max_us
        )
    };
    // --panel: open the driver's control panel the way the settings screen does
    if std::env::args().any(|a| a == "--panel") {
        let t = Instant::now();
        let r = gachi_engine::open_asio_control_panel();
        println!("control panel: {r:?} after {:?}", t.elapsed());
    }
    std::thread::sleep(Duration::from_secs(2));
    engine.input_cb.load.take();
    engine.output_cb.load.take();
    println!("every 5 s, µs: p50 p99 p99.9 max | longest gap between callbacks");
    let (mut worst_in, mut worst_out) = (0f32, 0f32);
    for _ in 0..secs / 5 {
        std::thread::sleep(Duration::from_secs(5));
        let (i, o) = (engine.input_cb.load.take(), engine.output_cb.load.take());
        worst_in = worst_in.max(i.gap_max_us);
        worst_out = worst_out.max(o.gap_max_us);
        // the driver's latency now (asked again, as the app does every 2 s), and its buffer
        let check = engine.driver_check();
        let (li, lo) = engine.device_latency();
        let (bi, bo) = engine.real_buffers();
        println!(
            "in  {} || out {} || driver latency in {li} / out {lo} frames, buffers {bi}/{bo} ({check:?}, panel open: {})",
            show(&i),
            show(&o),
            gachi_engine::device::asio_panel_open()
        );
    }
    stop.store(true, Relaxed);
    feeder.join().unwrap();
    println!(
        "longest gap: in {worst_in:.0} µs, out {worst_out:.0} µs (one buffer = {:.0} µs)",
        outb as f64 * 1e6 / info.sample_rate as f64
    );
}

#[cfg(not(feature = "asio"))]
fn main() {
    eprintln!("build with --features asio");
}
