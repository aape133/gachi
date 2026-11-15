//! What sending costs the input callback, and how long a packet takes from `send` to the
//! peer's jitter-buffer ring, with 1, 4 and 7 others, single and doubled (lossy link) sends.
//!
//! Everything runs in this process over this PC's own addresses: the real `UdpSender`
//! (encode, seal per peer, `send_to`) and the real receive threads (open, check, push). The
//! wire itself is not in it (the loopback path skips the network card and its interrupt
//! moderation), so "arrival" is the software part of the trip only.
//!
//! cargo run -p gachi-net --release --example send_cost [seconds per case]
//!   --crypto   seal/open alone, AES-256-GCM vs ChaCha20-Poly1305
//!   --nic      plain send_to through the network card
//!   --control  the same loop without sending
//!   --eco on|off  Windows power throttling (EcoQoS) forced on / opted out of

use gachi_engine::{Frame, PacketSink};
use gachi_net::crypto::{self, Cipher, Identity};
use gachi_net::{Gathered, Net, NetConfig, NetEvent};
use gachi_proto::audio::SampleFormat;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const PACKET: usize = 32;
const RATE: u32 = 48_000;
const SLOTS: usize = 1 << 16;

/// A local stand-in for a STUN server (answers with the sender's address).
fn spawn_stun() -> String {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    thread::spawn(move || {
        let mut buf = [0u8; 576];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if n >= 20 {
                let txid: [u8; 12] = buf[8..20].try_into().unwrap();
                let _ = sock.send_to(&gachi_net::stun::response(&txid, from), from);
            }
        }
    });
    addr.to_string()
}

fn start(stun: &str, id: u16) -> Net {
    let cfg = NetConfig {
        my_id: id,
        sample_rate: RATE,
        packet_frames: PACKET as u16,
        format: SampleFormat::Pcm24,
        punch_timeout: NetConfig::DEFAULT_PUNCH_TIMEOUT,
        upnp: false,
        peer_timeout: NetConfig::DEFAULT_PEER_TIMEOUT,
    };
    Net::start(Gathered::gather(&[stun.to_string()]).unwrap(), cfg).unwrap()
}

fn pct(v: &mut [u64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 * q).ceil() as usize).clamp(1, v.len()) - 1;
    v[i] as f64 / 1000.0
}

fn show(v: &mut [u64]) -> String {
    format!("{:>5.1} {:>5.1} {:>6.1} {:>7.1}", pct(v, 0.5), pct(v, 0.99), pct(v, 0.999), pct(v, 1.0))
}

