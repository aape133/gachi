//! Addresses as they appear in log files. People send their logs to others to diagnose a
//! problem, so public addresses (which point at a home) are cut short; the kind of route
//! (LAN, public IPv4, IPv6) and the port, which diagnosis needs, stay.
//!
//! - private and loopback IPv4 in full: `192.168.0.7:40000`
//! - public IPv4: first two parts only: `203.0.x.x:40000`
//! - IPv6: the first group only: `[2001:…]:40000` (link-local / loopback in full)

use std::fmt;
use std::net::{IpAddr, SocketAddr};

/// An address for the log (see the module notes).
pub struct Shown(pub SocketAddr);

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let port = self.0.port();
        match self.0.ip() {
            IpAddr::V4(ip) if ip.is_private() || ip.is_loopback() || ip.is_link_local() => write!(f, "{ip}:{port}"),
            IpAddr::V4(ip) => {
                let [a, b, ..] = ip.octets();
                write!(f, "{a}.{b}.x.x:{port}")
            }
            IpAddr::V6(ip) if ip.is_loopback() || ip.is_unicast_link_local() => write!(f, "[{ip}]:{port}"),
            IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
                Some(v4) => Shown(SocketAddr::new(v4.into(), port)).fmt(f),
                None => write!(f, "[{:x}:…]:{port}", ip.segments()[0]),
            },
        }
    }
}

/// `-` when there is none.
pub fn opt(addr: Option<SocketAddr>) -> String {
    addr.map_or_else(|| "-".into(), |a| Shown(a).to_string())
}

pub fn list(addrs: &[SocketAddr]) -> String {
    let parts: Vec<String> = addrs.iter().map(|a| Shown(*a).to_string()).collect();
    format!("[{}]", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(a: &str) -> String {
        Shown(a.parse().unwrap()).to_string()
    }

    #[test]
    fn public_addresses_are_cut_short() {
        assert_eq!(s("192.168.0.7:40000"), "192.168.0.7:40000");
        assert_eq!(s("10.1.2.3:5"), "10.1.2.3:5");
        assert_eq!(s("127.0.0.1:9"), "127.0.0.1:9");
        assert_eq!(s("203.0.113.77:40000"), "203.0.x.x:40000");
        assert_eq!(s("[2001:db8::1]:40000"), "[2001:…]:40000");
        assert_eq!(s("[::ffff:203.0.113.77]:1"), "203.0.x.x:1");
        assert_eq!(s("[fe80::1]:2"), "[fe80::1]:2");
        assert_eq!(opt(None), "-");
        assert_eq!(list(&["1.2.3.4:5".parse().unwrap(), "192.168.1.2:5".parse().unwrap()]), "[1.2.x.x:5, 192.168.1.2:5]");
    }
}
