//! One PC sends a gachi-like UDP stream to another (232-byte packets every 0.667 ms, as
//! 32-frame 24-bit stereo audio), switching every 30 s between DSCP EF (46, what gachi
//! marks) and no mark; the other PC measures how late each packet is. No gachi involved:
//! is it the path, the mark, or the app?
//!
//! On the receiving PC:   udp_path recv <sender ip> [seconds]
//! On the sending PC:     udp_path send <receiver ip> [seconds]
//!                        udp_path send4 <receiver ip> [seconds]   (4 flows from 4 ports, no
//!                        mark: does one receive queue / CPU on the other side hold them up?)
//!
//! The receiver knocks first (a packet to the sender every 200 ms), so the Windows firewall
//! lets the sender's stream back in as a reply. Ports 50600 (receiver) and 50601 (sender).

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const RECV_PORT: u16 = 50600;
const SEND_PORT: u16 = 50601;
const PERIOD_US: f64 = 32.0 / 48.0 * 1000.0;
const PHASE_S: u64 = 30;
const SIZE: usize = 232;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "udp_path recv <sender ip> [seconds] | udp_path send|send4 <receiver ip> [seconds]";
    let mode = args.get(1).expect(usage).as_str();
    let peer: std::net::IpAddr = args.get(2).expect(usage).parse().expect("an IP address");
    let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(180);
    match mode {
        "send" => send(peer, secs, 1),
        "send4" => send(peer, secs, 4),
        "recv" => recv(peer, secs),
        _ => panic!("{usage}"),
    }
}

#[cfg(windows)]
fn pro_audio() {
    gachi_net::prio::pro_audio();
}
#[cfg(not(windows))]
fn pro_audio() {}

/// Packet: [seq u32][phase u8 (0 = EF, 1 = plain)][flow u8] + padding. With `flows` > 1 the
/// packets go round-robin from that many sockets (source ports 50601, 50602, ...): each
/// flow lands in its own receive queue on the other side (RSS hashes the ports).
fn send(to: std::net::IpAddr, secs: u64, flows: usize) {
    let socks: Vec<socket2::Socket> = (0..flows)
        .map(|f| {
            let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)).unwrap();
            sock.set_reuse_address(true).ok();
            sock.bind(&SocketAddr::from(([0, 0, 0, 0], SEND_PORT + f as u16)).into()).expect("bind a sender port");
            let _ = sock.set_send_buffer_size(1 << 20);
            sock
        })
        .collect();
    let dest: socket2::SockAddr = SocketAddr::new(to, RECV_PORT).into();
    pro_audio();
    if flows > 1 {
        println!("sending to {to}:{RECV_PORT} for {secs} s, {flows} flows (ports {SEND_PORT}..{}), no mark", SEND_PORT + flows as u16 - 1);
    } else {
        println!("sending to {to}:{RECV_PORT} for {secs} s, {PHASE_S} s EF / {PHASE_S} s plain");
    }
    let mut buf = [0x47u8; SIZE];
    let start = Instant::now();
    let mut seq = 0u32;
    let mut phase = u8::MAX;
    // the sender's own hold-ups: a send later than planned by this much (it was off the CPU)
    let (mut late_5, mut late_10, mut worst_late) = (0u32, 0u32, 0f64);
    while start.elapsed().as_secs() < secs {
        let want = if flows > 1 { 1 } else { ((start.elapsed().as_secs() / PHASE_S) % 2) as u8 };
        if want != phase {
            phase = want;
            for s in &socks {
                let _ = s.set_tos(if phase == 0 { 46 << 2 } else { 0 });
            }
            if flows == 1 {
                println!("  {:>4} s: {}", start.elapsed().as_secs(), if phase == 0 { "EF (46)" } else { "plain (0)" });
            }
        }
        let planned = start + Duration::from_secs_f64(seq as f64 * PERIOD_US / 1e6);
        let late = Instant::now().saturating_duration_since(planned).as_secs_f64() * 1000.0;
        if seq > 0 {
            worst_late = worst_late.max(late);
            if late > 5.0 {
                late_5 += 1;
            }
            if late > 10.0 {
                late_10 += 1;
            }
        }
        let flow = seq as usize % flows;
        buf[..4].copy_from_slice(&seq.to_le_bytes());
        buf[4] = phase;
        buf[5] = flow as u8;
        let _ = socks[flow].send_to(&buf, &dest);
        seq += 1;
        let next = start + Duration::from_secs_f64(seq as f64 * PERIOD_US / 1e6);
        while Instant::now() < next {
            std::hint::spin_loop();
        }
    }
    println!("sent {seq} packets; sends later than planned: > 5 ms {late_5}, > 10 ms {late_10}, worst {worst_late:.1} ms (the sender itself held up)");
}

