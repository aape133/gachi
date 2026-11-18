//! Where does receive lateness come from on this PC? Sends gachi-sized UDP packets to
//! itself over loopback (no network card, no audio driver) the way gachi does: a sender at
//! one packet per 0.667 ms (two per 1.33 ms "callback"), a receiver thread in MMCSS "Pro
//! Audio" blocking in recv. Prints every 2 s how late packets arrived compared to when they
//! were sent (the same "spread" gachi logs).
//!
//! cargo run -p gachi-net --release --example udp_probe [seconds] [--load]
//!
//! `--load`: every other 20 s, every core busy at normal priority (big buffers walked and
//! rewritten, as a build does), to see what a busy PC does to the receiver.
//! `--prio mmcss|critical|time`: the receiver in MMCSS "Pro Audio" (as gachi, the default),
//! the same at MMCSS priority "critical", or at plain THREAD_PRIORITY_TIME_CRITICAL.
//!
//! Spikes here too (e.g. only while another program runs) → the cause is how this PC
//! schedules threads / handles interrupts, not the network card or gachi's network code.
//! Clean here but not in gachi → the network card / its driver path.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// While `on`: one thread per core walking a 64 MB buffer (memory bus, caches, every core).
fn load(on: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let n = std::thread::available_parallelism().map_or(8, |n| n.get());
    for i in 0..n {
        let (on, stop) = (on.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut v = vec![0u64; 8 << 20];
            let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ i as u64;
            while !stop.load(Ordering::Relaxed) {
                if !on.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                for k in (0..v.len()).step_by(7) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    v[k] = v[k].wrapping_add(x);
                }
                std::hint::black_box(&v);
            }
        });
    }
}

/// The receiving thread's priority, as `--prio` asks.
fn receiver_priority(how: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, GetCurrentThread, SetThreadPriority, AVRT_PRIORITY_CRITICAL,
            THREAD_PRIORITY_TIME_CRITICAL,
        };
        match how {
            "time" => {
                // SAFETY: the current thread's pseudo-handle is always valid.
                let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
                println!("receiver: THREAD_PRIORITY_TIME_CRITICAL (ok={})", ok != 0);
            }
            "critical" => {
                let task: Vec<u16> = "Pro Audio".encode_utf16().chain(Some(0)).collect();
                let mut index = 0u32;
                // SAFETY: NUL-terminated name and a valid out-pointer; the handle is ours.
                let h = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
                let ok = !h.is_null() && unsafe { AvSetMmThreadPriority(h, AVRT_PRIORITY_CRITICAL) } != 0;
                println!("receiver: MMCSS Pro Audio, priority critical (ok={ok})");
            }
            _ => {
                gachi_net::prio::pro_audio();
                println!("receiver: MMCSS Pro Audio (as gachi)");
            }
        }
    }
    #[cfg(not(windows))]
    let _ = how;
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(60);
    let with_load = std::env::args().any(|a| a == "--load");
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let addr = rx.local_addr().unwrap();
    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    let t0 = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let loading = Arc::new(AtomicBool::new(false));
    if with_load {
        load(loading.clone(), stop.clone());
    }

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

    let args: Vec<String> = std::env::args().collect();
    let prio = args.iter().position(|a| a == "--prio").and_then(|i| args.get(i + 1)).map_or("mmcss", |s| s.as_str());
    receiver_priority(prio);
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
            // (the load goes on and off every 20 s, from 20 s on)
            let phase = with_load && (t0.elapsed().as_secs() / 20) % 2 == 1;
            lates.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let q = |p: f64| lates.get(((lates.len().max(1) - 1) as f64 * p) as usize).copied().unwrap_or(0.0);
            let flag = if q(1.0) > 5.0 { "  ← spike" } else { "" };
            let label = if !with_load { "" } else if loading.load(Ordering::Relaxed) { " [busy]" } else { " [idle]" };
            println!(
                "{:5.0} s  {:7}  {:6.3}  {:6.3}  {:7.3}  {:7.3}{label}{flag}",
                t0.elapsed().as_secs_f64(),
                lates.len(),
                q(0.5),
                q(0.99),
                q(0.999),
                q(1.0)
            );
            lates.clear();
            report += Duration::from_secs(2);
            loading.store(phase, Ordering::Relaxed);
        }
    }
    stop.store(true, Ordering::Relaxed);
    sender.join().unwrap();
}
