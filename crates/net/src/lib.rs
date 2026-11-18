//! P2P UDP transport. Peers only ever talk directly; nothing is relayed.
//!
//! One UDP socket per session, used for everything:
//! - **Gathering** ([`Gathered`]): before joining, we open the socket, ask a public STUN
//!   server for our public address, and make our session keys. A newcomer sends all of
//!   that with its join request, so it can be introduced before it even has an id.
//! - **Audio out**: [`UdpSender`] is the engine's [`PacketSink`]; it encodes and sends from
//!   the input callback itself, so there is no thread hop before the packet leaves.
//! - **Receive thread**: decodes packets, feeds each peer's jitter-buffer ring, answers
//!   pings and punches, keeps per-peer stats, and carries room messages reliably.
//! - **Hole punching**: both sides fire `Punch` packets at every candidate address at the same
//!   time; the first address that gets through (either direction) becomes the peer's route.
//!   If none does, the two simply cannot connect ([`NetEvent::PunchFailed`]).
//! - **Encryption**: everything but punches is sealed per peer with ChaCha20-Poly1305 (keys
//!   from an X25519 exchange, see [`crypto`]). Unsealed audio is dropped.
//! - **IPv6**: the socket is dual-stack; a global IPv6 address is offered as a candidate.
//! - **UPnP / NAT-PMP**: in the background we ask the router to forward our port; if it
//!   agrees, the public address becomes an extra candidate ([`NetEvent::Candidates`]).
//! - **Best path**: the first address that gets through becomes the route, then for 3 s the
//!   peer's more direct addresses are tried too (same network > IPv6 > public IPv4) and the
//!   route moves to the best one that answers; the peer follows (roaming).

pub mod prio;
mod control;
pub mod crypto;
pub mod joinseal;
pub mod logaddr;
mod recv;
mod sock;
mod stats;
pub mod stun;
pub mod natpmp;
pub mod upnp;
pub mod wifi;

pub use stats::{buffer_level, PeerNetStats, WORTH_BALANCED, WORTH_FAST, WORTH_STEADY};

use arc_swap::ArcSwap;
use crypto::{Identity, PeerCrypto};
use gachi_engine::{Frame, PacketSink};
use gachi_proto::audio::{self, SampleFormat, MAX_PACKET, MAX_WIRE};
use gachi_proto::PeerId;
use sock::Sock;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct NetConfig {
    pub my_id: PeerId,
    pub sample_rate: u32,
    pub packet_frames: u16,
    pub format: SampleFormat,
    /// how long to try hole punching before giving up on a peer
    pub punch_timeout: Duration,
    /// ask the router (UPnP) to forward our port
    pub upnp: bool,
    /// a connected peer silent this long is reported as [`NetEvent::PeerLost`]
    pub peer_timeout: Duration,
}

impl NetConfig {
    pub const DEFAULT_PUNCH_TIMEOUT: Duration = Duration::from_secs(8);
    pub const DEFAULT_PEER_TIMEOUT: Duration = Duration::from_secs(10);
}

#[derive(Debug)]
pub enum NetEvent {
    /// Punching got through: we talk to the peer at `addr`.
    Connected { id: PeerId, addr: SocketAddr },
    /// No candidate address got through within the punch timeout.
    PunchFailed { id: PeerId },
    /// We have new addresses (e.g. the router mapped a port): announce them again.
    Candidates(Vec<SocketAddr>),
    /// A room message from a peer (reliable, in order; see `gachi_proto::peer`).
    Control { id: PeerId, data: Vec<u8> },
    /// The peer said goodbye.
    PeerLeft { id: PeerId },
    /// The peer went silent without saying goodbye (crash, network loss).
    /// `online`: our own internet answered meanwhile (a STUN server did), so the silence is
    /// the peer's. `false`: nothing answered: it is we who are cut off.
    PeerLost { id: PeerId, online: bool },
    /// We hear a lost peer again.
    PeerBack { id: PeerId },
}

