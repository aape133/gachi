use crate::control::ControlChannel;
use crate::crypto::{Identity, PeerCrypto, ReplayWindow};
use crate::sock::Sock;
use crate::stats::{PeerNetStats, StreamTracker};
use crate::{now_us, Ctl, NetConfig, NetEvent, Routes};
use gachi_engine::Frame;
use gachi_proto::audio::{self, Packet, MAX_INNER, MAX_SEALED, MAX_WIRE};
use gachi_proto::PeerId;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const PUNCH_EVERY: Duration = Duration::from_millis(20);
const PING_EVERY: Duration = Duration::from_millis(500);
/// A connected peer silent this long: punch again (its address or ours may have changed).
const REPUNCH_AFTER: Duration = Duration::from_secs(2);
const REPUNCH_EVERY: Duration = Duration::from_millis(100);
/// How often we look whether our own network changed (Wi-Fi ↔ wired, new IP).
const NET_CHECK_EVERY: Duration = Duration::from_secs(3);
/// After our network changed, punch everyone for this long.
const REPUNCH_FOR: Duration = Duration::from_secs(15);
/// A STUN answer we wait for no longer than this.
const STUN_WAIT: Duration = Duration::from_secs(1);
/// A connected peer silent this long: start asking a STUN server whether we are online.
const PROBE_AFTER: Duration = Duration::from_secs(3);
const PROBE_EVERY: Duration = Duration::from_secs(2);
/// Nothing at all heard for this long (no peer, no STUN answer): we are offline.
const OFFLINE_AFTER: Duration = Duration::from_secs(5);
/// After connecting, keep trying the peer's better addresses this long (see `path_rank`):
/// the first address that gets through is not always the shortest path.
const UPGRADE_FOR: Duration = Duration::from_secs(3);
const UPGRADE_EVERY: Duration = Duration::from_millis(100);

/// How direct a path to `addr` is: 0 = on our network (private IPv4, loopback), 1 = IPv6
/// (no NAT on the way), 2 = a public IPv4 address (through the NATs; between two PCs in one
/// home it even goes out to the router and back, "hairpin").
fn path_rank(addr: SocketAddr) -> u8 {
    match addr.ip() {
        IpAddr::V4(v4) if v4.is_private() || v4.is_loopback() || v4.is_link_local() => 0,
        IpAddr::V6(_) => 1,
        IpAddr::V4(_) => 2,
    }
}

/// Offset between the peer's clock and ours, from the fastest ping round trips (NTP-style).
#[derive(Default)]
struct ClockEstimate {
    best_rtt_us: Option<u64>,
}

struct Peer {
    tx: rtrb::Producer<Frame>,
    candidates: Vec<SocketAddr>,
    token: Option<u32>,
    /// the address that got through (punching); `None` until connected
    route: Option<SocketAddr>,
    punch_started: Option<Instant>,
    last_punch: Instant,
    tracker: StreamTracker,
    /// this peer's figures, owned by this thread; copied out to the shared map whenever
    /// its lock is free (the app reads there). The receive thread never waits on that lock:
    /// a reader preempted while holding it would hold up every packet.
    stats: PeerNetStats,
    failed: bool,
    keys: Option<Arc<PeerCrypto>>,
    key: Option<crate::crypto::PeerKey>,
    replay: ReplayWindow,
    control: ControlChannel,
    clock: ClockEstimate,
    /// last authenticated packet from this peer
    last_heard: Option<Instant>,
    lost_reported: bool,
    /// last punch sent while trying to find the peer again
    last_repunch: Instant,
    /// we send this peer each packet twice (it reported loss)
    redundant: bool,
    /// audio sequence numbers seen lately (drops the second copy of a packet)
    seen: SeqWindow,
    /// when the first route was found: better addresses are tried for `UPGRADE_FOR` after
    connected_at: Option<Instant>,
    last_upgrade: Instant,
}

