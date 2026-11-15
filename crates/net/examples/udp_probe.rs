//! Where does receive lateness come from on this PC? Sends gachi-sized UDP packets to
//! itself over loopback (no network card, no audio driver) the way gachi does: a sender at
//! one packet per 0.667 ms (two per 1.33 ms "callback"), a receiver thread in MMCSS "Pro
//! Audio" blocking in recv. Prints every 2 s how late packets arrived compared to when they
//! were sent (the same "spread" gachi logs).
//!
//! cargo run -p gachi-net --release --example udp_probe [seconds]
//!
//! Spikes here too (e.g. only while another program runs) → the cause is how this PC
//! schedules threads / handles interrupts, not the network card or gachi's network code.
//! Clean here but not in gachi → the network card / its driver path.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(60);
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let addr = rx.local_addr().unwrap();
    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    let t0 = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));

    let sender = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            gachi_net::prio::pro_audio();
            let period = Duration::from_micros(1333);
            let mut next = Instant::now();
            let mut seq: u64 = 0;
            let mut pkt = [0u8; 236]; // a 32-frame stereo 24-bit packet, sealed
            while !stop.load(Ordering::Relaxed) {
                while Instant::now() < next {
                    std::hint::spin_loop();
                }
                next += period;
                for _ in 0..2 {
                    pkt[..8].copy_from_slice(&seq.to_le_bytes());
                    let sent_us = t0.elapsed().as_micros() as u64;
                    pkt[8..16].copy_from_slice(&sent_us.to_le_bytes());
                    let _ = tx.send_to(&pkt, addr);
                    seq += 1;
                }
            }
        })
    };

    gachi_net::prio::pro_audio();
    let mut buf = [0u8; 2048];
    let end = Instant::now() + Duration::from_secs(secs);
    let mut report = Instant::now() + Duration::from_secs(2);
    let mut lates: Vec<f64> = Vec::with_capacity(4096);
    println!("  time   packets   median    p99     p99.9    max    (late = arrival - send, ms)");
    while Instant::now() < end {
        if let Ok((n, _)) = rx.recv_from(&mut buf) {
            if n >= 16 {
                let sent = u64::from_le_bytes(buf[8..16].try_into().unwrap());
                let now = t0.elapsed().as_micros() as u64;
                lates.push(now.saturating_sub(sent) as f64 / 1000.0);
            }
        }
        if Instant::now() >= report {
            lates.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let q = |p: f64| lates.get(((lates.len().max(1) - 1) as f64 * p) as usize).copied().unwrap_or(0.0);
            let flag = if q(1.0) > 5.0 { "  ← spike" } else { "" };
            println!(
                "{:5.0} s  {:7}  {:6.3}  {:6.3}  {:7.3}  {:7.3}{flag}",
                t0.elapsed().as_secs_f64(),
                lates.len(),
                q(0.5),
                q(0.99),
                q(0.999),
                q(1.0)
            );
            lates.clear();
            report += Duration::from_secs(2);
        }
    }
    stop.store(true, Ordering::Relaxed);
    sender.join().unwrap();
}
