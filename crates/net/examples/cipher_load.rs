//! What the audio encryption costs the audio thread, AES-256-GCM against ChaCha20-Poly1305,
//! on an idle PC and on one whose every core is busy (a build: compiling and linking fill
//! the cores and the memory bus).
//!
//! cargo run -p gachi-net --release --example cipher_load
//!
//! A thread paced like an audio callback (every 1.33 ms: 64 frames at 48 kHz) does one
//! callback's worth of crypto per wake: seals two 32-frame packets to each peer and opens
//! two from each. Per cipher and load: how long that took (p50 / p99 / p99.9 / max, µs),
//! against the callback's 1333 µs, and how late the thread woke (what the load itself
//! does to timing, cipher or not). Under load the thread runs once at normal priority and
//! once in MMCSS "Pro Audio" (the app's receiver; a driver's audio thread runs about as
//! high).

use gachi_net::crypto::{Cipher, Identity, PeerCrypto};
use gachi_proto::audio::{self, Packet, SampleFormat, MAX_PACKET, MAX_SEALED};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PERIOD: Duration = Duration::from_micros(1333);
const SECONDS: u64 = 20;
const PEERS: usize = 4;

/// A packet from a peer, sealed: its counter, header and body.
type Sealed = (u64, Vec<u8>, Vec<u8>);

/// Busy work on every core, like a build: big buffers walked and rewritten (memory bus,
/// caches) with arithmetic between.
fn load(stop: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    let n = std::thread::available_parallelism().map_or(8, |n| n.get());
    (0..n)
        .map(|i| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut v = vec![0u64; 8 << 20]; // 64 MB each
                let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ i as u64;
                while !stop.load(Relaxed) {
                    for k in (0..v.len()).step_by(7) {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        v[k] = v[k].wrapping_add(x);
                    }
                    std::hint::black_box(&v);
                }
            })
        })
        .collect()
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p) as usize]
}

/// Pairs for `PEERS` peers: (ours to them, theirs to us).
fn pairs(c: Cipher) -> Vec<(PeerCrypto, PeerCrypto)> {
    (0..PEERS as u16)
        .map(|i| {
            let (a, b) = (Identity::generate(), Identity::generate());
            (a.session_using(c, 1, 2 + i, &b.public).unwrap(), b.session_using(c, 2 + i, 1, &a.public).unwrap())
        })
        .collect()
}

fn run(c: Cipher, busy: bool, pro: bool) {
    let stop = Arc::new(AtomicBool::new(false));
    let workers = if busy { load(stop.clone()) } else { Vec::new() };
    std::thread::sleep(Duration::from_millis(500));
    let peers = pairs(c);
    // 24-bit stereo, 32 frames: the packet a 64-frame callback sends twice
    let mut plain = [0u8; MAX_PACKET];
    let n = audio::encode_audio(&mut plain, 1, 0, 2, SampleFormat::Pcm24, &[0.1; 64]);
    let mut wire = [0u8; MAX_SEALED];
    let mut out = [0u8; MAX_SEALED];
    // what each peer sends us, sealed beforehand: (counter, header, body)
    let incoming: Vec<Vec<Sealed>> = peers
        .iter()
        .map(|(_, theirs)| {
            (0..2)
                .map(|_| {
                    let len = theirs.seal(2, &plain[..n], &mut wire);
                    let Ok(Packet::Sealed { counter, aad, body, .. }) = audio::decode(&wire[..len]) else { panic!("decode") };
                    (counter, aad.to_vec(), body.to_vec())
                })
                .collect()
        })
        .collect();
    let (mut work, mut late) = (Vec::new(), Vec::new());
    let end = Instant::now() + Duration::from_secs(SECONDS);
    let mut next = Instant::now() + PERIOD;
    while Instant::now() < end {
        // wait for the next "callback" (sleep most of it, spin the rest)
        let now = Instant::now();
        if next > now + Duration::from_micros(1100) {
            std::thread::sleep(next - now - Duration::from_micros(1000));
        }
        while Instant::now() < next {
            std::hint::spin_loop();
        }
        late.push((Instant::now() - next).as_secs_f64() * 1e6);
        let t = Instant::now();
        for ((ours, _), from_them) in peers.iter().zip(&incoming) {
            for (counter, aad, body) in from_them {
                std::hint::black_box(ours.seal(1, &plain[..n], &mut wire));
                assert!(ours.open(*counter, aad, body, &mut out).is_some());
            }
        }
        work.push(t.elapsed().as_secs_f64() * 1e6);
        next += PERIOD;
        if next < Instant::now() {
            next = Instant::now() + PERIOD; // fell behind: go on from now
        }
    }
    stop.store(true, Relaxed);
    for w in workers {
        let _ = w.join();
    }
    let mut w = work;
    let calls = w.len();
    println!(
        "  {:<18} {:<5} {:<9} {:>6} calls | crypto per callback µs p50 {:>5.1} p99 {:>5.1} p99.9 {:>6.1} max {:>7.1} | woke late µs p50 {:>5.0} p99 {:>6.0} max {:>7.0}",
        c.label(),
        if busy { "busy" } else { "idle" },
        if pro { "Pro Audio" } else { "normal" },
        calls,
        pct(&mut w, 0.5),
        pct(&mut w, 0.99),
        pct(&mut w, 0.999),
        pct(&mut w, 1.0),
        pct(&mut late, 0.5),
        pct(&mut late, 0.99),
        pct(&mut late, 1.0),
    );
}

fn main() {
    println!(
        "{} peers: per callback, 2 packets sealed to each and 2 opened from each; budget 1333 µs; {} cores",
        PEERS,
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    // as the app: no power throttling for this process
    gachi_net::prio::power_throttling(Some(false));
    for (busy, pro) in [(false, false), (true, false), (true, true)] {
        for c in [Cipher::Aes256Gcm, Cipher::ChaCha20Poly1305] {
            // the measuring thread: at normal priority, or in MMCSS "Pro Audio" as the
            // app's receiver (a driver's audio thread runs about as high)
            std::thread::spawn(move || {
                if pro {
                    gachi_net::prio::pro_audio();
                }
                run(c, busy, pro);
            })
            .join()
            .unwrap();
        }
    }
}
