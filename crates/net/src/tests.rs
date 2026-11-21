use super::*;
use std::time::Duration;

/// A local stand-in for a public STUN server: answers Binding requests with the sender's
/// address.
fn spawn_stun() -> String {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    thread::spawn(move || {
        let mut buf = [0u8; 576];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if n >= 20 {
                let txid: [u8; 12] = buf[8..20].try_into().unwrap();
                let _ = sock.send_to(&stun::response(&txid, from), from);
            }
        }
    });
    addr.to_string()
}

fn cfg(id: PeerId) -> NetConfig {
    NetConfig {
        my_id: id,
        sample_rate: 48_000,
        packet_frames: 32,
        format: SampleFormat::Pcm24,
        punch_timeout: NetConfig::DEFAULT_PUNCH_TIMEOUT,
        upnp: false, // never touch the real router from tests
        peer_timeout: NetConfig::DEFAULT_PEER_TIMEOUT,
    }
}

fn start_with(stun: &str, cfg: NetConfig) -> Net {
    let g = Gathered::gather(&[stun.to_string()]).unwrap();
    Net::start(g, cfg).unwrap()
}

fn start(stun: &str, id: PeerId) -> Net {
    start_with(stun, cfg(id))
}

fn wait_connected(net: &Net, id: PeerId) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(NetEvent::Connected { id: got, addr }) = net.try_event() {
            assert_eq!(got, id);
            return addr;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("peer {id} never connected");
}

#[test]
fn gathering_asks_stun_for_the_public_address() {
    let stun = spawn_stun();
    let g = Gathered::gather(&["unreachable.invalid:3478".into(), stun]).unwrap();
    let public = g.public.expect("the second STUN server answered");
    assert_eq!(public.port(), g.socket.port());
    assert!(g.candidates.contains(&public));
    // nobody answers: still usable on the LAN
    let none = Gathered::gather(&["127.0.0.1:9".into()]).unwrap();
    assert!(none.public.is_none());
}

#[test]
fn two_peers_punch_and_stream_audio() {
    let stun = spawn_stun();
    let a = start(&stun, 1);
    let b = start(&stun, 2);
    assert!(!a.candidates.is_empty(), "STUN should give a candidate");

    let (a_tx, _a_rx) = rtrb::RingBuffer::<Frame>::new(64);
    let (b_tx, mut b_rx) = rtrb::RingBuffer::<Frame>::new(64);
    a.add_peer(2, a_tx);
    b.add_peer(1, b_tx);
    a.set_candidates(2, b.candidates.clone(), b.token, b.peer_key());
    b.set_candidates(1, a.candidates.clone(), a.token, a.peer_key());

    wait_connected(&a, 2);
    wait_connected(&b, 1);

    send_and_check(&a, &mut b_rx);

    // RTT shows up after a ping round (pings are sealed too)
    // (wait for it: a busy machine running other tests can delay a round)
    let t = std::time::Instant::now();
    while a.stats(2).unwrap().rtt_ms.is_none() && t.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(a.stats(2).unwrap().rtt_ms.is_some());

    // same process, same clock: the measured offset is about zero
    let off = a
        .stats(2)
        .unwrap()
        .clock_offset_us
        .expect("offset measured");
    assert!(off.abs() < 2_000, "offset {off} µs");
}

fn wait_event(net: &Net, f: impl Fn(&NetEvent) -> bool) -> NetEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(ev) = net.try_event() {
            if f(&ev) {
                return ev;
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("event never came");
}

#[test]
fn room_messages_arrive_in_order_and_bye_is_heard() {
    let (a, b, _b_rx) = connected_pair(&spawn_stun());
    for i in 0..50u32 {
        a.send_control(2, format!("msg {i}").into_bytes());
    }
    let mut got = Vec::new();
    while got.len() < 50 {
        if let NetEvent::Control { id, data } =
            wait_event(&b, |e| matches!(e, NetEvent::Control { .. }))
        {
            assert_eq!(id, 1);
            got.push(String::from_utf8(data).unwrap());
        }
    }
    assert_eq!(got, (0..50).map(|i| format!("msg {i}")).collect::<Vec<_>>());

    // a leaves on purpose: b hears about it right away
    drop(a);
    assert!(matches!(
        wait_event(&b, |e| matches!(e, NetEvent::PeerLeft { .. })),
        NetEvent::PeerLeft { id: 1 }
    ));
}

#[test]
fn silent_peer_is_reported_lost() {
    let stun = spawn_stun();
    let quick = |id| NetConfig {
        peer_timeout: Duration::from_millis(400),
        ..cfg(id)
    };
    let a = start_with(&stun, quick(1));
    let b = start_with(&stun, quick(2));
    let (a_tx, _) = rtrb::RingBuffer::<Frame>::new(8);
    let (b_tx, _) = rtrb::RingBuffer::<Frame>::new(8);
    a.add_peer(2, a_tx);
    b.add_peer(1, b_tx);
    a.set_candidates(2, b.candidates.clone(), b.token, b.peer_key());
    b.set_candidates(1, a.candidates.clone(), a.token, a.peer_key());
    wait_connected(&a, 2);
    // a ping round: a has heard b at least once (waits longer on a busy machine)
    let t = Instant::now();
    while a.stats(2).unwrap().rtt_ms.is_none() && t.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(50));
    }
    // b goes quiet without a goodbye (it simply stops talking to a)
    b.remove_peer(1);
    assert!(matches!(
        wait_event(&a, |e| matches!(e, NetEvent::PeerLost { .. })),
        NetEvent::PeerLost { id: 2, .. }
    ));
}

