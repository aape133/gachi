//! Runs the real audio path with no network: input callback → packets → peer ring →
//! jitter buffer → mixer → output callback. The peer is muted, so nothing is played back.
//!
//! Also records 3 s of tracks (you + the looped-back peer) into the temp folder and, with
//! `--sounds`, plays the test tone and a 120 BPM metronome (turn your volume down).
//!
//! cargo run -p gachi-engine --example loopback --features asio -- [asio|default] [device name] [buffer] [--sounds]

use gachi_engine::{Engine, EngineConfig, Frame, HostChoice, InputChannels, PacketSink};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Loop {
    tx: Arc<Mutex<Option<rtrb::Producer<Frame>>>>,
    sent: Arc<AtomicU32>,
}

impl PacketSink for Loop {
    fn send(&mut self, frame: &Frame) {
        self.sent.fetch_add(1, Ordering::Relaxed);
        // test harness only: try_lock never blocks the audio thread
        if let Ok(mut g) = self.tx.try_lock() {
            if let Some(tx) = g.as_mut() {
                let _ = tx.push(*frame);
            }
        }
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let sounds = args.iter().any(|a| a == "--sounds");
    args.retain(|a| a != "--sounds");
    let host = match args.first().map(String::as_str) {
        Some("default") => HostChoice::Default,
        _ => HostChoice::Asio,
    };
    let device = args.get(1).cloned().filter(|s| !s.is_empty());
    let buffer_size = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(64);

    let tx = Arc::new(Mutex::new(None));
    let sent = Arc::new(AtomicU32::new(0));
    let cfg = EngineConfig {
        host,
        input_device: device.clone(),
        output_device: device,
        sample_rate: 48_000,
        buffer_size,
        packet_frames: 32,
        input: InputChannels::Stereo(0, 1),
    };
    let mut engine = match Engine::start(cfg, Box::new(Loop { tx: tx.clone(), sent: sent.clone() })) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("engine failed: {e}");
            std::process::exit(1);
        }
    };
    println!("stream: {:?}", engine.info);
    println!("local latency (in+out buffers): {:.2} ms", engine.local_latency_ms());

    let (ptx, peer) = engine.add_peer(1);
    peer.params.mute.store(true, Ordering::Relaxed);
    *tx.lock().unwrap() = Some(ptx);

    let rec_dir = std::env::temp_dir().join("gachi-loopback-rec");
    let names = Arc::new(Mutex::new(std::collections::HashMap::from([(0, "me".to_string()), (1, "loop".to_string())])));
    engine.start_recording(rec_dir, names).expect("start recording");
    if sounds {
        engine.play_test_tone(1.0);
        engine.set_metronome(true, 120.0, std::time::SystemTime::now() + Duration::from_millis(1200));
    }

    for sec in 1..=3 {
        std::thread::sleep(Duration::from_secs(1));
        let s = &peer.stats;
        println!(
            "{sec}s  packets/s={}  input peak={:.3?}  peer peak={:.3?}  jitter depth={} target={} underruns={} concealed={}",
            sent.swap(0, Ordering::Relaxed),
            engine.self_meter.take(),
            peer.meter.take(),
            s.depth.load(Ordering::Relaxed),
            s.target.load(Ordering::Relaxed),
            s.underruns.load(Ordering::Relaxed),
            s.concealed.load(Ordering::Relaxed),
        );
        let per_ch: Vec<String> = engine
            .input_meters
            .take(engine.info.input_channels as usize)
            .iter()
            .enumerate()
            .map(|(i, p)| format!("{}:{:.3}", i + 1, p))
            .collect();
        println!("    input channels  {}", per_ch.join("  "));
    }
    engine.set_metronome(false, 120.0, std::time::SystemTime::now());
    match engine.stop_recording() {
        Some(Ok(r)) => println!("recorded {:.2} s, {} files, dropped blocks {} → {}", r.seconds, r.files.len(), r.dropped_blocks, r.dir.display()),
        other => println!("recording: {other:?}"),
    }
}