/// Which of the last 64 sequence numbers arrived.
#[derive(Default)]
struct SeqWindow {
    newest: Option<u32>,
    bits: u64,
}

impl SeqWindow {
    /// Marks `seq` as seen; false if it already was (a duplicate).
    fn first_time(&mut self, seq: u32) -> bool {
        let Some(newest) = self.newest else {
            self.newest = Some(seq);
            self.bits = 1;
            return true;
        };
        let d = seq.wrapping_sub(newest) as i32;
        if d > 0 {
            self.bits = if d >= 64 { 0 } else { self.bits << d };
            self.bits |= 1;
            self.newest = Some(seq);
            true
        } else if d > -64 {
            let bit = 1u64 << (-d);
            let fresh = self.bits & bit == 0;
            self.bits |= bit;
            fresh
        } else if d < -1000 {
            // the sender restarted its stream: start over
            self.newest = Some(seq);
            self.bits = 1;
            true
        } else {
            true // too old to tell; let the jitter buffer decide
        }
    }
}

pub(crate) struct Receiver {
    socket: Arc<Sock>,
    cfg: NetConfig,
    my_token: u32,
    identity: Arc<Identity>,
    routes: Routes,
    stats: Arc<Mutex<HashMap<PeerId, PeerNetStats>>>,
    ctl_rx: mpsc::Receiver<Ctl>,
    events: mpsc::Sender<NetEvent>,
    stop: Arc<AtomicBool>,
    peers: HashMap<PeerId, Peer>,
    last_ping: Instant,
    ping_nonce: u32,
    /// set by `Ctl::Goodbye`: we said bye, leave the loop
    done: bool,
    /// our network as last seen (local IPv4 toward the internet)
    last_lan: Option<IpAddr>,
    net_check: Instant,
    /// a STUN server to ask for our new public address (resolved once at the start)
    stun_server: Option<SocketAddr>,
    /// a STUN question in flight: transaction id, when asked
    stun_pending: Option<([u8; 12], Instant)>,
    /// while a peer is silent: is it them or us? A STUN server is asked every
    /// `PROBE_EVERY`; `internet_at` is when anything last answered (it, or a peer)
    probe: Option<[u8; 12]>,
    last_probe: Instant,
    internet_at: Instant,
    /// after our network changed: punch every peer until then
    repunch_all_until: Option<Instant>,
    /// keys of removed peers. If one comes back with the same key, its keys (and their
    /// send counter) are taken up again: deriving them anew would restart the counter at 0
    /// and reuse nonces under the same key, which breaks the encryption.
    retired: HashMap<PeerId, (crate::crypto::PeerKey, Arc<PeerCrypto>, ReplayWindow)>,
}