pub(crate) enum Ctl {
    AddPeer { id: PeerId, tx: rtrb::Producer<Frame> },
    /// new mixer input for an existing peer (after the audio engine restarted); route is kept
    SetOutput { id: PeerId, tx: rtrb::Producer<Frame> },
    /// the peer's sample rate (from its status), for timing stats
    SetRate { id: PeerId, sample_rate: u32 },
    Candidates { id: PeerId, addrs: Vec<SocketAddr>, token: u32, key: crypto::PeerKey },
    SendControl { id: PeerId, data: Vec<u8> },
    /// also send each packet a second time, a little later (the peer reports loss)
    SetRedundancy { id: PeerId, on: bool },
    RemovePeer(PeerId),
    /// say bye to everyone and stop
    Goodbye,
}

/// Confirmed routes the sender uses: peer, address, keys, and whether to send each packet
/// twice (see [`REDUNDANT_LAG`]). Swapped atomically; the audio thread only loads it.
pub(crate) type Routes = Arc<ArcSwap<Vec<(PeerId, SocketAddr, Arc<PeerCrypto>, bool)>>>;

/// On a lossy link the sender repeats each packet this many packets later. Losses come in
/// short bursts, so the copy should not travel right next to the original; two packets
/// (1.3 ms at 32 frames) is still well inside any jitter buffer, so it costs no delay.
pub const REDUNDANT_LAG: usize = 2;

/// A socket with our addresses and session keys, ready before we know our id.
pub struct Gathered {
    socket: Sock,
    identity: Arc<Identity>,
    lan_ip: Option<IpAddr>,
    /// addresses peers should try (LAN, public via STUN, IPv6)
    pub candidates: Vec<SocketAddr>,
    /// whether a STUN server told us our public address (if not, only LAN/IPv6 peers connect)
    pub public: Option<SocketAddr>,
    /// how the router maps us (see [`stun::Mapping`])
    pub mapping: stun::Mapping,
    /// the STUN server that told us `public` (asked again while a peer is silent: see
    /// `recv`)
    stun: Option<SocketAddr>,
    /// put in our punch packets so peers only accept us
    pub token: u32,
    /// our X25519 public key for this session
    pub public_key: [u8; 32],
}

impl Gathered {
    /// What peers need to encrypt to us: our public key and whether we have AES in hardware.
    pub fn peer_key(&self) -> crypto::PeerKey {
        crypto::PeerKey { public: self.public_key, aes: crypto::aes_in_hardware() }
    }

    /// Binds the socket and collects candidates. `stun_servers` are tried in order
    /// (`host:port`), see [`stun::DEFAULT_SERVERS`].
    pub fn gather(stun_servers: &[String]) -> io::Result<Self> {
        let socket = Sock::bind()?;
        let mut public = None;
        let mut asked: Option<SocketAddr> = None;
        let mut second = None;
        let mut lan_ip = None;
        for server in stun_servers {
            let Some(addr) = stun::resolve(server) else { continue };
            lan_ip = lan_ip.or_else(|| local_ip_toward(addr).filter(|ip| ip.is_ipv4()));
            match asked {
                None => {
                    if let Some(p) = stun::query(&socket, addr) {
                        public = Some(p);
                        asked = Some(addr);
                    }
                }
                // one more server, elsewhere: the same answer means peers will see us
                // there too (one try: joining does not wait on it)
                Some(first) if first.ip() != addr.ip() => {
                    second = stun::query_tries(&socket, addr, 1);
                    break;
                }
                Some(_) => {}
            }
        }
        let mapping = stun::Mapping::from_answers(public, second);
        // offline or STUN blocked: still find our LAN address
        let lan_ip = lan_ip.or_else(|| local_ip_toward("1.1.1.1:53".parse().expect("valid")).filter(|ip| ip.is_ipv4()));
        let local = lan_ip.map(|ip| SocketAddr::new(ip, socket.port()));
        // a global IPv6 address usually needs no NAT traversal at all
        let v6 = socket
            .is_dual_stack()
            .then(sock::global_ipv6)
            .flatten()
            .map(|ip| SocketAddr::new(ip.into(), socket.port()));
        // other adapters (a VPN both sides are on, a second card)
        let others = sock::other_ipv4s(lan_ip).into_iter().map(|ip| SocketAddr::new(ip.into(), socket.port()));
        // a router that gives every destination its own port, in order: where the next
        // peers will probably see us
        let guessed = second.map(|s| mapping.predicted(s, 2)).unwrap_or_default();
        let mut candidates: Vec<SocketAddr> = local.into_iter().chain(others).chain(public).chain(guessed).chain(v6).collect();
        candidates.dedup();
        tracing::info!(candidates = %logaddr::list(&candidates), ?mapping, "udp candidates");
        if matches!(mapping, stun::Mapping::Varies { .. }) {
            tracing::warn!("the router gives each destination its own port (symmetric NAT): peers behind ordinary routers may not get through");
        }
        if public.is_none() {
            tracing::warn!("no STUN server answered; only LAN/IPv6 peers will connect");
        }
        let identity = Arc::new(Identity::generate());
        Ok(Self { socket, public_key: identity.public, identity, lan_ip, candidates, public, mapping, stun: asked, token: rand::random() })
    }
}

