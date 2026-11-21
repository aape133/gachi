//! Does the router answer the UPnP / NAT-PMP question every time? Asks only: nothing is
//! mapped or changed on the router.
//!
//! The network check in the settings said "the router cannot do it" now and then on a
//! router that can (and a session logged "no UPnP router found" on the same router that
//! mapped a port a minute later). This asks a number of times and says how long each took.
//!
//! cargo run -p gachi-net --release --example upnp_check [times]

use std::time::Instant;

fn main() {
    let times: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let mut yes = 0;
    for k in 0..times {
        let t = Instant::now();
        let found = gachi_net::upnp::available();
        yes += found as usize;
        println!(
            "{:>2}: {} in {:>5.0} ms",
            k + 1,
            if found {
                "can open a port"
            } else {
                "no answer     "
            },
            t.elapsed().as_secs_f64() * 1e3
        );
        // (a little apart, as someone opening the settings again would be)
        std::thread::sleep(std::time::Duration::from_millis(700));
    }
    println!("{yes}/{times}");
}