impl Receiver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        socket: Arc<Sock>,
        cfg: NetConfig,
        my_token: u32,
        identity: Arc<Identity>,
        routes: Routes,
        stats: Arc<Mutex<HashMap<PeerId, PeerNetStats>>>,
        ctl_rx: mpsc::Receiver<Ctl>,
        events: mpsc::Sender<NetEvent>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            socket,
            cfg,
            my_token,
            identity,
            routes,
            stats,
            ctl_rx,
            events,
            stop,
            peers: HashMap::new(),
            retired: HashMap::new(),
            last_ping: Instant::now(),
            ping_nonce: 0,
            done: false,
            last_lan: lan_now(),
            net_check: Instant::now(),
            stun_server: crate::stun::DEFAULT_SERVERS.iter().find_map(|s| crate::stun::resolve(s)),
            stun_pending: None,
            probe: None,
            last_probe: Instant::now(),
            internet_at: Instant::now(),
            repunch_all_until: None,
        }
    }

    pub fn run(mut self) {
        let mut buf = [0u8; 2048];
        let mut plain = [0u8; MAX_SEALED];
        let mut frame = Frame::EMPTY;
        while !self.done && !self.stop.load(Ordering::Relaxed) {
            self.handle_ctl();
            if self.done {
                break;
            }
            self.timers();
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => self.on_datagram(&buf[..n], from, &mut plain, &mut frame),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                // Windows reports ICMP "port unreachable" from an earlier send as ConnectionReset
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                Err(e) => tracing::warn!("udp recv: {e}"),
            }
        }
    }

    fn handle_ctl(&mut self) {
        while let Ok(ctl) = self.ctl_rx.try_recv() {
            match ctl {
                Ctl::AddPeer { id, tx } => {
                    self.peers.insert(
                        id,
                        Peer {
                            tx,
                            candidates: Vec::new(),
                            token: None,
                            route: None,
                            punch_started: None,
                            last_punch: Instant::now(),
                            tracker: StreamTracker::new(self.cfg.sample_rate),
                            stats: PeerNetStats::default(),
                            failed: false,
                            keys: None,
                            key: None,
                            replay: ReplayWindow::default(),
                            control: ControlChannel::default(),
                            clock: ClockEstimate::default(),
                            last_heard: None,
                            lost_reported: false,
                            last_repunch: Instant::now(),
                            redundant: false,
                            seen: SeqWindow::default(),
                            connected_at: None,
                            last_upgrade: Instant::now(),
                        },
                    );
                    self.stats.lock().unwrap().insert(id, PeerNetStats::default());
                }
                Ctl::SetRate { id, sample_rate } => {
                    if let Some(p) = self.peers.get_mut(&id) {
                        p.tracker.set_sample_rate(sample_rate);
                    }
                }
                Ctl::SetOutput { id, tx } => {
                    if let Some(p) = self.peers.get_mut(&id) {
                        p.tx = tx;
                    }
                }
                Ctl::Candidates { id, addrs, token, key } => {
                    if let Some(p) = self.peers.get_mut(&id) {
                        if p.key != Some(key) {
                            if let Some(old) = p.key.zip(p.keys.take()) {
                                // a new key for a known peer: keep the old one's counter too
                                self.retired.insert(id, (old.0, old.1, std::mem::take(&mut p.replay)));
                            }
                            match self.retired.remove(&id) {
                                // the same peer, same key, back again: same keys, counter goes on
                                Some((k, keys, replay)) if k == key => {
                                    p.keys = Some(keys);
                                    p.replay = replay;
                                }
                                other => {
                                    if let Some(o) = other {
                                        self.retired.insert(id, o);
                                    }
                                    let Some(keys) = self.identity.session_with(self.cfg.my_id, id, &key) else {
                                        tracing::warn!(id, "peer sent an invalid public key; not connecting");
                                        continue;
                                    };
                                    tracing::info!(id, cipher = keys.cipher().label(), "audio encryption");
                                    p.keys = Some(Arc::new(keys));
                                    p.replay = ReplayWindow::default();
                                }
                            }
                            p.key = Some(key);
                        }
                        let changed = p.candidates != addrs;
                        p.candidates = addrs;
                        p.token = Some(token);
                        p.failed = false;
                        if p.route.is_none() {
                            p.punch_started = Some(Instant::now());
                        } else if changed {
                            // the peer's network changed: reach it at its new addresses
                            p.last_heard = Some(Instant::now() - REPUNCH_AFTER);
                        }
                    }
                }
                Ctl::SendControl { id, data } => {
                    if let Some(p) = self.peers.get_mut(&id) {
                        if !p.control.queue(data) {
                            tracing::warn!(id, "too many unacknowledged room messages; dropping one");
                        }
                    }
                }
                Ctl::SetRedundancy { id, on } => {
                    if let Some(p) = self.peers.get_mut(&id) {
                        if p.redundant != on {
                            p.redundant = on;
                            tracing::info!(id, on, "sending this peer every packet twice (lossy link)");
                            self.publish_routes();
                        }
                    }
                }
                Ctl::RemovePeer(id) => {
                    if let Some(p) = self.peers.remove(&id) {
                        if let (Some(key), Some(keys)) = (p.key, p.keys) {
                            self.retired.insert(id, (key, keys, p.replay));
                        }
                    }
                    self.stats.lock().unwrap().remove(&id);
                    self.publish_routes();
                }
                Ctl::Goodbye => {
                    // tell everyone we are leaving on purpose (twice, in case one is lost)
                    let mut out = [0u8; 8];
                    let n = audio::encode_bye(&mut out, self.cfg.my_id);
                    for _ in 0..2 {
                        for (&id, p) in &self.peers {
                            if let Some(route) = p.route {
                                self.send_to_peer(route, id, &out[..n]);
                            }
                        }
                    }
                    self.done = true;
                    return;
                }
            }
        }
    }

    /// Seals an inner packet (ping, pong, control, bye) for a peer and sends it to `addr`.
    fn send_to_peer(&self, addr: SocketAddr, dest: PeerId, plain: &[u8]) {
        let Some(keys) = self.peers.get(&dest).and_then(|p| p.keys.as_ref()) else { return };
        send_sealed(&self.socket, &self.cfg, keys, addr, plain);
    }

    fn timers(&mut self) {
        let now = Instant::now();
        let me = self.cfg.my_id;
        let mut out = [0u8; 32];

        // someone is silent: them, or us? Ask a STUN server meanwhile
        let silent = self.peers.values().any(|p| p.route.is_some() && p.last_heard.is_some_and(|h| now - h > PROBE_AFTER));
        if let (true, Some(server)) = (silent && now - self.last_probe >= PROBE_EVERY, self.stun_server) {
            let txid: [u8; 12] = rand::random();
            let _ = self.socket.send_to(&crate::stun::request(&txid), server);
            self.probe = Some(txid);
            self.last_probe = now;
        }
        // (no server to ask: we cannot tell, and say "online" as before)
        let online = self.stun_server.is_none() || now - self.internet_at < OFFLINE_AFTER;

        for (&id, p) in self.peers.iter_mut() {
            // room messages: (re)send what is due
            if let (Some(route), Some(keys)) = (p.route, p.keys.as_ref()) {
                let mut buf = [0u8; MAX_INNER];
                let dropped = p.control.send_due(now, |seq, data| {
                    let n = audio::encode_control(&mut buf, me, seq, data);
                    send_sealed(&self.socket, &self.cfg, keys, route, &buf[..n]);
                });
                if dropped > 0 {
                    tracing::warn!(id, dropped, "room messages never acknowledged");
                }
            }

            // silent for too long: gone without saying bye (crash, network loss)
            if let (Some(heard), false) = (p.last_heard, p.lost_reported) {
                if now - heard > self.cfg.peer_timeout {
                    p.lost_reported = true;
                    tracing::warn!(id, online, "peer went silent");
                    let _ = self.events.send(NetEvent::PeerLost { id, online });
                }
            }

            // just connected over a roundabout path (hairpin through the router, NAT'd IPv4):
            // keep knocking on its more direct addresses for a moment
            if let (Some(route), Some(at)) = (p.route, p.connected_at) {
                if now - at < UPGRADE_FOR && now - p.last_upgrade >= UPGRADE_EVERY {
                    let better: Vec<SocketAddr> = p.candidates.iter().copied().filter(|a| path_rank(*a) < path_rank(route)).collect();
                    if !better.is_empty() {
                        p.last_upgrade = now;
                        let n = audio::encode_punch(&mut out, false, me, self.my_token);
                        for addr in better {
                            let _ = self.socket.send_to(&out[..n], addr);
                        }
                    }
                }
            }

            // connected but silent (an address changed somewhere, ours or theirs): punch
            // its addresses again; a punch that gets through moves the route there
            if p.route.is_some() {
                let silent = p.last_heard.is_some_and(|h| now - h > REPUNCH_AFTER);
                let ours_changed = self.repunch_all_until.is_some_and(|t| now < t);
                if (silent || ours_changed) && now - p.last_repunch >= REPUNCH_EVERY {
                    p.last_repunch = now;
                    let n = audio::encode_punch(&mut out, false, me, self.my_token);
                    for addr in p.candidates.iter().chain(p.route.as_ref()) {
                        let _ = self.socket.send_to(&out[..n], *addr);
                    }
                }
                continue;
            }
            let Some(started) = p.punch_started else { continue };
            if p.failed {
                continue;
            }
            if now - started > self.cfg.punch_timeout {
                // no relay: these two networks cannot reach each other directly
                p.punch_started = None;
                p.failed = true;
                tracing::warn!(id, "hole punching failed");
                let _ = self.events.send(NetEvent::PunchFailed { id });
                continue;
            }
            if now - p.last_punch >= PUNCH_EVERY {
                p.last_punch = now;
                let n = audio::encode_punch(&mut out, false, me, self.my_token);
                for addr in &p.candidates {
                    let _ = self.socket.send_to(&out[..n], *addr);
                }
            }
        }
        self.check_network(now);
        if now - self.last_ping >= PING_EVERY {
            self.last_ping = now;
            self.ping_nonce = self.ping_nonce.wrapping_add(1);
            let n = audio::encode_ping(&mut out, me, self.ping_nonce, now_us());
            for (&id, p) in &self.peers {
                if let Some(route) = p.route {
                    self.send_to_peer(route, id, &out[..n]);
                }
            }
        }
    }

    /// Our own network changed (another Wi-Fi, a cable plugged in, a new IP): ask STUN for
    /// our new public address, tell the room our new addresses, and punch everyone again.
    fn check_network(&mut self, now: Instant) {
        // a STUN question that got no answer: announce what we know without it
        if let Some((_, asked)) = self.stun_pending {
            if now - asked > STUN_WAIT {
                self.stun_pending = None;
                self.announce_addresses(None);
            }
        }
        if now - self.net_check < NET_CHECK_EVERY {
            return;
        }
        self.net_check = now;
        let lan = lan_now();
        if lan.is_none() || lan == self.last_lan {
            return;
        }
        tracing::info!(old = ?self.last_lan, new = ?lan, "our network changed");
        self.last_lan = lan;
        self.repunch_all_until = Some(now + REPUNCH_FOR);
        match self.stun_server {
            Some(server) => {
                let txid: [u8; 12] = rand::random();
                let _ = self.socket.send_to(&crate::stun::request(&txid), server);
                self.stun_pending = Some((txid, now));
            }
            None => self.announce_addresses(None),
        }
    }

    /// Our addresses now (LAN, public from STUN if known, global IPv6) → the room.
    fn announce_addresses(&mut self, public: Option<SocketAddr>) {
        let port = self.socket.port();
        let mut addrs: Vec<SocketAddr> = self.last_lan.map(|ip| SocketAddr::new(ip, port)).into_iter().collect();
        addrs.extend(crate::sock::other_ipv4s(self.last_lan).into_iter().map(|ip| SocketAddr::new(ip.into(), port)));
        addrs.extend(public);
        if self.socket.is_dual_stack() {
            addrs.extend(crate::sock::global_ipv6().map(|ip| SocketAddr::new(ip.into(), port)));
        }
        addrs.dedup();
        tracing::info!(addrs = %crate::logaddr::list(&addrs), "announcing our new addresses");
        let _ = self.events.send(NetEvent::Candidates(addrs));
    }

    fn set_route(&mut self, id: PeerId, addr: SocketAddr) {
        let Some(p) = self.peers.get_mut(&id) else { return };
        if p.route == Some(addr) {
            return;
        }
        if p.route.is_none() {
            p.connected_at = Some(Instant::now());
        }
        p.route = Some(addr);
        tracing::info!(id, addr = %crate::logaddr::Shown(addr), "peer connected");
        let _ = self.events.send(NetEvent::Connected { id, addr });
        self.publish_routes();
    }

    fn publish_routes(&self) {
        // never send to a peer we have no keys for
        let routes: Vec<_> =
            self.peers.iter().filter_map(|(&id, p)| Some((id, p.route?, p.keys.clone()?, p.redundant))).collect();
        self.routes.store(Arc::new(routes));
    }

    fn on_datagram(&mut self, data: &[u8], from: SocketAddr, plain: &mut [u8], frame: &mut Frame) {
        // the answer to "are we online?" (asked while a peer is silent)
        if let (Some(txid), Some(server)) = (self.probe, self.stun_server) {
            if from == server && crate::stun::parse_response(data, &txid).is_some() {
                self.probe = None;
                self.internet_at = Instant::now();
                return;
            }
        }
        // the answer to our "what is our public address now?" question
        if let (Some((txid, _)), Some(server)) = (self.stun_pending, self.stun_server) {
            if from == server {
                if let Some(public) = crate::stun::parse_response(data, &txid) {
                    self.stun_pending = None;
                    self.announce_addresses(Some(public));
                }
                return;
            }
        }
        let Ok(packet) = audio::decode(data) else { return };
        let via = from;
        match packet {
            Packet::Sealed { sender, counter, aad, body } => {
                let Some(p) = self.peers.get_mut(&sender) else { return };
                let Some(keys) = p.keys.as_ref() else { return };
                let Some(n) = keys.open(counter, aad, body, plain) else {
                    tracing::trace!(sender, "dropping packet that failed authentication");
                    return;
                };
                if !p.replay.accept(counter) {
                    return; // duplicate or replayed
                }
                let Ok(inner) = audio::decode(&plain[..n]) else { return };
                let allowed = matches!(
                    inner,
                    Packet::Audio { .. }
                        | Packet::Ping { .. }
                        | Packet::Pong { .. }
                        | Packet::Control { .. }
                        | Packet::ControlAck { .. }
                        | Packet::Bye { .. }
                );
                if packet_sender(&inner) != Some(sender) || !allowed {
                    return;
                }
                p.last_heard = Some(Instant::now());
                self.internet_at = Instant::now();
                if p.lost_reported && !matches!(inner, Packet::Bye { .. }) {
                    p.lost_reported = false;
                    let _ = self.events.send(NetEvent::PeerBack { id: sender });
                }
                // roaming: a packet only this peer could have sealed, and new (the replay
                // window passed it), came from another address: the peer moved (new IP,
                // NAT rebinding). Follow it, as WireGuard does.
                let moved = p.route.is_some_and(|r| r != via) && !matches!(inner, Packet::Bye { .. });
                if moved {
                    tracing::info!(sender, from = %crate::logaddr::opt(p.route), to = %crate::logaddr::Shown(via), "peer moved to a new address");
                    self.set_route(sender, via);
                }
                self.on_packet(inner, via, frame);
            }
            // hole punching stays in the clear (it only carries the signaling token)
            Packet::Punch { .. } | Packet::PunchAck { .. } => self.on_packet(packet, via, frame),
            // anything else unsealed (audio, pings, room messages) is not accepted
            _ => {}
        }
    }

    fn on_packet(&mut self, packet: Packet<'_>, via: SocketAddr, frame: &mut Frame) {
        let me = self.cfg.my_id;
        let mut out = [0u8; 32];
        match packet {
            Packet::Audio { sender, header, payload } => {
                let Some(p) = self.peers.get_mut(&sender) else { return };
                if p.route != Some(via) {
                    return; // only accept audio from the confirmed route
                }
                if !p.seen.first_time(header.seq) {
                    return; // the second copy of a packet we already have
                }
                frame.seq = header.seq;
                frame.frames = header.frames;
                frame.channels = header.channels;
                audio::decode_samples(&header, payload, &mut frame.data);
                if p.tx.push(*frame).is_err() {
                    tracing::trace!(sender, "peer ring full, dropping packet");
                }
                p.tracker.on_packet(header.seq, header.frames, &mut p.stats);
                p.stats.bits = (header.format.bytes() * 8) as u8;
                p.stats.channels = header.channels;
                p.stats.packet_frames = header.frames;
                // out to the app every few packets, if nobody is reading just now
                if p.stats.received % 8 == 0 {
                    if let Ok(mut shared) = self.stats.try_lock() {
                        if let Some(s) = shared.get_mut(&sender) {
                            s.clone_from(&p.stats);
                        }
                    }
                }
            }
            Packet::Punch { sender, token } => {
                let Some(p) = self.peers.get(&sender) else { return };
                if p.token != Some(token) {
                    return;
                }
                let may_route = punch_may_route(p, via);
                let n = audio::encode_punch(&mut out, true, me, self.my_token);
                let _ = self.socket.send_to(&out[..n], via);
                // their packet reached us from `via`, so that address works for sending back
                if may_route {
                    self.set_route(sender, via);
                }
            }
            Packet::PunchAck { sender, token } => {
                if self.peers.get(&sender).is_some_and(|p| p.token == Some(token) && punch_may_route(p, via)) {
                    self.set_route(sender, via);
                }
            }
            Packet::Ping { sender, nonce, sent_us } => {
                if self.peers.get(&sender).is_some_and(|p| p.route == Some(via)) {
                    let n = audio::encode_pong(&mut out, me, nonce, sent_us, now_us());
                    self.send_to_peer(via, sender, &out[..n]);
                }
            }
            Packet::Pong { sender, sent_us, their_us, .. } => {
                let Some(p) = self.peers.get_mut(&sender) else { return };
                if p.route != Some(via) {
                    return;
                }
                let now = now_us();
                let rtt_us = now.saturating_sub(sent_us);
                let rtt = rtt_us as f32 / 1000.0;
                // clock offset (their clock − ours), trusting only near-fastest round trips:
                // a slow one was queued somewhere, so its midpoint is off
                let best = p.clock.best_rtt_us.map_or(rtt_us, |b| b.min(rtt_us));
                let offset = (rtt_us <= best + 1_000).then(|| their_us as i64 - (sent_us + rtt_us / 2) as i64);
                // let the best slowly age, so a route change can raise it again
                p.clock.best_rtt_us = Some(best + 20);
                let s = &mut p.stats;
                s.rtt_ms = Some(match s.rtt_ms {
                    Some(prev) => prev * 0.8 + rtt * 0.2,
                    None => rtt,
                });
                if offset.is_some() {
                    s.clock_offset_us = offset;
                }
                if let Ok(mut shared) = self.stats.try_lock() {
                    if let Some(out) = shared.get_mut(&sender) {
                        out.clone_from(&p.stats);
                    }
                }
            }
            Packet::Control { sender, seq, payload } => {
                // always acknowledge (our earlier ack may have been lost)
                let n = audio::encode_control_ack(&mut out, me, seq);
                self.send_to_peer(via, sender, &out[..n]);
                let Some(p) = self.peers.get_mut(&sender) else { return };
                for data in p.control.on_data(seq, payload) {
                    let _ = self.events.send(NetEvent::Control { id: sender, data });
                }
            }
            Packet::ControlAck { sender, seq } => {
                if let Some(p) = self.peers.get_mut(&sender) {
                    p.control.on_ack(seq);
                }
            }
            Packet::Bye { sender } => {
                if let Some(p) = self.peers.get_mut(&sender) {
                    if !p.lost_reported {
                        p.lost_reported = true; // report the departure once
                        tracing::info!(sender, "peer left");
                        let _ = self.events.send(NetEvent::PeerLeft { id: sender });
                    }
                }
            }
            Packet::Sealed { .. } => {}
        }
    }
}