/// Connects a and b directly; returns b's ring for audio from a.
fn connected_pair(stun: &str) -> (Net, Net, rtrb::Consumer<Frame>) {
    let a = start(stun, 1);
    let b = start(stun, 2);
    let (a_tx, _) = rtrb::RingBuffer::<Frame>::new(64);
    let (b_tx, b_rx) = rtrb::RingBuffer::<Frame>::new(64);
    a.add_peer(2, a_tx);
    b.add_peer(1, b_tx);
    a.set_candidates(2, b.candidates.clone(), b.token, b.peer_key());
    b.set_candidates(1, a.candidates.clone(), a.token, a.peer_key());
    wait_connected(&a, 2);
    wait_connected(&b, 1);
    (a, b, b_rx)
}

/// Memory over a long room: two peers stream audio and room messages to each other while
/// others come and go many times a second (each with its own keys, punching, stats). What
/// the process holds must level off, not grow with every one who came and went.
/// Run alone (it measures the whole process):
/// `cargo test -p gachi-net --release --lib -- --ignored --exact tests::memory_levels_off_while_people_come_and_go --nocapture`
/// (`GACHI_SOAK_SECS` for longer than 40 s)
#[test]
#[ignore]
fn memory_levels_off_while_people_come_and_go() {
    let secs: u64 = std::env::var("GACHI_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let (a, b, mut b_rx) = connected_pair(&spawn_stun());
    let mut sender = a.sender();
    let mut f = Frame {
        channels: 2,
        frames: 32,
        ..Frame::EMPTY
    };
    let start = Instant::now();
    let (mut seq, mut next_id, mut churned) = (0u32, 100u16, 0u32);
    let mut last_churn = Instant::now();
    let mut visitor: Option<PeerId> = None;
    let mut samples = Vec::new();
    let mut next_sample = Duration::from_secs(5);
    while start.elapsed() < Duration::from_secs(secs) {
        // ~1000 audio packets a second, a room message now and then
        for _ in 0..2 {
            f.seq = seq;
            seq = seq.wrapping_add(1);
            sender.send(&f);
        }
        if seq % 100 == 0 {
            a.send_control(2, vec![7u8; 200]);
        }
        while b_rx.pop().is_ok() {}
        while a.try_event().is_some() {}
        while b.try_event().is_some() {}
        // someone comes in and goes again, every 50 ms
        if last_churn.elapsed() >= Duration::from_millis(50) {
            last_churn = Instant::now();
            if let Some(id) = visitor.take() {
                b.remove_peer(id);
                churned += 1;
            } else {
                let id = next_id;
                next_id = next_id.wrapping_add(1).max(100);
                let (tx, _) = rtrb::RingBuffer::<Frame>::new(64);
                b.add_peer(id, tx);
                let key = crypto::PeerKey {
                    public: crypto::Identity::generate().public,
                    aes: false,
                };
                b.set_candidates(id, a.candidates.clone(), rand::random(), key);
                b.send_control(id, vec![1u8; 100]);
                visitor = Some(id);
            }
        }
        if start.elapsed() >= next_sample {
            next_sample += Duration::from_secs(5);
            let live = alloc_guard::live_bytes();
            println!(
                "{:>4} s: {:>9} bytes held, {churned} came and went",
                start.elapsed().as_secs(),
                live
            );
            samples.push(live);
        }
        thread::sleep(Duration::from_millis(2));
    }
    // from 10 s on (everything warmed up) to the end
    let (first, last) = (samples[1], *samples.last().unwrap());
    let per_visit = (last - first) as f64
        / (churned as f64 * (samples.len() - 2) as f64 / samples.len() as f64).max(1.0);
    println!(
        "grew {} bytes after warm-up ({per_visit:.0} per one who came and went)",
        last - first
    );
    assert!(
        last - first < 64 * 1024,
        "held {first} at 10 s, {last} at the end: grows"
    );
}

#[test]
fn drops_unencrypted_and_wrongly_keyed_audio() {
    let (a, b, mut b_rx) = connected_pair(&spawn_stun());
    let b_addr = *b.candidates.first().unwrap();

    // an attacker spoofing a's socket: plain audio packet, then one sealed with another key
    let mut plain = [0u8; audio::MAX_PACKET];
    let n = audio::encode_audio(&mut plain, 1, 99, 1, SampleFormat::Pcm16, &[0.9; 32]);
    a.socket.send_to(&plain[..n], b_addr).unwrap();
    let stranger = crypto::Identity::generate()
        .session_with(1, 2, &b.peer_key())
        .unwrap();
    let mut wire = [0u8; audio::MAX_SEALED];
    let len = stranger.seal(1, &plain[..n], &mut wire);
    a.socket.send_to(&wire[..len], b_addr).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(b_rx.pop().is_err(), "forged audio must not reach the mixer");

    // a replayed genuine packet is accepted once only
    let keys = a.routes.load().first().map(|r| r.2.clone()).unwrap();
    let len = keys.seal(1, &plain[..n], &mut wire);
    a.socket.send_to(&wire[..len], b_addr).unwrap();
    a.socket.send_to(&wire[..len], b_addr).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(b_rx.pop().is_ok());
    assert!(b_rx.pop().is_err(), "replay dropped");
}

fn send_and_check(from: &Net, to_rx: &mut rtrb::Consumer<Frame>) {
    let mut sender = from.sender();
    let mut f = Frame {
        seq: 7,
        channels: 2,
        frames: 32,
        ..Frame::EMPTY
    };
    f.data[..64]
        .iter_mut()
        .enumerate()
        .for_each(|(i, s)| *s = i as f32 / 100.0);
    // (sent again every 100 ms until it is there: what is checked is that the connection
    // carries audio, and one datagram can be lost on a busy machine, the tests running side
    // by side. It failed once that way. The receiver keeps one copy of a sequence number)
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut resend = Instant::now();
    let got = loop {
        if Instant::now() >= resend {
            sender.send(&f);
            resend += Duration::from_millis(100);
        }
        if let Ok(frame) = to_rx.pop() {
            break frame;
        }
        assert!(Instant::now() < deadline, "audio never arrived");
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!((got.seq, got.frames, got.channels), (7, 32, 2));
    assert!((got.data[10] - 0.10).abs() < 1e-5);
}

#[test]
fn unreachable_peer_is_reported_not_relayed() {
    let stun = spawn_stun();
    let quick = |id| NetConfig {
        punch_timeout: Duration::from_millis(300),
        ..cfg(id)
    };
    let a = start_with(&stun, quick(1));
    let b = start_with(&stun, quick(2));
    let (a_tx, _) = rtrb::RingBuffer::<Frame>::new(8);
    a.add_peer(2, a_tx);
    // addresses nobody listens on: punching cannot get through
    a.set_candidates(
        2,
        vec!["127.0.0.1:9".parse().unwrap()],
        b.token,
        b.peer_key(),
    );
    assert!(matches!(
        wait_event(&a, |e| matches!(e, NetEvent::PunchFailed { .. })),
        NetEvent::PunchFailed { id: 2 }
    ));
    assert!(
        a.routes.load().is_empty(),
        "no route, nothing is sent anywhere"
    );
}

#[test]
fn ignores_punch_with_wrong_token() {
    let stun = spawn_stun();
    let a = start(&stun, 1);
    let b = start(&stun, 2);
    let (a_tx, _) = rtrb::RingBuffer::<Frame>::new(8);
    let (b_tx, _) = rtrb::RingBuffer::<Frame>::new(8);
    a.add_peer(2, a_tx);
    b.add_peer(1, b_tx);
    // b expects a wrong token from a, and a never punches toward b
    b.set_candidates(
        1,
        a.candidates.clone(),
        a.token.wrapping_add(1),
        a.peer_key(),
    );
    thread::sleep(Duration::from_millis(400));
    assert!(a.try_event().is_none());
}

#[test]
fn connects_over_ipv6() {
    let stun = spawn_stun();
    let a = start(&stun, 1);
    let b = start(&stun, 2);
    if !a.socket.is_dual_stack() {
        eprintln!("no IPv6 on this machine; skipping");
        return;
    }
    // offer only IPv6 loopback addresses, so the route can only be IPv6
    let v6 = |n: &Net| {
        vec![SocketAddr::new(
            std::net::Ipv6Addr::LOCALHOST.into(),
            n.socket.port(),
        )]
    };
    let (a_tx, _) = rtrb::RingBuffer::<Frame>::new(64);
    let (b_tx, mut b_rx) = rtrb::RingBuffer::<Frame>::new(64);
    a.add_peer(2, a_tx);
    b.add_peer(1, b_tx);
    a.set_candidates(2, v6(&b), b.token, b.peer_key());
    b.set_candidates(1, v6(&a), a.token, a.peer_key());
    assert!(wait_connected(&a, 2).is_ipv6());
    assert!(a.routes.load().iter().all(|r| r.1.is_ipv6()));
    send_and_check(&a, &mut b_rx);
}

#[test]
fn copies_on_a_lossy_link_arrive_once() {
    let (a, b, mut b_rx) = connected_pair(&spawn_stun());
    a.set_redundancy(2, true);
    thread::sleep(Duration::from_millis(50)); // the routes are republished
    let mut sender = a.sender();
    for seq in 0..20u32 {
        let mut f = Frame {
            seq,
            channels: 2,
            frames: 32,
            ..Frame::EMPTY
        };
        f.data[0] = seq as f32;
        sender.send(&f);
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(200));
    let mut got = Vec::new();
    while let Ok(f) = b_rx.pop() {
        got.push(f.seq);
    }
    // every packet once: the late copies (2 packets behind) are recognised and dropped
    assert_eq!(got, (0..20).collect::<Vec<_>>());
    drop(b);
}

/// Random bytes, and random bytes behind a valid header (so they get past the first
/// checks): what an attacker or a broken router could send.
fn junk(rng: &mut impl rand::Rng) -> Vec<u8> {
    let len = rng.gen_range(0..1600);
    let mut b: Vec<u8> = (0..len).map(|_| rng.gen()).collect();
    if len >= 2 && rng.gen_bool(0.7) {
        b[0] = audio::MAGIC;
        b[1] = rng.gen_range(0..12);
    }
    b
}

#[test]
fn parsers_survive_random_input() {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(0x9ac41);
    let keys = crypto::Identity::generate()
        .session_with(
            1,
            2,
            &crypto::PeerKey {
                public: crypto::Identity::generate().public,
                aes: true,
            },
        )
        .unwrap();
    let join = joinseal::JoinKey::generate();
    let mut out = [0u8; audio::MAX_PACKET];
    let mut samples = [0f32; audio::MAX_FRAMES * audio::MAX_CHANNELS];
    for i in 0..50_000 {
        let b = junk(&mut rng);
        match audio::decode(&b) {
            Ok(audio::Packet::Audio {
                header, payload, ..
            }) => {
                audio::decode_samples(&header, payload, &mut samples);
            }
            Ok(audio::Packet::Sealed {
                counter, aad, body, ..
            }) => {
                assert!(
                    keys.open(counter, aad, body, &mut out).is_none(),
                    "random bytes never authenticate"
                );
            }
            _ => {}
        }
        let _ = gachi_proto::peer::PeerMsg::from_bytes(&b);
        let _ = stun::parse_response(&b, &[0; 12]);
        if b.len() >= 32 && i % 25 == 0 {
            let eph: [u8; 32] = b[..32].try_into().unwrap();
            assert!(join.open(&eph, None, &b[32..]).is_none());
        }
    }
    // JSON-shaped junk for the room messages
    for s in [
        "{}",
        r#"{"t":"chat"}"#,
        r#"{"t":"chat","text":1}"#,
        r#"{"t":"link_report","raw_loss":"x"}"#,
        "[",
        "null",
    ] {
        assert!(
            gachi_proto::peer::PeerMsg::from_bytes(s.as_bytes()).is_none(),
            "{s}"
        );
    }
    let _ = rng.gen::<u8>();
}

#[test]
fn junk_packets_do_not_disturb_a_connection() {
    use rand::SeedableRng;
    let (a, b, mut b_rx) = connected_pair(&spawn_stun());
    let b_addr = *b.candidates.first().unwrap();
    let route_before = a.routes.load().first().map(|r| r.1);
    // a stranger floods b with junk while a and b talk
    let attacker = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    for _ in 0..5_000 {
        let _ = attacker.send_to(&junk(&mut rng), b_addr);
    }
    // and replays/forges with a's real sender id from its own address
    let mut fake = [0u8; audio::MAX_SEALED];
    fake[0] = audio::MAGIC;
    fake[1] = 10; // sealed
    fake[2] = 1;
    for _ in 0..500 {
        let _ = attacker.send_to(&fake[..64], b_addr);
    }
    thread::sleep(Duration::from_millis(200));
    while b_rx.pop().is_ok() {}
    send_and_check(&a, &mut b_rx);
    assert_eq!(
        a.routes.load().first().map(|r| r.1),
        route_before,
        "junk never moves a route"
    );
    let b_route = b.routes.load().first().map(|r| r.1);
    assert_ne!(
        b_route,
        Some(attacker.local_addr().unwrap()),
        "unauthenticated packets never move a route"
    );
}

#[test]
fn public_address_on_the_pc_means_no_router() {
    let ip = |s: &str| s.parse::<IpAddr>().unwrap();
    assert!(behind_router(ip("192.168.0.7")));
    assert!(behind_router(ip("10.1.2.3")));
    assert!(behind_router(ip("100.72.1.9")), "carrier NAT");
    assert!(!behind_router(ip("203.0.113.5")), "straight on the modem");
    assert!(!behind_router(ip("100.128.0.1")));
}

#[test]
fn a_peer_back_with_the_same_key_keeps_its_counter() {
    let (a, b, _b_rx) = connected_pair(&spawn_stun());
    let keys = |n: &Net| n.routes.load().first().map(|r| r.2.clone()).unwrap();
    let before = keys(&a);
    // a drops b and takes it in again (same session, same key)
    a.remove_peer(2);
    let (tx, _) = rtrb::RingBuffer::<Frame>::new(8);
    a.add_peer(2, tx);
    a.set_candidates(2, b.candidates.clone(), b.token, b.peer_key());
    wait_connected(&a, 2);
    // the same keys (send counter going on), not new ones starting again at 0
    assert!(
        Arc::ptr_eq(&before, &keys(&a)),
        "keys were derived anew: nonces would repeat"
    );
}

#[test]
fn punches_cannot_pull_a_live_route_away() {
    let (a, b, _b_rx) = connected_pair(&spawn_stun());
    let route = |n: &Net| n.routes.load().first().map(|r| r.1).unwrap();
    // a talks to b sealed (pings) so b has heard a recently
    let t = Instant::now();
    while b.stats(1).unwrap().rtt_ms.is_none() && t.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(50));
    }
    let before = route(&b);
    // someone who saw a's token (it travels in the clear in punches) punches b as "a"
    let mallory = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut out = [0u8; 32];
    let n = audio::encode_punch(&mut out, false, 1, a.token);
    let b_addr = *b.candidates.first().unwrap();
    for _ in 0..5 {
        mallory.send_to(&out[..n], b_addr).unwrap();
        let n = audio::encode_punch(&mut out, true, 1, a.token);
        mallory.send_to(&out[..n], b_addr).unwrap();
    }
    thread::sleep(Duration::from_millis(200));
    assert_eq!(route(&b), before, "b still sends a's audio to a");
    assert_ne!(route(&b), mallory.local_addr().unwrap());
}

