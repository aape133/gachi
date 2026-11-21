//! Input/output channel counts per device (to see where loopback channels live).
//!
//! cargo run -p gachi-engine --example channels --features asio

use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    for id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(id) else {
            continue;
        };
        println!("== {id:?}");
        if let Ok(devs) = host.input_devices() {
            for d in devs {
                let ch = d.default_input_config().map(|c| c.channels()).unwrap_or(0);
                let max = d
                    .supported_input_configs()
                    .map(|it| it.map(|c| c.channels()).max().unwrap_or(0))
                    .unwrap_or(0);
                println!(
                    "  in  {:>2} ch (max {:>2})  {}",
                    ch,
                    max,
                    d.name().unwrap_or_default()
                );
            }
        }
        if let Ok(devs) = host.output_devices() {
            for d in devs {
                let ch = d.default_output_config().map(|c| c.channels()).unwrap_or(0);
                println!(
                    "  out {:>2} ch             {}",
                    ch,
                    d.name().unwrap_or_default()
                );
            }
        }
    }
}
