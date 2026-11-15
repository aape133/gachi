//! Minimal STUN client (RFC 5389 Binding): asks a public STUN server which address our UDP
//! socket appears as from the internet. That address is the main candidate peers punch to.
//!
//! Default servers: Cloudflare's (free, unlimited), then Google's as a backup.

use crate::sock::Sock;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

pub const DEFAULT_SERVERS: &[&str] = &["stun.cloudflare.com:3478", "stun.l.google.com:19302"];

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED: u16 = 0x0001;
const ATTR_XOR_MAPPED: u16 = 0x0020;

/// Resolves a STUN server name to an IPv4 address (our public IPv4 is what NATs hide).
pub fn resolve(server: &str) -> Option<SocketAddr> {
    let addrs: Vec<SocketAddr> = server.to_socket_addrs().ok()?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied()
}

pub fn request(txid: &[u8; 12]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // length 0: no attributes
    out[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    out[8..20].copy_from_slice(txid);
    out
}

/// The mapped address in a Binding success response for `txid`, if this is one.
pub fn parse_response(buf: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20 || u16::from_be_bytes([buf[0], buf[1]]) != BINDING_SUCCESS {
        return None;
    }
    if u32::from_be_bytes(buf[4..8].try_into().ok()?) != MAGIC || &buf[8..20] != txid {
        return None;
    }
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let attrs = buf.get(20..20 + len)?;
    let mut plain = None;
    let mut i = 0;
    while i + 4 <= attrs.len() {
        let kind = u16::from_be_bytes([attrs[i], attrs[i + 1]]);
        let alen = u16::from_be_bytes([attrs[i + 2], attrs[i + 3]]) as usize;
        let val = attrs.get(i + 4..i + 4 + alen)?;
        match kind {
            ATTR_XOR_MAPPED => return decode_addr(val, true, txid),
            ATTR_MAPPED => plain = decode_addr(val, false, txid),
            _ => {}
        }
        i += 4 + alen.div_ceil(4) * 4; // attributes are padded to 4 bytes
    }
    plain
}

fn decode_addr(v: &[u8], xor: bool, txid: &[u8; 12]) -> Option<SocketAddr> {
    if v.len() < 8 {
        return None;
    }
    let mut port = u16::from_be_bytes([v[2], v[3]]);
    if xor {
        port ^= (MAGIC >> 16) as u16;
    }
    let ip: IpAddr = match v[1] {
        1 => {
            let mut b: [u8; 4] = v[4..8].try_into().ok()?;
            if xor {
                b.iter_mut().zip(MAGIC.to_be_bytes()).for_each(|(x, m)| *x ^= m);
            }
            Ipv4Addr::from(b).into()
        }
        2 => {
            let mut b: [u8; 16] = v.get(4..20)?.try_into().ok()?;
            if xor {
                let mut mask = [0u8; 16];
                mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
                mask[4..].copy_from_slice(txid);
                b.iter_mut().zip(mask).for_each(|(x, m)| *x ^= m);
            }
            Ipv6Addr::from(b).into()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Builds a Binding success response (tests and local fakes).
pub fn response(txid: &[u8; 12], addr: SocketAddr) -> Vec<u8> {
    let mut val = vec![0u8, 0];
    let xport = addr.port() ^ (MAGIC >> 16) as u16;
    match addr.ip() {
        IpAddr::V4(ip) => {
            val[1] = 1;
            val.extend_from_slice(&xport.to_be_bytes());
            val.extend(ip.octets().iter().zip(MAGIC.to_be_bytes()).map(|(a, m)| a ^ m));
        }
        IpAddr::V6(ip) => {
            val[1] = 2;
            val.extend_from_slice(&xport.to_be_bytes());
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..].copy_from_slice(txid);
            val.extend(ip.octets().iter().zip(mask).map(|(a, m)| a ^ m));
        }
    }
    let mut out = Vec::with_capacity(24 + val.len());
    out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    out.extend_from_slice(&((4 + val.len()) as u16).to_be_bytes());
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.extend_from_slice(txid);
    out.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
    out.extend_from_slice(&(val.len() as u16).to_be_bytes());
    out.extend_from_slice(&val);
    out
}

/// Asks `server` for our public address over `socket` (3 tries, ~1 s in total).
/// Must run before the receive thread owns the socket.
pub fn query(socket: &Sock, server: SocketAddr) -> Option<SocketAddr> {
    query_tries(socket, server, 3)
}

/// How our router maps our socket to the internet, from what two STUN servers saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    /// fewer than two answers
    Unknown,
    /// the same public address whoever we talk to: a peer can reach us at it once we have
    /// sent to that peer (ordinary home routers)
    Steady,
    /// another public port for every destination ("symmetric": carrier NAT on mobile
    /// networks, company firewalls). The address STUN saw is not where a peer will see us.
    /// `step`: how far apart the two ports were (0 = not even the same IP).
    Varies { step: i32 },
}

impl Mapping {
    pub fn from_answers(first: Option<SocketAddr>, second: Option<SocketAddr>) -> Self {
        match (first, second) {
            (Some(a), Some(b)) if a == b => Self::Steady,
            (Some(a), Some(b)) if a.ip() == b.ip() => Self::Varies { step: b.port() as i32 - a.port() as i32 },
            (Some(_), Some(_)) => Self::Varies { step: 0 },
            _ => Self::Unknown,
        }
    }

    /// Where the next peers will probably see us, when the router hands out ports in
    /// order (many do: each new destination gets the last port plus a small step). `last`
    /// is the latest answer. Nothing for routers that pick ports at random.
    pub fn predicted(self, last: SocketAddr, n: usize) -> Vec<SocketAddr> {
        let Self::Varies { step } = self else { return Vec::new() };
        if step == 0 || step.abs() > MAX_STEP {
            return Vec::new();
        }
        (1..=n as i32)
            .filter_map(|k| u16::try_from(last.port() as i32 + step * k).ok().filter(|p| *p >= 1024))
            .map(|port| SocketAddr::new(last.ip(), port))
            .collect()
    }
}

/// Ports further apart than this between two questions asked one after the other were
/// not handed out in order.
const MAX_STEP: i32 = 16;

pub fn query_tries(socket: &Sock, server: SocketAddr, tries: u32) -> Option<SocketAddr> {
    let txid: [u8; 12] = rand::random();
    let req = request(&txid);
    socket.set_read_timeout(Some(Duration::from_millis(330))).ok()?;
    let mut buf = [0u8; 576];
    for _ in 0..tries {
        socket.send_to(&req, server).ok()?;
        let deadline = Instant::now() + Duration::from_millis(330);
        while Instant::now() < deadline {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if from == server => {
                    if let Some(addr) = parse_response(&buf[..n], &txid) {
                        return Some(addr);
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_router_kind_shows_in_two_answers() {
        let a = |s: &str| Some(s.parse::<SocketAddr>().unwrap());
        assert_eq!(Mapping::from_answers(a("203.0.113.5:4000"), None), Mapping::Unknown);
        assert_eq!(Mapping::from_answers(a("203.0.113.5:4000"), a("203.0.113.5:4000")), Mapping::Steady);
        // ports in order: the next ones can be guessed
        let m = Mapping::from_answers(a("203.0.113.5:40000"), a("203.0.113.5:40002"));
        assert_eq!(m, Mapping::Varies { step: 2 });
        assert_eq!(m.predicted(a("203.0.113.5:40002").unwrap(), 2), ["203.0.113.5:40004".parse().unwrap(), "203.0.113.5:40006".parse::<SocketAddr>().unwrap()]);
        // ports at random, or another address altogether: nothing to guess
        let m = Mapping::from_answers(a("203.0.113.5:40000"), a("203.0.113.5:12345"));
        assert!(matches!(m, Mapping::Varies { .. }) && m.predicted(a("203.0.113.5:12345").unwrap(), 2).is_empty());
        assert_eq!(Mapping::from_answers(a("203.0.113.5:40000"), a("203.0.113.9:40000")), Mapping::Varies { step: 0 });
        assert!(Mapping::Steady.predicted(a("203.0.113.5:4000").unwrap(), 2).is_empty());
        // never off the end of the port range
        assert!(Mapping::Varies { step: 16 }.predicted(a("203.0.113.5:65530").unwrap(), 2).is_empty());
    }

    #[test]
    fn roundtrip_v4_and_v6() {
        let txid = [7u8; 12];
        for addr in ["203.0.113.9:40123", "[2001:db8::42]:5000"] {
            let addr: SocketAddr = addr.parse().unwrap();
            assert_eq!(parse_response(&response(&txid, addr), &txid), Some(addr));
        }
        assert_eq!(parse_response(&response(&txid, "1.2.3.4:5".parse().unwrap()), &[8u8; 12]), None, "other txid");
    }

    #[test]
    fn parses_a_real_server_response() {
        // captured shape: XOR-MAPPED-ADDRESS 1.2.3.4:1234 behind a SOFTWARE attribute (padded)
        let txid = [1u8; 12];
        let mut r = response(&txid, "1.2.3.4:1234".parse().unwrap());
        let soft = b"x\0\0\0"; // SOFTWARE "x", padded to 4
        let mut with = r[..20].to_vec();
        with.extend_from_slice(&0x8022u16.to_be_bytes());
        with.extend_from_slice(&1u16.to_be_bytes());
        with.extend_from_slice(soft);
        with.extend_from_slice(&r.split_off(20));
        let len = (with.len() - 20) as u16;
        with[2..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(parse_response(&with, &txid), Some("1.2.3.4:1234".parse().unwrap()));
    }
}