#[test]
fn a_roundabout_first_path_moves_to_a_more_direct_one() {
    let stun = spawn_stun();
    let a = start(&stun, 1);
    let b = start(&stun, 2);
    if !a.socket.is_dual_stack() || !b.socket.is_dual_stack() {
        return; // no IPv6 on this machine: nothing to rank against
    }
    let v6 = |n: &Net| SocketAddr::new("::1".parse().unwrap(), n.socket.port());
    let v4 = |n: &Net| SocketAddr::new("127.0.0.1".parse().unwrap(), n.socket.port());
    let (a_tx, _a_rx) = rtrb::RingBuffer::<Frame>::new(64);
    let (b_tx, _b_rx) = rtrb::RingBuffer::<Frame>::new(64);
    a.add_peer(2, a_tx);
    b.add_peer(1, b_tx);
    // first only the roundabout path (IPv6 ranks below "on our network")
    a.set_candidates(2, vec![v6(&b)], b.token, b.peer_key());
    b.set_candidates(1, vec![v6(&a)], a.token, a.peer_key());
    assert_eq!(wait_connected(&a, 2), v6(&b));
    // then the room tells the direct one too, shortly after connecting
    a.set_candidates(2, vec![v6(&b), v4(&b)], b.token, b.peer_key());
    b.set_candidates(1, vec![v6(&a), v4(&a)], a.token, a.peer_key());
    let route = |n: &Net| n.routes.load().first().map(|r| r.1);
    let t = Instant::now();
    while (route(&a) != Some(v4(&b)) || route(&b) != Some(v4(&a)))
        && t.elapsed() < Duration::from_secs(4)
    {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(route(&a), Some(v4(&b)), "a moved to the direct path");
    assert_eq!(route(&b), Some(v4(&a)), "and b with it");
}

/// Sending runs on the input audio callback: encode, seal and send_to, single and doubled
/// (copies on a lossy link), without asking the allocator for anything.
#[test]
fn sending_does_not_allocate() {
    let (a, _b, _b_rx) = connected_pair(&spawn_stun());
    let mut sender = a.sender();
    let mut frame = Frame {
        channels: 2,
        frames: 32,
        ..Frame::EMPTY
    };
    frame.data[..64].fill(0.25);
    for doubled in [false, true] {
        a.set_redundancy(2, doubled);
        thread::sleep(Duration::from_millis(100));
        for seq in 0..8 {
            frame.seq = seq; // (first calls: the history fills)
            sender.send(&frame);
        }
        let calls = crate::alloc_guard::count(|| {
            for seq in 8..2000 {
                frame.seq = seq;
                sender.send(&frame);
            }
        });
        assert_eq!(
            calls, 0,
            "send (doubled: {doubled}) asked the allocator {calls} times"
        );
    }
}

/// From the sender's call to the packet in the receiver's mixer ring: the receive thread
/// must pick each one up at once (it waits on the socket, not on a timer). Prints the
/// figures (`cargo test -p gachi-net arrival -- --nocapture`).
#[test]
fn audio_reaches_the_mixer_ring_at_once() {
    let (a, _b, mut b_rx) = connected_pair(&spawn_stun());
    thread::sleep(Duration::from_millis(300));
    while b_rx.pop().is_ok() {}
    let mut sender = a.sender();
    let mut f = Frame {
        channels: 2,
        frames: 32,
        ..Frame::EMPTY
    };
    let mut took = Vec::new();
    for seq in 0..300u32 {
        f.seq = seq;
        let t = Instant::now();
        sender.send(&f);
        // (spun on: sleeping between looks would measure the sleep)
        while b_rx.pop().is_err() {
            if t.elapsed() > Duration::from_millis(100) {
                break;
            }
            std::hint::spin_loop();
        }
        took.push(t.elapsed().as_micros() as u64);
        thread::sleep(Duration::from_millis(2));
    }
    took.sort_unstable();
    let at = |q: f64| took[((took.len() - 1) as f64 * q) as usize];
    println!(
        "arrival µs: p50 {} p90 {} p99 {} max {}",
        at(0.5),
        at(0.9),
        at(0.99),
        at(1.0)
    );
    assert!(at(0.5) < 2_000, "half the packets took over 2 ms: {took:?}");
    assert!(at(0.99) < 20_000, "p99 {} µs", at(0.99));
}

/// The started socket does not wait: a send the system has no room for is dropped instead
/// of holding up the audio callback (see `Sock::set_nonblocking`).
#[cfg(windows)]
#[test]
fn the_socket_never_waits() {
    let a = start(&spawn_stun(), 1);
    let mut buf = [0u8; 64];
    let t = Instant::now();
    let r = a.socket.recv_from(&mut buf);
    assert!(
        matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "{r:?}"
    );
    assert!(
        t.elapsed() < Duration::from_millis(3),
        "a read waited {:?}",
        t.elapsed()
    );
}

/// With nothing to read the receive thread waits on the socket: it must not spin (the
/// socket no longer waits by itself). Measured as the thread's own CPU time over a second.
#[cfg(windows)]
#[test]
fn an_idle_receive_thread_does_not_spin() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetThreadTimes;
    let a = start(&spawn_stun(), 1);
    let handle = a.thread.as_ref().unwrap().as_raw_handle();
    let cpu = || {
        let z = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut c, mut e, mut k, mut u) = (z, z, z, z);
        // SAFETY: the handle of a thread that lives as long as `a`; four valid out-pointers
        assert_ne!(
            unsafe { GetThreadTimes(handle as _, &mut c, &mut e, &mut k, &mut u) },
            0
        );
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) / 10; // µs
        t(k) + t(u)
    };
    thread::sleep(Duration::from_millis(200));
    let before = cpu();
    thread::sleep(Duration::from_secs(1));
    let used = cpu() - before;
    assert!(
        used < 100_000,
        "the idle receive thread used {used} µs of CPU in a second"
    );
}

