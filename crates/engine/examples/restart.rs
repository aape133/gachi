//! Opens and closes the engine several times (like changing device settings in a room)
//! and checks that input still arrives after each reopen.
//!
//! cargo run -p gachi-engine --example restart --features asio -- [asio|default] [rounds]

use gachi_engine::{Engine, EngineConfig, Frame, HostChoice, InputChannels, PacketSink};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct Count {
    packets: AtomicU64,
    nonzero: AtomicU64,
    samples: AtomicU64,
}

struct Sink(Arc<Count>);

impl PacketSink for Sink {
    fn send(&mut self, f: &Frame) {
        let n = f.frames as usize * f.channels as usize;
        let nz = f.data[..n].iter().filter(|x| **x != 0.0).count();
        self.0.packets.fetch_add(1, Ordering::Relaxed);
        self.0.samples.fetch_add(n as u64, Ordering::Relaxed);
        self.0.nonzero.fetch_add(nz as u64, Ordering::Relaxed);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let host = if args.first().map(String::as_str) == Some("default") { HostChoice::Default } else { HostChoice::Asio };
    let rounds: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    for round in 1..=rounds {
        let c = Arc::new(Count::default());
        let cfg = EngineConfig {
            host,
            input_device: None,
            output_device: None,
            sample_rate: 48_000,
            buffer_size: 64,
            packet_frames: 32,
            input: InputChannels::Stereo(0, 1),
        };
        let t = std::time::Instant::now();
        let engine = match Engine::start(cfg, Box::new(Sink(c.clone()))) {
            Ok(e) => e,
            Err(e) => {
                println!("round {round}: start failed: {e}");
                continue;
            }
        };
        let open_ms = t.elapsed().as_millis();
        std::thread::sleep(Duration::from_millis(1500));
        let (p, s, nz) = (c.packets.load(Ordering::Relaxed), c.samples.load(Ordering::Relaxed), c.nonzero.load(Ordering::Relaxed));
        println!(
            "round {round}: open {open_ms} ms · {} {} Hz · packets {p} · non-zero samples {:.1}% · driver: {:?}",
            engine.info.input_device,
            engine.info.sample_rate,
            100.0 * nz as f64 / s.max(1) as f64,
            engine.driver_check()
        );
        drop(engine);
    }
}