/// Seals `plain` and sends it to `addr`.
fn send_sealed(socket: &Sock, cfg: &NetConfig, keys: &PeerCrypto, addr: SocketAddr, plain: &[u8]) {
    let mut out = [0u8; MAX_WIRE];
    let n = keys.seal(cfg.my_id, plain, &mut out);
    let _ = socket.send_to(&out[..n], addr);
}

fn packet_sender(p: &Packet<'_>) -> Option<PeerId> {
    // every packet kind carries its sender
    Some(match *p {
        Packet::Audio { sender, .. }
        | Packet::Ping { sender, .. }
        | Packet::Pong { sender, .. }
        | Packet::Punch { sender, .. }
        | Packet::PunchAck { sender, .. }
        | Packet::Control { sender, .. }
        | Packet::ControlAck { sender, .. }
        | Packet::Bye { sender }
        | Packet::Sealed { sender, .. } => sender,
    })
}

/// Our LAN IPv4 toward the internet right now (no packet is sent).
/// Whether a punch packet (unencrypted, carries only the token the room knows) may set
/// this peer's route to `via`. Only while we have no working route: once the peer talks to
/// us sealed, only sealed packets move it (roaming). Otherwise anyone who saw the token
/// (on the path, or the connection server) could send punches and pull our audio away.
///
/// Also, right after connecting: a more direct path (`path_rank`) to one of the addresses the
/// room gave for this peer. That is no hijack: only the peer's own announced addresses.
fn punch_may_route(p: &Peer, via: SocketAddr) -> bool {
    match p.route {
        None => true,
        Some(r) if r == via => true,
        Some(r) if upgrades(p, r, via) => true,
        Some(_) => p.last_heard.is_none_or(|t| t.elapsed() > REPUNCH_AFTER),
    }
}