pub struct Net {
    socket: Arc<Sock>,
    cfg: NetConfig,
    routes: Routes,
    ctl_tx: mpsc::Sender<Ctl>,
    stats: Arc<Mutex<HashMap<PeerId, PeerNetStats>>>,
    events_rx: mpsc::Receiver<NetEvent>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    /// put in our punch packets; peers learn it from the room
    pub token: u32,
    /// addresses peers should try
    pub candidates: Vec<SocketAddr>,
    /// how the router maps us (see [`stun::Mapping`])
    pub mapping: stun::Mapping,
    /// our public address as a STUN server saw it when we started (`None`: none answered)
    pub public: Option<SocketAddr>,
    /// our X25519 public key for this session
    pub public_key: [u8; 32],
}

impl Net {
    /// Our public key and AES support, as peers need them.
    pub fn peer_key(&self) -> crypto::PeerKey {
        crypto::PeerKey { public: self.public_key, aes: crypto::aes_in_hardware() }
    }

    /// Starts the receive thread on a gathered socket (and the UPnP helper if enabled).
    pub fn start(g: Gathered, cfg: NetConfig) -> io::Result<Self> {
        let Gathered { socket, identity, lan_ip, candidates, token, public_key, mapping, public, stun, .. } = g;
        socket.set_read_timeout(Some(Duration::from_millis(5)))?;
        // sends never wait (they run on the audio callback); the receive thread waits for
        // datagrams in `wait_readable`
        socket.set_nonblocking()?;
        let socket = Arc::new(socket);
        let routes: Routes = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let stats = Arc::new(Mutex::new(HashMap::new()));
        let (ctl_tx, ctl_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        let worker = recv::Receiver::new(
            socket.clone(),
            cfg.clone(),
            token,
            identity,
            routes.clone(),
            stats.clone(),
            ctl_rx,
            events_tx.clone(),
            stop.clone(),
            stun,
            public,
        );
        let thread = thread::Builder::new().name("gachi-net-recv".into()).spawn(move || {
            prio::pro_audio();
            worker.run()
        })?;

        // a public address on the PC itself (plugged into the modem or wall jack): no router
        // to open a port on, and nothing to open
        if let (true, Some(ip)) = (cfg.upnp, lan_ip.filter(|ip| behind_router(*ip))) {
            let (port, stop, events, mut cands) = (socket.port(), stop.clone(), events_tx.clone(), candidates.clone());
            let _ = thread::Builder::new().name("gachi-upnp".into()).spawn(move || {
                upnp::run(ip, port, stop, |public| {
                    if !cands.contains(&public) {
                        cands.push(public);
                        let _ = events.send(NetEvent::Candidates(cands));
                    }
                });
            });
        }

        Ok(Self { socket, cfg, routes, ctl_tx, stats, events_rx, stop, thread: Some(thread), token, candidates, mapping, public, public_key })
    }

    /// The packet sink to hand to the audio engine (PCM in the configured format).
    pub fn sender(&self) -> UdpSender {
        self.sender_with(self.cfg.format)
    }

    /// Same, with another sample format (quality can change mid-session; receivers read the
    /// format from each packet). Always uncompressed PCM: no codec, no codec delay.
    pub fn sender_with(&self, format: SampleFormat) -> UdpSender {
        UdpSender {
            socket: self.socket.clone(),
            routes: self.routes.clone(),
            my_id: self.cfg.my_id,
            format,
            history: Box::new([([0u8; MAX_PACKET], 0); REDUNDANT_LAG + 1]),
            next: 0,
            wire: Box::new([0u8; MAX_WIRE]),
        }
    }

    /// The peer's sample rate (it reports it in the room).
    pub fn set_peer_rate(&self, id: PeerId, sample_rate: u32) {
        let _ = self.ctl_tx.send(Ctl::SetRate { id, sample_rate });
    }

    /// Points an existing peer's audio at a new mixer input, keeping its route and stats.
    pub fn set_peer_output(&self, id: PeerId, tx: rtrb::Producer<Frame>) {
        let _ = self.ctl_tx.send(Ctl::SetOutput { id, tx });
    }

    /// Registers a peer; decoded audio from it goes into `tx`. Punching starts once
    /// its candidates are known ([`Net::set_candidates`]).
    pub fn add_peer(&self, id: PeerId, tx: rtrb::Producer<Frame>) {
        let _ = self.ctl_tx.send(Ctl::AddPeer { id, tx });
    }

    /// The peer's addresses, punch token and public key (with its AES support).
    pub fn set_candidates(&self, id: PeerId, addrs: Vec<SocketAddr>, token: u32, key: crypto::PeerKey) {
        let _ = self.ctl_tx.send(Ctl::Candidates { id, addrs, token, key });
    }

    /// Send our audio to this peer twice (it reported packet loss), or stop doing so.
    pub fn set_redundancy(&self, id: PeerId, on: bool) {
        let _ = self.ctl_tx.send(Ctl::SetRedundancy { id, on });
    }

    /// Sends a room message to one peer, reliably and in order (it waits for the route and
    /// keys if they are not ready yet). At most `gachi_proto::audio::MAX_CONTROL_PAYLOAD` bytes.
    pub fn send_control(&self, id: PeerId, data: Vec<u8>) {
        if data.len() <= gachi_proto::audio::MAX_CONTROL_PAYLOAD {
            let _ = self.ctl_tx.send(Ctl::SendControl { id, data });
        } else {
            tracing::warn!(len = data.len(), "room message too long, not sent");
        }
    }

    pub fn remove_peer(&self, id: PeerId) {
        let _ = self.ctl_tx.send(Ctl::RemovePeer(id));
    }

    pub fn stats(&self, id: PeerId) -> Option<PeerNetStats> {
        self.stats.lock().ok()?.get(&id).cloned()
    }

    pub fn try_event(&self) -> Option<NetEvent> {
        self.events_rx.try_recv().ok()
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        // the receive thread says bye to every peer, then exits
        if self.ctl_tx.send(Ctl::Goodbye).is_err() {
            self.stop.store(true, Ordering::Relaxed);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.stop.store(true, Ordering::Relaxed); // UPnP thread: remove the port mapping
    }
}

/// Engine → network. Runs on the input audio callback: encode once, then seal for each
/// confirmed peer (its own key) into a preallocated buffer and `send_to`.
/// No locks, no allocation.
pub struct UdpSender {
    socket: Arc<Sock>,
    routes: Routes,
    my_id: PeerId,
    format: SampleFormat,
    /// the last few encoded packets (a ring), for the late copies on lossy links
    history: Box<[([u8; MAX_PACKET], usize); REDUNDANT_LAG + 1]>,
    next: usize,
    wire: Box<[u8; MAX_WIRE]>,
}

impl PacketSink for UdpSender {
    fn send(&mut self, frame: &Frame) {
        let routes = self.routes.load();
        if routes.is_empty() {
            return;
        }
        let slot = self.next;
        self.next = (self.next + 1) % self.history.len();
        let (plain, len) = &mut self.history[slot];
        *len = audio::encode_audio(&mut plain[..], self.my_id, frame.seq, frame.channels, self.format, frame.samples());
        let n = *len;
        // the packet from REDUNDANT_LAG packets ago (the slot about to be reused next-but-…)
        let old = (slot + self.history.len() - REDUNDANT_LAG) % self.history.len();
        for (_, addr, keys, redundant) in routes.iter() {
            let sealed = keys.seal(self.my_id, &self.history[slot].0[..n], &mut self.wire[..]);
            let _ = self.socket.send_to(&self.wire[..sealed], *addr);
            let (copy, copy_len) = &self.history[old];
            if *redundant && *copy_len > 0 {
                let sealed = keys.seal(self.my_id, &copy[..*copy_len], &mut self.wire[..]);
                let _ = self.socket.send_to(&self.wire[..sealed], *addr);
            }
        }
    }
}

/// Network check for the settings screen: asks the STUN servers for our public address on
/// a throwaway socket. Returns `(local, public, how the router maps us)`, or `None` if none
/// answered.
pub fn probe(stun_servers: &[String]) -> io::Result<Option<(SocketAddr, SocketAddr, stun::Mapping)>> {
    let g = Gathered::gather(stun_servers)?;
    let local = g.lan_ip.map(|ip| SocketAddr::new(ip, g.socket.port()));
    Ok(g.public.map(|p| (local.unwrap_or(p), p, g.mapping)))
}

/// A value every device behind the same router computes alike, and nobody else: the
/// router's hardware address (the default gateway's MAC, which only devices on its network
/// see), stretched like a password. "Nearby rooms" are shown only to those who bring the
/// same value, so the connection server need not keep anyone's address to tell who is on
/// the same network. The stretching (100,000 rounds) keeps the server's owner from trying
/// every possible router address against a stored value. `None` without a router (plugged
/// straight into the modem, a dial-up or VPN link).
pub fn router_key() -> Option<String> {
    use std::sync::Mutex;
    static LAST: Mutex<Option<(String, String)>> = Mutex::new(None);
    let mac = netdev::get_default_gateway().ok()?.mac_addr.to_string().to_lowercase();
    if mac.chars().all(|c| c == '0' || c == ':' || c == '-') {
        return None;
    }
    let mut last = LAST.lock().ok()?;
    if let Some((m, key)) = last.as_ref() {
        if *m == mac {
            return Some(key.clone());
        }
    }
    let key: String = joinseal::password_key("gachi nearby v1", &mac)[..16].iter().map(|b| format!("{b:02x}")).collect();
    *last = Some((mac, key.clone()));
    Some(key)
}

/// Does this machine have IPv6 internet?
pub fn has_ipv6() -> bool {
    sock::global_ipv6().is_some()
}

/// The local IP the OS would use to reach `target` (UDP `connect` sends nothing).
pub fn local_ip_toward(target: SocketAddr) -> Option<IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect(target).ok()?;
    let ip = probe.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// Microseconds since the first call (monotonic, this process). Ping timestamps and the
/// host's metronome clock are in this unit; peers convert with the measured clock offset.
pub fn now_us() -> u64 {
    use std::sync::OnceLock;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod nat_sim;

/// Whether this local address sits behind a NAT router (private, or the carrier-grade
/// 100.64/10 range), as opposed to a public address handed straight to the PC.
pub fn behind_router(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private() || v4.is_link_local() || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(_) => false,
    }
}

/// Tests only: counts allocator calls on the calling thread inside [`alloc_guard::count`]
/// (as in gachi-engine): sending runs on the input audio callback and must not allocate.
#[cfg(test)]
pub(crate) mod alloc_guard {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicI64, Ordering::Relaxed};

    /// bytes held from the allocator right now, all threads (see [`live_bytes`])
    static LIVE: AtomicI64 = AtomicI64::new(0);

    thread_local! {
        static WATCHING: Cell<bool> = const { Cell::new(false) };
        static CALLS: Cell<u64> = const { Cell::new(0) };
    }

    pub struct Counting;

    fn note() {
        let _ = WATCHING.try_with(|w| {
            if w.get() {
                let _ = CALLS.try_with(|c| c.set(c.get() + 1));
            }
        });
    }

    // SAFETY: every call goes to the system allocator unchanged
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note();
            let p = System.alloc(layout);
            if !p.is_null() {
                LIVE.fetch_add(layout.size() as i64, Relaxed);
            }
            p
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            note();
            LIVE.fetch_sub(layout.size() as i64, Relaxed);
            System.dealloc(ptr, layout)
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note();
            let p = System.realloc(ptr, layout, new_size);
            if !p.is_null() {
                LIVE.fetch_add(new_size as i64 - layout.size() as i64, Relaxed);
            }
            p
        }
    }

    /// Bytes the whole process holds from the allocator (for leak tests: run alone).
    pub fn live_bytes() -> i64 {
        LIVE.load(Relaxed)
    }

    pub fn count(f: impl FnOnce()) -> u64 {
        CALLS.with(|c| c.set(0));
        WATCHING.with(|w| w.set(true));
        f();
        WATCHING.with(|w| w.set(false));
        CALLS.with(|c| c.get())
    }
}

#[cfg(test)]
#[global_allocator]
static ALLOC: alloc_guard::Counting = alloc_guard::Counting;
