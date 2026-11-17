//! Tests only: routers (NATs) in software, to see the direct connection get through the
//! kinds of router people have. Two `Net`s on this PC, each behind a `Nat`, each seeing
//! the other only as an address that `Nat` stands in for.
//!
//! How it is built: a host behind a router never sends anywhere directly. Every outside
//! address `X` it talks to is a socket of its router's (`view(X)`, on `X`'s IP, another
//! port): what the host sends there goes out through one of the router's public sockets
//! (its "mapping") to `X`; what reaches a public socket from `S` comes in, if the router's
//! filter lets it, from the router's socket for `S`. Different routers and servers sit on
//! different loopback addresses (127.0.0.N), so "the same IP" means something.
//!
//! The kinds (RFC 4787):
//! - mapping: one public port for everything the host sends (`Independent`, ordinary home
//!   routers, most carrier NAT) or one per destination (`PerDestination`, "symmetric":
//!   handed out in order from a base, as many carrier and company routers do);
//! - filtering: anyone may send to a mapping (`Open`, "full cone"), only addresses the
//!   host sent to (`Address`), only exact address and port (`AddressPort`, "port
//!   restricted", the most common).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    Independent,
    PerDestination,
    /// per destination, the ports picked at random (no guessing where the next one is)
    PerDestinationRandom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Open,
    Address,
    AddressPort,
}

struct Map {
    host: SocketAddr,
    /// the destination this mapping is for (per-destination mapping only)
    dest: Option<SocketAddr>,
    public: Arc<UdpSocket>,
    /// where the host has sent from it (what the filter lets back in)
    sent_to: Vec<SocketAddr>,
}

struct State {
    /// outside address → the router's socket standing in for it
    views: HashMap<SocketAddr, Arc<UdpSocket>>,
    maps: Vec<Map>,
    next_port: u16,
}

struct Inner {
    ip: Ipv4Addr,
    mapping: Mapping,
    filter: Filter,
    state: Mutex<State>,
    stop: Arc<AtomicBool>,
    /// what a host behind it sends to the router's own public address comes back in to the
    /// host it is mapped to ("hairpinning"); without, it is dropped
    hairpin: AtomicBool,
    /// packets dropped by the filter (to see a router at work)
    pub filtered: AtomicU64,
}

#[derive(Clone)]
pub struct Nat(Arc<Inner>);

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn bind(ip: IpAddr, port: u16) -> Option<Arc<UdpSocket>> {
    let s = UdpSocket::bind(SocketAddr::new(ip, port)).ok()?;
    s.set_read_timeout(Some(Duration::from_millis(50))).ok()?;
    Some(Arc::new(s))
}

impl Nat {
    /// A router whose public address is `ip` (a loopback address of its own).
    pub fn new(ip: Ipv4Addr, mapping: Mapping, filter: Filter) -> Self {
        // per-destination ports are handed out in order from a free spot
        let base = UdpSocket::bind((ip, 0)).and_then(|s| s.local_addr()).map(|a| a.port()).unwrap_or(40_000);
        Nat(Arc::new(Inner {
            ip,
            mapping,
            filter,
            state: Mutex::new(State { views: HashMap::new(), maps: Vec::new(), next_port: base }),
            stop: Arc::new(AtomicBool::new(false)),
            hairpin: AtomicBool::new(true),
            filtered: AtomicU64::new(0),
        }))
    }

    /// A router that does not send back in what its hosts send to its own public address.
    pub fn without_hairpin(self) -> Self {
        self.0.hairpin.store(false, Ordering::Relaxed);
        self
    }

    pub fn ip(&self) -> Ipv4Addr {
        self.0.ip
    }

    pub fn filtered(&self) -> u64 {
        self.0.filtered.load(Ordering::Relaxed)
    }

