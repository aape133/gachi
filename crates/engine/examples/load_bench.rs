//! How long the engine's audio processing takes with 1, 4 and 7 others in the room, at the
//! DAW's (or driver's) block sizes: the cost side of "can the buffer go lower?".
//!
//! Runs DAW mode in-process: a stand-in plugin hands blocks over the bridge at the real block
//! rate and times the answer; the others' packets arrive as they would from the network
//! thread. The input side's network send is not in here (a counting sink): see gachi-net's
//! `send_cost` example for that part.
//!
//! cargo run -p gachi-engine --release --example load_bench [seconds per case] [--stress] [--send | --send=copies]
//!
//! --send: the sink takes as long as sending to the room really does (see `send_time`).
//!
//! --stress: 7 others, with the CPU kept busy (normal or audio priority threads), and how
//! often the mix comes back later than the plugin waits for it.

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only (DAW mode)");
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(windows)]
mod imp {
    use gachi_bridge::PluginEnd;
    use gachi_engine::{CallbackLoad, Engine, EngineConfig, Frame, HostChoice, InputChannels, LoadWindow, PacketSink};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const RATE: u32 = 48_000;
    const PACKET: usize = 32;

    /// Counts the packets; with `--send`, takes as long over each as sending it to the
    /// room does (about 5.5 µs a peer, doubled links 9.5: gachi-net's `send_cost`).
    struct Count(Arc<AtomicUsize>, Duration);
    impl PacketSink for Count {
        fn send(&mut self, _: &Frame) {
            self.0.fetch_add(1, Relaxed);
            let t = Instant::now();
            while t.elapsed() < self.1 {
                std::hint::spin_loop();
            }
        }
    }

    /// `--send`: the time a real send takes, per peer and packet; `--send=copies`: with
    /// every link doubled.
    fn send_time(peers: usize) -> Duration {
        let per_peer = match std::env::args().find(|a| a.starts_with("--send")).as_deref() {
            None => 0.0,
            Some("--send=copies") => 9.5,
            Some(_) => 5.5,
        };
        Duration::from_secs_f64(per_peer * peers as f64 * 1e-6)
    }

    fn show(w: &LoadWindow) -> String {
        format!("{:>5.0} {:>5.0} {:>6.0} {:>6.0}", w.p50_us, w.p99_us, w.p999_us, w.max_us)
    }