/* ───────────── behind routers (see `nat_sim`) ───────────── */

mod behind_routers {
    use super::*;
    use crate::nat_sim::{reachable, stun_at, Filter, Mapping, Nat};
    use std::net::Ipv4Addr;

    fn lo(n: u8) -> Ipv4Addr {
        Ipv4Addr::new(127, 0, 0, n)
    }

    /// A peer behind `nat`: it asks the STUN servers through it, as it would at home.
    fn behind(nat: &Nat, stuns: &[SocketAddr], id: PeerId) -> Net {
        let servers: Vec<String> = stuns.iter().map(|s| nat.view(*s).to_string()).collect();
        start_with_servers(&servers, id)
    }

    fn start_with_servers(servers: &[String], id: PeerId) -> Net {
        let g = Gathered::gather(servers).unwrap();
        Net::start(g, cfg(id)).unwrap()
    }

    /// Two peers, each behind its router, told of each other as the room tells them (the
    /// addresses each announced, as the other's router shows them). Two STUN servers, as
    /// the app asks: that is how a router that changes ports per destination is noticed.
    fn pair(na: &Nat, nb: &Nat) -> (Net, Net, rtrb::Consumer<Frame>) {
        let stuns = [stun_at(lo(10)), stun_at(lo(11))];
        let a = behind(na, &stuns, 1);
        let b = behind(nb, &stuns, 2);
        let (a_tx, _) = rtrb::RingBuffer::<Frame>::new(64);
        let (b_tx, b_rx) = rtrb::RingBuffer::<Frame>::new(64);
        a.add_peer(2, a_tx);
        b.add_peer(1, b_tx);
        a.set_candidates(2, reachable(&b.candidates, nb, na), b.token, b.peer_key());
        b.set_candidates(1, reachable(&a.candidates, na, nb), a.token, a.peer_key());
        (a, b, b_rx)
    }