/// `via` is one of the peer's announced addresses, more direct than the route `r`, and we
/// are still looking for better paths (shortly after connecting).
fn upgrades(p: &Peer, r: SocketAddr, via: SocketAddr) -> bool {
    p.connected_at.is_some_and(|t| t.elapsed() < UPGRADE_FOR + Duration::from_secs(1))
        && p.candidates.contains(&via)
        && path_rank(via) < path_rank(r)
}

fn lan_now() -> Option<IpAddr> {
    crate::local_ip_toward("1.1.1.1:53".parse().expect("valid")).filter(|ip| ip.is_ipv4())
}

#[cfg(test)]
mod path_tests {
    use super::path_rank;

    #[test]
    fn the_most_direct_path_ranks_first() {
        let r = |s: &str| path_rank(s.parse().unwrap());
        assert_eq!(r("192.168.0.7:4000"), 0, "same home network");
        assert_eq!(r("192.168.0.14:4000"), 0);
        assert_eq!(r("10.0.0.2:4000"), 0);
        assert_eq!(r("[2001:db8::1]:4000"), 1, "IPv6: no NAT");
        assert_eq!(r("203.0.113.5:4000"), 2, "public IPv4: through the NATs (hairpin at home)");
        assert!(r("192.168.0.14:4000") < r("[2001:db8::1]:4000") && r("[2001:db8::1]:4000") < r("203.0.113.5:4000"));
    }
}

#[cfg(test)]
mod seq_tests {
    use super::SeqWindow;

    #[test]
    fn second_copies_are_recognised() {
        let mut w = SeqWindow::default();
        for s in 100..110 {
            assert!(w.first_time(s));
        }
        assert!(!w.first_time(108), "copy of a recent packet");
        assert!(w.first_time(110));
        // a lost packet filled by its late copy counts once
        assert!(w.first_time(112));
        assert!(w.first_time(111), "the gap is filled");
        assert!(!w.first_time(111));
        // a restarted stream
        assert!(w.first_time(3));
        assert!(w.first_time(4));
    }
}
