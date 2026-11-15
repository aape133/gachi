//! Lists audio devices for each host this build supports.
//!
//! cargo run -p gachi-engine --example devices [--features asio]

use gachi_engine::{list_devices, HostChoice, ASIO_AVAILABLE};

fn main() {
    let mut hosts = vec![("Default", HostChoice::Default)];
    if ASIO_AVAILABLE {
        hosts.insert(0, ("ASIO", HostChoice::Asio));
    }
    for (name, host) in hosts {
        println!("== {name}");
        let devs = list_devices(host);
        if devs.is_empty() {
            println!("  (no devices)");
        }
        for d in devs {
            let io = match (d.has_input, d.has_output) {
                (true, true) => "in/out",
                (true, false) => "in",
                _ => "out",
            };
            println!("  [{io:>6}] {}", d.label);
            for (i, n) in d.input_names.iter().enumerate() {
                println!("           in {:>2}: {n}", i + 1);
            }
        }
    }
}