    /// Connected, or punching gave up (`None`), within the punch timeout.
    fn outcome(net: &Net) -> Option<SocketAddr> {
        let deadline = Instant::now() + NetConfig::DEFAULT_PUNCH_TIMEOUT + Duration::from_secs(2);
        while Instant::now() < deadline {
            match net.try_event() {
                Some(NetEvent::Connected { addr, .. }) => return Some(addr),
                Some(NetEvent::PunchFailed { .. }) => return None,
                _ => thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("neither connected nor gave up");
    }

    fn connects(na: Nat, nb: Nat) {
        let (a, b, mut b_rx) = pair(&na, &nb);
        assert!(outcome(&a).is_some(), "a did not get through");
        assert!(outcome(&b).is_some(), "b did not get through");
        thread::sleep(Duration::from_millis(100));
        while b_rx.pop().is_ok() {}
        send_and_check(&a, &mut b_rx);
    }

    fn home(ip: u8) -> Nat {
        Nat::new(lo(ip), Mapping::Independent, Filter::AddressPort)
    }

    fn symmetric(ip: u8) -> Nat {
        Nat::new(lo(ip), Mapping::PerDestination, Filter::AddressPort)
    }

    /// The common case: two homes, each router port restricted (it lets in only what
    /// answers what was sent).
    #[test]
    fn home_router_to_home_router() {
        connects(home(20), home(21));
    }

    #[test]
    fn open_router_to_home_router() {
        connects(
            Nat::new(lo(22), Mapping::Independent, Filter::Open),
            home(23),
        );
    }

    #[test]
    fn address_restricted_router_to_home_router() {
        connects(
            Nat::new(lo(24), Mapping::Independent, Filter::Address),
            home(25),
        );
    }

    /// A phone's hotspot (a carrier that changes ports per destination, in order) to a
    /// home: the home side knocks on the ports the hotspot will use next.
    #[test]
    fn hotspot_to_home_router() {
        connects(symmetric(26), home(27));
    }

    /// A hotspot whose ports cannot be guessed, to a home router that only lets in what
    /// answers what it sent: nobody can get in. To one that lets in anything from an
    /// address it sent to (address restricted, or a port opened by UPnP), it gets through.
    #[test]
    fn unguessable_hotspot_needs_the_home_side_open() {
        let random = || Nat::new(lo(30), Mapping::PerDestinationRandom, Filter::AddressPort);
        let (a, b, _) = pair(&random(), &home(31));
        assert_eq!(
            (outcome(&a), outcome(&b)),
            (None, None),
            "port restricted home: no way in"
        );
        drop((a, b));
        connects(
            random(),
            Nat::new(lo(32), Mapping::Independent, Filter::Address),
        );
    }

    /// The router forgets its mappings mid-session (restarted, or a carrier NAT moved us):
    /// what one side sends goes out from a new public port, which the other side's router
    /// does not let in, and nothing gets back either. The side that moved must know it is
    /// its own silence (the app then comes back in through the room); the other side sees
    /// a peer that went away.
    #[test]
    fn a_router_that_forgets_its_mappings_is_noticed_by_its_own_side() {
        let (na, nb) = (home(33), home(34));
        let (a, b, _) = pair(&na, &nb);
        assert!(outcome(&a).is_some() && outcome(&b).is_some());
        thread::sleep(Duration::from_millis(300));
        na.rebind();
        let deadline = Instant::now() + Duration::from_secs(15);
        let (mut a_lost, mut b_lost) = (None, None);
        while Instant::now() < deadline && (a_lost.is_none() || b_lost.is_none()) {
            if let Some(NetEvent::PeerLost { online, .. }) = a.try_event() {
                a_lost = Some(online);
            }
            if let Some(NetEvent::PeerLost { online, .. }) = b.try_event() {
                b_lost = Some(online);
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            a_lost,
            Some(false),
            "a's address moved: its own silence (as if offline)"
        );
        assert_eq!(b_lost, Some(true), "b's internet is fine: a went away");
    }

    /// A path that breaks right after connecting, before anything sealed came through: the
    /// peer must still be reported silent (it was never "heard", so it never was).
    #[test]
    fn a_peer_never_heard_after_connecting_still_goes_silent() {
        let (na, nb) = (home(35), home(36));
        let (a, _b, _) = pair(&na, &nb);
        assert!(outcome(&a).is_some());
        na.rebind();
        let deadline = Instant::now() + Duration::from_secs(14);
        while Instant::now() < deadline {
            if let Some(NetEvent::PeerLost { .. }) = a.try_event() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("never reported silent");
    }

    /// Two people behind the same carrier NAT (two homes, or two phones, of one provider):
    /// each can reach the other only at the carrier's public address, which only works if
    /// the carrier's router sends that back in ("hairpinning"). Without it, punching gives
    /// up and says so.
    #[test]
    fn two_behind_one_carrier_nat() {
        let carrier = || Nat::new(lo(40), Mapping::Independent, Filter::AddressPort);
        let cgn = carrier();
        connects(cgn.clone(), cgn);
        let cgn = carrier().without_hairpin();
        let (a, b, _) = pair(&cgn, &cgn);
        assert_eq!((outcome(&a), outcome(&b)), (None, None));
    }

    /// Two hotspots whose ports cannot be guessed: neither knows where the other will be
    /// let in. Punching must give up and say so (no relay).
    ///
    /// (Not two that hand their ports out in order: those do meet when each one's next
    /// port is the one the other guessed, see the test below, and here the order in which
    /// a router maps what a host sends at once is decided by its threads. With the ports
    /// in order this test failed now and then, when the whole workspace was tested at once.)
    #[test]
    fn hotspot_to_hotspot_gives_up() {
        let random = |ip| Nat::new(lo(ip), Mapping::PerDestinationRandom, Filter::AddressPort);
        let (na, nb) = (random(28), random(29));
        let (a, b, _) = pair(&na, &nb);
        assert_eq!(outcome(&a), None, "a should have given up");
        assert_eq!(outcome(&b), None, "b should have given up");
        // (the routers did turn the knocking away: nothing went around them)
        assert!(na.filtered() > 0 && nb.filtered() > 0);
    }

    /// Two hotspots that hand their ports out in order can meet: each knocks on the port
    /// the other will use next, and when that is the very next thing each router maps, the
    /// two knocks meet. Each side is told only the other's first guessed address here, so
    /// that it is (told all of them, which one a router maps first is up to its threads).
    #[test]
    fn hotspots_with_ports_in_order_meet_when_the_guess_is_next() {
        let (na, nb) = (symmetric(48), symmetric(49));
        let stuns = [stun_at(lo(10)), stun_at(lo(11))];
        let a = behind(&na, &stuns, 1);
        let b = behind(&nb, &stuns, 2);
        let (a_tx, _) = rtrb::RingBuffer::<Frame>::new(64);
        let (b_tx, _b_rx) = rtrb::RingBuffer::<Frame>::new(64);
        a.add_peer(2, a_tx);
        b.add_peer(1, b_tx);
        // (as announced: the address the first server saw, then the guesses)
        let first_guess = |c: Vec<SocketAddr>| c.into_iter().skip(1).take(1).collect::<Vec<_>>();
        a.set_candidates(
            2,
            first_guess(reachable(&b.candidates, &nb, &na)),
            b.token,
            b.peer_key(),
        );
        b.set_candidates(
            1,
            first_guess(reachable(&a.candidates, &na, &nb)),
            a.token,
            a.peer_key(),
        );
        assert!(outcome(&a).is_some(), "a did not get through");
        assert!(outcome(&b).is_some(), "b did not get through");
    }
}
