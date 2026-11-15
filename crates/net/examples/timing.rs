//! How long gathering our addresses takes (what "방 만들기" waits for first).
//!
//! cargo run -p gachi-net --example timing

use gachi_net::{stun, Gathered};
use std::time::Instant;

fn main() {
    let servers: Vec<String> = stun::DEFAULT_SERVERS.iter().map(|s| s.to_string()).collect();
    for i in 1..=3 {
        let t = Instant::now();
        let r = stun::resolve(&servers[0]);
        let dns = t.elapsed();
        let t = Instant::now();
        let g = Gathered::gather(&servers).expect("gather");
        println!(
            "run {i}: dns {:>4} ms · gather {:>4} ms · public found: {}",
            dns.as_millis(),
            t.elapsed().as_millis(),
            g.public.is_some() && r.is_some()
        );
    }
    let t = Instant::now();
    let up = gachi_net::upnp::available();
    println!("UPnP discovery {:>4} ms ({up})", t.elapsed().as_millis());
}