    /// Noise, so nothing in the chain can take a shortcut on silence.
    fn noise(seed: &mut u64) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        ((*seed >> 11) as f32 / (1u64 << 53) as f32 - 0.5) * 0.4
    }

    fn case(name: &str, peers: usize, block: usize, secs: f64) {
        std::env::set_var("GACHI_BRIDGE_NAME", name);
        let cfg = EngineConfig {
            host: HostChoice::Daw,
            input_device: None,
            output_device: None,
            sample_rate: RATE,
            buffer_size: block as u32,
            packet_frames: PACKET as u16,
            input: InputChannels::Stereo(0, 1),
        };
        let sent = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::start(cfg, Box::new(Count(sent.clone(), send_time(peers)))).expect("engine");
        let mut plugin = PluginEnd::open(name, 1).expect("bridge");
        plugin.set_format(RATE, block as u32);
        let mut txs: Vec<_> = (0..peers)
            .map(|i| {
                let (tx, h) = engine.add_peer(10 + i as u16);
                h.params.jitter_target.store(300, Relaxed);
                tx
            })
            .collect();

        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut up = vec![0f32; block * 2];
        let mut down = vec![0f32; block * 2];
        let round = CallbackLoad::default();
        let period = Duration::from_secs_f64(block as f64 / RATE as f64);
        let blocks = (secs / period.as_secs_f64()) as u64;
        let (mut seq, mut acc, mut late) = (0u32, 0usize, 0u32);
        // what the plugin waits for the mix before playing the block without it (crates/plugin:
        // a quarter of the block, at most 300 µs): longer and the mix comes a block late
        let wait = (period / 4).min(Duration::from_micros(300));
        let mut over_wait = 0u32;
        let start = Instant::now();
        for b in 0..blocks {
            // the network thread: packets arrive at the rate they are played
            acc += block;
            while acc >= PACKET {
                acc -= PACKET;
                for tx in &mut txs {
                    let mut f = Frame { seq, channels: 2, frames: PACKET as u16, ..Frame::EMPTY };
                    for s in &mut f.data[..PACKET * 2] {
                        *s = noise(&mut seed);
                    }
                    let _ = tx.push(f);
                }
                seq += 1;
            }
            for s in &mut up {
                *s = noise(&mut seed);
            }
            // the plugin: hand the block over and wait for the mix, as the DAW thread does
            let t = round.begin();
            let t0 = Instant::now();
            plugin.write_up(&up);
            let deadline = Instant::now() + period;
            while plugin.down_available() < block && Instant::now() < deadline {
                std::hint::spin_loop();
            }
            round.end(t);
            if b > blocks / 10 && t0.elapsed() > wait {
                over_wait += 1;
            }
            if plugin.read_down(&mut down) < block {
                late += 1;
            }
            // the next block at the DAW's pace
            let next = start + period * (b as u32 + 1);
            while Instant::now() < next {
                std::hint::spin_loop();
            }
            if b == blocks / 10 {
                // drop the warm-up from every window
                round.take();
                engine.input_cb.load.take();
                engine.output_cb.load.take();
            }
        }
        let (r, i, o) = (round.take(), engine.input_cb.load.take(), engine.output_cb.load.take());
        let budget = period.as_secs_f64() * 1e6;
        println!(
            "{peers:>2} others  block {block:>3} ({budget:>4.0} µs) | in {} | out {} | round trip {} | past the plugin's wait ({:.0} µs) {over_wait} | late {late}/{blocks}",
            show(&i),
            show(&o),
            show(&r),
            wait.as_secs_f64() * 1e6,
        );
        assert!(sent.load(Relaxed) > 0);
        drop(engine);
    }

    /// Busy threads walking a big buffer (cache and memory traffic as well as CPU), at
    /// normal priority, or at the "Pro Audio" priority the DAW's own audio threads have.
    fn load(threads: usize, audio: bool, stop: Arc<std::sync::atomic::AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
        (0..threads)
            .map(|i| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    if audio {
                        gachi_bridge::pro_audio();
                    }
                    let mut buf = vec![i as u64; 4 << 20]; // 32 MB each
                    let mut k = i;
                    while !stop.load(Relaxed) {
                        for _ in 0..4096 {
                            k = (k.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) >> 33) % buf.len();
                            buf[k] = buf[k].wrapping_mul(31).wrapping_add(7);
                        }
                    }
                    std::hint::black_box(&buf);
                })
            })
            .collect()
    }

    pub fn main() {
        let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
        if std::env::args().any(|a| a == "--stress") {
            // 7 others, under no load, every logical CPU busy at normal priority, and half of
            // them busy at audio priority (a DAW with heavy plugins)
            gachi_bridge::pro_audio();
            let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
            let name = format!("gachi-load-bench-{}", std::process::id());
            for (label, n, audio) in [("no load", 0, false), ("all CPUs busy, normal priority", cpus, false), ("half the CPUs busy, audio priority", cpus / 2, true), ("all CPUs busy, audio priority", cpus, true)] {
                println!("== {label} ({n} threads)");
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let threads = load(n, audio, stop.clone());
                std::thread::sleep(Duration::from_millis(300));
                for block in [32, 64, 128] {
                    case(&format!("{name}-{label}-{block}").replace(' ', "-").replace(',', ""), 7, block, secs);
                }
                stop.store(true, Relaxed);
                threads.into_iter().for_each(|t| t.join().unwrap());
            }
            return;
        }
        gachi_bridge::pro_audio();
        println!("µs, p50 p99 p99.9 max (engine's own processing; round trip = plugin hands over → mix back)");
        let name = format!("gachi-load-bench-{}", std::process::id());
        for block in [32, 64, 128] {
            for peers in [1, 4, 7, 11, 15] {
                case(&format!("{name}-{block}-{peers}"), peers, block, secs);
            }
        }
    }
}