#[derive(Default)]
struct Phase {
    n: u64,
    lateness: Vec<f64>,
    /// worst lateness per second
    seconds: Vec<f64>,
    gaps_10: u32,
    gaps_20: u32,
    max_gap: f64,
}

fn recv(from: std::net::IpAddr, secs: u64) {
    let sock = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], RECV_PORT))).expect("bind 50600");
    sock.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let knock_to: Vec<SocketAddr> = (0..4).map(|f| SocketAddr::new(from, SEND_PORT + f)).collect();
    // --no-mmcss: receive at normal priority (Windows throttles network processing while
    // MMCSS "multimedia" threads run: NetworkThrottlingIndex)
    let mmcss = !std::env::args().any(|a| a == "--no-mmcss");
    if mmcss {
        pro_audio();
    }
    println!("receiving thread: {}", if mmcss { "MMCSS Pro Audio" } else { "normal priority (no MMCSS)" });
    println!("listening on {RECV_PORT} for {from}: waiting up to 10 min for the sender, then {secs} s");
    let start = Instant::now();
    let mut last_knock = start - Duration::from_secs(1);
    // [phase 0 (EF, one flow)], [phase 1 (plain): flow 0..3]
    let mut phases: Vec<Phase> = (0..5).map(|_| Phase::default()).collect();
    let mut flow_state: Vec<(f64, f64, f64, Option<f64>)> = vec![(f64::MAX, 0.0, 0.0, None); 5];
    let mut buf = [0u8; 2048];
    // per phase run: earliest transit, current second
    let mut cur: Option<u8> = None;
    let mut first: Option<Instant> = None;
    // wait up to 10 min for the sender, then measure `secs` from its first packet
    loop {
        match first {
            None if start.elapsed().as_secs() >= 600 => break,
            Some(t) if t.elapsed().as_secs() >= secs + 2 => break,
            _ => {}
        }
        if last_knock.elapsed() >= Duration::from_millis(200) {
            for k in &knock_to {
                let _ = sock.send_to(b"knock", k);
            }
            last_knock = Instant::now();
        }
        let Ok((n, src)) = sock.recv_from(&mut buf) else { continue };
        if src.ip() != from || n < 5 {
            continue;
        }
        let t0 = *first.get_or_insert_with(Instant::now);
        let arrival = t0.elapsed().as_secs_f64() * 1000.0;
        let seq = u32::from_le_bytes(buf[..4].try_into().unwrap());
        let phase = buf[4].min(1);
        let flow = if n > 5 { buf[5].min(3) } else { 0 };
        let slot = if phase == 0 { 0 } else { 1 + flow as usize };
        if cur != Some(phase) {
            // a new phase: timing starts over
            cur = Some(phase);
            for st in flow_state.iter_mut() {
                *st = (f64::MAX, arrival, 0.0, None);
            }
        }
        let (min_transit, sec_start, sec_max, last_arrival) = &mut flow_state[slot];
        let transit = arrival - seq as f64 * PERIOD_US / 1000.0;
        *min_transit = min_transit.min(transit);
        let late = transit - *min_transit;
        let p = &mut phases[slot];
        p.n += 1;
        p.lateness.push(late);
        if let Some(prev) = *last_arrival {
            let gap = arrival - prev;
            p.max_gap = p.max_gap.max(gap);
            if gap > 10.0 {
                p.gaps_10 += 1;
            }
            if gap > 20.0 {
                p.gaps_20 += 1;
            }
        }
        *last_arrival = Some(arrival);
        *sec_max = sec_max.max(late);
        if arrival - *sec_start >= 1000.0 {
            p.seconds.push(*sec_max);
            *sec_max = 0.0;
            *sec_start = arrival;
        }
    }
    println!("ms         packets | lateness p50   p99  p99.9    max | seconds with worst > 5 / 10 / 20 ms | gaps > 10 / 20 ms, longest");
    for (i, p) in phases.iter_mut().enumerate().filter(|(_, p)| p.n > 0) {
        p.lateness.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |f: f64| p.lateness.get(((p.lateness.len() as f64 * f) as usize).min(p.lateness.len().saturating_sub(1))).copied().unwrap_or(0.0);
        let over = |ms: f64| p.seconds.iter().filter(|&&s| s > ms).count();
        println!(
            "{:<10} {:>8} | {:>12.2} {:>5.2} {:>6.2} {:>6.1} | {:>3} / {:>3} / {:>3} of {:>3} | {:>4} / {:>4}, {:.1}",
            ["EF (46)", "plain", "plain f1", "plain f2", "plain f3"][i],
            p.n,
            q(0.5),
            q(0.99),
            q(0.999),
            q(1.0),
            over(5.0),
            over(10.0),
            over(20.0),
            p.seconds.len(),
            p.gaps_10,
            p.gaps_20,
            p.max_gap
        );
    }
}