/// One case. `cipher` is forced through what each side announces about AES (both say
/// "yes" → AES-256-GCM, else ChaCha20-Poly1305), as two real machines would.
fn case(stun: &str, cipher: Cipher, others: usize, doubled: bool, secs: f64) {
    let aes = cipher == Cipher::Aes256Gcm;
    assert!(!aes || crypto::aes_in_hardware(), "this CPU has no AES in hardware");
    let me = start(stun, 1);
    let origin = Instant::now();
    let sent_at: Arc<Vec<AtomicU64>> = Arc::new((0..SLOTS).map(|_| AtomicU64::new(0)).collect());
    let arrivals = Arc::new(Mutex::new(Vec::<u64>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let mut peers = Vec::new();
    let mut consumers = Vec::new();
    for k in 0..others {
        let id = 2 + k as u16;
        let them = start(stun, id);
        let (tx, _) = rtrb::RingBuffer::<Frame>::new(64);
        me.add_peer(id, tx);
        let (tx, rx) = rtrb::RingBuffer::<Frame>::new(1024);
        them.add_peer(1, tx);
        me.set_candidates(id, them.candidates.clone(), them.token, crypto::PeerKey { aes, ..them.peer_key() });
        them.set_candidates(1, me.candidates.clone(), me.token, crypto::PeerKey { aes, ..me.peer_key() });
        peers.push(them);
        consumers.push(rx);
    }
    let mut route = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut connected = 0;
    while connected < others && Instant::now() < deadline {
        match me.try_event() {
            Some(NetEvent::Connected { addr, .. }) => {
                connected += 1;
                route = Some(addr);
            }
            _ => thread::sleep(Duration::from_millis(2)),
        }
    }
    assert_eq!(connected, others, "every peer connected");
    // let the "most direct path" probing settle
    thread::sleep(Duration::from_millis(3500));
    if doubled {
        for k in 0..others {
            me.set_redundancy(2 + k as u16, true);
        }
        thread::sleep(Duration::from_millis(50));
    }

    // the jitter-buffer side: pop as the output callback would (here: as soon as possible)
    let collector = {
        let (sent_at, arrivals, stop) = (sent_at.clone(), arrivals.clone(), stop.clone());
        thread::spawn(move || {
            gachi_net::prio::pro_audio();
            let mut seen = vec![vec![0u32; SLOTS]; consumers.len()];
            let mut got = Vec::with_capacity(1 << 20);
            while !stop.load(Relaxed) {
                let mut any = false;
                for (rx, seen) in consumers.iter_mut().zip(&mut seen) {
                    while let Ok(f) = rx.pop() {
                        any = true;
                        let slot = f.seq as usize % SLOTS;
                        // the first copy only (doubled sends deliver each packet twice)
                        if seen[slot] == f.seq + 1 {
                            continue;
                        }
                        seen[slot] = f.seq + 1;
                        let now = origin.elapsed().as_nanos() as u64;
                        got.push(now.saturating_sub(sent_at[slot].load(Relaxed)));
                    }
                }
                if !any {
                    std::hint::spin_loop();
                }
            }
            *arrivals.lock().unwrap() = got;
        })
    };

    gachi_net::prio::pro_audio();
    let mut sender = me.sender();
    let period = Duration::from_secs_f64(PACKET as f64 / RATE as f64);
    let n = (secs / period.as_secs_f64()) as u32;
    let mut costs = Vec::with_capacity(n as usize);
    let mut frame = Frame { channels: 2, frames: PACKET as u16, ..Frame::EMPTY };
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let start = Instant::now();
    for seq in 0..n {
        frame.seq = seq;
        for s in &mut frame.data[..PACKET * 2] {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *s = ((seed >> 11) as f32 / (1u64 << 53) as f32 - 0.5) * 0.4;
        }
        let t = origin.elapsed().as_nanos() as u64;
        sent_at[seq as usize % SLOTS].store(t, Relaxed);
        sender.send(&frame);
        let took = origin.elapsed().as_nanos() as u64 - t;
        if seq >= n / 10 {
            costs.push(took);
        }
        let next = start + period * (seq + 1);
        while Instant::now() < next {
            std::hint::spin_loop();
        }
    }
    thread::sleep(Duration::from_millis(100));
    stop.store(true, Relaxed);
    collector.join().unwrap();
    let mut arr = std::mem::take(&mut *arrivals.lock().unwrap());
    let expected = n as usize * others;
    let got = arr.len();
    let warm = got / 10;
    let mut arr = arr.split_off(warm);
    let stalls = costs.iter().filter(|&&c| c > 300_000).count();
    println!(
        "{:<6} {others} others {:<7} | send per packet {} | send → ring {} | sends > 300 µs: {stalls} in {:.0} s | arrived {got}/{expected} via {}",
        if aes { "AES" } else { "ChaCha" },
        if doubled { "doubled" } else { "single" },
        show(&mut costs),
        show(&mut arr),
        costs.len() as f64 * period.as_secs_f64(),
        route.map_or("?".into(), |a| a.ip().to_string()),
    );
    drop(sender);
    drop(me);
    drop(peers);
}

/// The same loop and pace with ~5 µs of plain computing in place of `send`: stalls here are
/// the thread being held off the CPU (other drivers' interrupts and DPCs), not the socket.
fn control(secs: f64) {
    gachi_net::prio::pro_audio();
    let period = Duration::from_secs_f64(PACKET as f64 / RATE as f64);
    let n = (secs / period.as_secs_f64()) as u32;
    let mut costs = Vec::with_capacity(n as usize);
    let start = Instant::now();
    let mut x = 1u64;
    for seq in 0..n {
        let t = Instant::now();
        while t.elapsed() < Duration::from_micros(5) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        let took = t.elapsed().as_nanos() as u64;
        if seq >= n / 10 {
            costs.push(took);
        }
        let next = start + period * (seq + 1);
        while Instant::now() < next {
            std::hint::spin_loop();
        }
    }
    let stalls = costs.iter().filter(|&&c| c > 300_000).count();
    println!(
        "control (5 µs compute, no send) | took {} | > 300 µs: {stalls} in {:.0} s ({x})",
        show(&mut costs),
        costs.len() as f64 * period.as_secs_f64()
    );
}

/// Plain `send_to` through the network card (to the router's discard port, nothing on this
/// PC receives), `copies` packets of a 32-frame 24-bit stereo size per packet time: the
/// socket cost without the loopback path doing the receiver's work inside `send_to`.
fn nic(secs: f64, copies: usize) {
    let gw = netdev::get_default_gateway().expect("a default gateway");
    let ip = *gw.ipv4.first().expect("an IPv4 gateway");
    let to = std::net::SocketAddr::from((ip, 9));
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)).unwrap();
    let _ = sock.set_send_buffer_size(1 << 20);
    let _ = sock.set_tos(46 << 2);
    sock.bind(&std::net::SocketAddr::from(([0, 0, 0, 0], 0)).into()).unwrap();
    let sock: UdpSocket = sock.into();
    gachi_net::prio::pro_audio();
    let buf = [0x5au8; 230];
    let period = Duration::from_secs_f64(PACKET as f64 / RATE as f64);
    let n = (secs / period.as_secs_f64()) as u32;
    let mut costs = Vec::with_capacity(n as usize);
    let start = Instant::now();
    for seq in 0..n {
        let t = Instant::now();
        for _ in 0..copies {
            let _ = sock.send_to(&buf, to);
        }
        let took = t.elapsed().as_nanos() as u64;
        if seq >= n / 10 {
            costs.push(took);
        }
        let next = start + period * (seq + 1);
        while Instant::now() < next {
            std::hint::spin_loop();
        }
    }
    let stalls = costs.iter().filter(|&&c| c > 300_000).count();
    println!(
        "network card, {copies} send_to per packet time | took {} | > 300 µs: {stalls} in {:.0} s",
        show(&mut costs),
        costs.len() as f64 * period.as_secs_f64()
    );
}

