//! Reproduces the app's order: list ASIO devices on this thread, then open the engine
//! (its streams live on another thread). Then lists again and opens a second time.
//!
//! cargo run -p gachi-engine --example asio_threads --features asio

use gachi_engine::{
    list_devices, Engine, EngineConfig, Frame, HostChoice, InputChannels, PacketSink,
};

struct Nowhere;
impl PacketSink for Nowhere {
    fn send(&mut self, _: &Frame) {}
}

fn open(device: &str) -> Result<String, String> {
    let cfg = EngineConfig {
        host: HostChoice::Asio,
        input_device: Some(device.into()),
        output_device: Some(device.into()),
        sample_rate: 48_000,
        buffer_size: 64,
        packet_frames: 32,
        input: InputChannels::Stereo(0, 1),
    };
    Engine::start(cfg, Box::new(Nowhere))
        .map(|e| format!("{} / {}", e.info.input_device, e.info.output_device))
        .map_err(|e| e.to_string())
}

fn main() {
    let devs = list_devices(HostChoice::Asio);
    println!(
        "listed: {:?}",
        devs.iter().map(|d| &d.id).collect::<Vec<_>>()
    );
    let name = devs.first().map(|d| d.id.clone()).unwrap_or_default();
    println!("open #1: {:?}", open(&name));
    println!(
        "listed again: {:?}",
        list_devices(HostChoice::Asio)
            .iter()
            .map(|d| &d.id)
            .collect::<Vec<_>>()
    );
    println!("open #2: {:?}", open(&name));
}