    /// Where a host behind this router sends to reach `outside`.
    pub fn view(&self, outside: SocketAddr) -> SocketAddr {
        let mut st = self.0.state.lock().unwrap();
        if let Some(s) = st.views.get(&outside) {
            return s.local_addr().unwrap();
        }
        let sock = bind(outside.ip(), 0).expect("a socket for the view");
        let addr = sock.local_addr().unwrap();
        st.views.insert(outside, sock.clone());
        drop(st);
        let nat = self.clone();
        let stop = self.0.stop.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while !stop.load(Ordering::Relaxed) {
                if let Ok((n, host)) = sock.recv_from(&mut buf) {
                    nat.outbound(host, outside, &buf[..n]);
                }
            }
        });
        addr
    }

    /// Forgets every mapping (the router restarted, or a carrier NAT moved us): what the
    /// host sends next goes out from new public ports.
    pub fn rebind(&self) {
        self.0.state.lock().unwrap().maps.clear();
    }

    fn outbound(&self, host: SocketAddr, dest: SocketAddr, data: &[u8]) {
        let public = {
            let mut st = self.0.state.lock().unwrap();
            let key = |m: &Map| m.host == host && (self.0.mapping == Mapping::Independent || m.dest == Some(dest));
            let i = match st.maps.iter().position(key) {
                Some(i) => i,
                None => {
                    let public = match self.0.mapping {
                        Mapping::Independent | Mapping::PerDestinationRandom => bind(IpAddr::V4(self.0.ip), 0),
                        // the next free port in order
                        Mapping::PerDestination => loop {
                            st.next_port = st.next_port.wrapping_add(1).max(1024);
                            if let Some(s) = bind(IpAddr::V4(self.0.ip), st.next_port) {
                                break Some(s);
                            }
                        },
                    }
                    .expect("a public socket");
                    self.listen(public.clone());
                    st.maps.push(Map { host, dest: Some(dest), public, sent_to: Vec::new() });
                    st.maps.len() - 1
                }
            };
            let m = &mut st.maps[i];
            if !m.sent_to.contains(&dest) {
                m.sent_to.push(dest);
            }
            m.public.clone()
        };
        let _ = public.send_to(data, dest);
    }

    /// Packets reaching a public socket.
    fn listen(&self, public: Arc<UdpSocket>) {
        let nat = self.clone();
        let stop = self.0.stop.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let me = public.local_addr().unwrap();
            while !stop.load(Ordering::Relaxed) {
                if let Ok((n, from)) = public.recv_from(&mut buf) {
                    nat.inbound(me, from, &buf[..n]);
                }
            }
        });
    }

    fn inbound(&self, public: SocketAddr, from: SocketAddr, data: &[u8]) {
        if from.ip() == IpAddr::V4(self.0.ip) && !self.0.hairpin.load(Ordering::Relaxed) {
            self.0.filtered.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let host = {
            let st = self.0.state.lock().unwrap();
            // (a mapping the router forgot lets nothing in)
            let Some(m) = st.maps.iter().find(|m| m.public.local_addr().ok() == Some(public)) else { return };
            let allowed = match self.0.filter {
                Filter::Open => true,
                Filter::Address => m.sent_to.iter().any(|d| d.ip() == from.ip()),
                Filter::AddressPort => m.sent_to.contains(&from),
            };
            if !allowed {
                self.0.filtered.fetch_add(1, Ordering::Relaxed);
                return;
            }
            m.host
        };
        // in from the router's socket for the sender
        let via = self.view(from);
        let sock = self.0.state.lock().unwrap().views.values().find(|s| s.local_addr().ok() == Some(via)).cloned();
        if let Some(s) = sock {
            let _ = s.send_to(data, host);
        }
    }
}

/// Of a peer's candidate addresses, the ones the rest of the internet can reach: those on
/// its router's public address (the LAN address, the PC's other adapters and its IPv6 are
/// not reachable from another home here), each as the other side's router shows it.
pub fn reachable(candidates: &[SocketAddr], their_router: &Nat, our_router: &Nat) -> Vec<SocketAddr> {
    candidates.iter().filter(|a| a.ip() == IpAddr::V4(their_router.ip())).map(|a| our_router.view(*a)).collect()
}

/// A STUN server of our own on `ip`: answers with the address the request came from.
pub fn stun_at(ip: Ipv4Addr) -> SocketAddr {
    let sock = UdpSocket::bind((ip, 0)).unwrap();
    let addr = sock.local_addr().unwrap();
    thread::spawn(move || {
        let mut buf = [0u8; 576];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if n >= 20 {
                let txid: [u8; 12] = buf[8..20].try_into().unwrap();
                let _ = sock.send_to(&crate::stun::response(&txid, from), from);
            }
        }
    });
    addr
}