/// Seal and open alone, per cipher and packet size (ns per packet, mean and the slowest of
/// 1000 batches of 100), checking every packet opens back to the same samples and that a
/// flipped bit is refused.
fn crypto_cost() {
    use gachi_proto::audio::{self, Packet, SampleFormat, MAX_PACKET, MAX_SEALED};
    println!("AES in hardware: {}", crypto::aes_in_hardware());
    println!("cipher             format  frames bytes | seal mean  worst | open mean  worst  (ns per packet)");
    for cipher in [Cipher::Aes256Gcm, Cipher::ChaCha20Poly1305] {
        for (format, fname) in [(SampleFormat::Pcm16, "16-bit"), (SampleFormat::Pcm24, "24-bit")] {
            for frames in [32usize, 64, 128] {
                let (a, b) = (Identity::generate(), Identity::generate());
                let tx = a.session_using(cipher, 1, 2, &b.public).unwrap();
                let rx = b.session_using(cipher, 2, 1, &a.public).unwrap();
                let samples: Vec<f32> = (0..frames * 2).map(|i| ((i as f32) * 0.37).sin() * 0.5).collect();
                let mut plain = [0u8; MAX_PACKET];
                let n = audio::encode_audio(&mut plain, 1, 7, 2, format, &samples);
                let mut wire = vec![[0u8; MAX_SEALED]; 100];
                let mut lens = [0usize; 100];
                let mut out = [0u8; MAX_SEALED];
                let (mut seal_sum, mut seal_worst, mut open_sum, mut open_worst) = (0u128, 0u128, 0u128, 0u128);
                let batches = 1000u128;
                for _ in 0..batches {
                    let t = Instant::now();
                    for k in 0..100 {
                        lens[k] = tx.seal(1, &plain[..n], &mut wire[k]);
                    }
                    let e = t.elapsed().as_nanos();
                    seal_sum += e;
                    seal_worst = seal_worst.max(e);
                    let t = Instant::now();
                    for k in 0..100 {
                        let Ok(Packet::Sealed { counter, aad, body, .. }) = audio::decode(&wire[k][..lens[k]]) else { panic!("not sealed") };
                        assert_eq!(rx.open(counter, aad, body, &mut out), Some(n), "{} failed to open", cipher.label());
                    }
                    let e = t.elapsed().as_nanos();
                    open_sum += e;
                    open_worst = open_worst.max(e);
                }
                // what came out is what went in (decode the last one fully)
                let Ok(Packet::Audio { header, payload, .. }) = audio::decode(&out[..n]) else { panic!("inner packet") };
                let mut back = vec![0f32; frames * 2];
                assert_eq!(audio::decode_samples(&header, payload, &mut back), frames * 2);
                let tol = if format == SampleFormat::Pcm16 { 1.0 / 16384.0 } else { 1.0 / 4_000_000.0 };
                assert!(samples.iter().zip(&back).all(|(x, y)| (x - y).abs() < tol), "samples changed on the way");
                // a flipped bit anywhere after the header is refused
                let mut bad = wire[0];
                bad[lens[0] / 2] ^= 0x10;
                if let Ok(Packet::Sealed { counter, aad, body, .. }) = audio::decode(&bad[..lens[0]]) {
                    assert!(rx.open(counter, aad, body, &mut out).is_none(), "tampered packet opened");
                }
                println!(
                    "{:<18} {fname}  {frames:>6} {:>5} | {:>9.0} {:>6.0} | {:>9.0} {:>6.0}",
                    cipher.label(),
                    lens[0],
                    seal_sum as f64 / (batches * 100) as f64,
                    seal_worst as f64 / 100.0,
                    open_sum as f64 / (batches * 100) as f64,
                    open_worst as f64 / 100.0,
                );
            }
        }
    }
}

