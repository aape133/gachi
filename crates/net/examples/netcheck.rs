//! What this machine's network offers for P2P (read-only: STUN query and UPnP discovery,
//! no port mapping). Prints whether things work, not your addresses.
//!
//! cargo run -p gachi-net --example netcheck

use gachi_net::stun;

fn main() {
    for server in stun::DEFAULT_SERVERS {
        let found = gachi_net::probe(&[server.to_string()])
            .ok()
            .flatten()
            .map(|(l, p, _)| (l, p));
        println!(
            "STUN {server}: {}",
            if found.is_some() {
                "public address found"
            } else {
                "no answer"
            }
        );
    }
    // both servers from one socket: does the router show us at the same address to both?
    let all: Vec<String> = stun::DEFAULT_SERVERS
        .iter()
        .map(|s| s.to_string())
        .collect();
    match gachi_net::Gathered::gather(&all) {
        Ok(g) => println!(
            "router mapping: {:?}; addresses announced: {}",
            g.mapping,
            g.candidates.len()
        ),
        Err(e) => println!("router mapping: {e}"),
    }
    // (whether there is one, and how long it takes: not the value)
    let t = std::time::Instant::now();
    let key = gachi_net::router_key();
    println!(
        "router value for nearby rooms: {} ({:?}, then cached)",
        if key.is_some() {
            "found"
        } else {
            "none (no router seen)"
        },
        t.elapsed()
    );
    println!("global IPv6: {}", gachi_net::has_ipv6());
    println!("UPnP router: {}", gachi_net::upnp::available());
}