fn main() {
    let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
    // --eco on|off: Windows power throttling (EcoQoS) forced on, or opted out of
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--eco") {
        let want = match args.get(i + 1).map(String::as_str) {
            Some("on") => Some(true),
            Some("off") => Some(false),
            _ => None,
        };
        println!("power throttling {want:?}: {}", if gachi_net::prio::power_throttling(want) { "set" } else { "refused" });
    }
    if std::env::args().any(|a| a == "--crypto") {
        crypto_cost();
        return;
    }
    if std::env::args().any(|a| a == "--control") {
        control(secs);
        return;
    }
    if std::env::args().any(|a| a == "--nic") {
        nic(secs, 1);
        nic(secs, 7);
        return;
    }
    let stun = spawn_stun();
    println!("µs, p50 p99 p99.9 max; one packet = {PACKET} frames every {:.0} µs", PACKET as f64 * 1e6 / RATE as f64);
    let ciphers: Vec<Cipher> = if crypto::aes_in_hardware() {
        vec![Cipher::Aes256Gcm, Cipher::ChaCha20Poly1305]
    } else {
        println!("(no AES in hardware here: ChaCha20 only)");
        vec![Cipher::ChaCha20Poly1305]
    };
    for others in [1, 4, 7] {
        for doubled in [false, true] {
            for &c in &ciphers {
                case(&stun, c, others, doubled, secs);
            }
        }
    }
}
