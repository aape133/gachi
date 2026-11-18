//! NAT-PMP (RFC 6886): ask the router to forward our UDP port. A second way next to UPnP,
//! for routers that have UPnP off or only speak NAT-PMP (Apple, some ISP routers).
//!
//! Two small UDP questions to the default gateway on port 5351: "what is your public
//! address?" and "map this UDP port". The mapping is leased; we renew it at half time and
//! delete it (lifetime 0) when the session ends.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

pub const PORT: u16 = 5351;
/// Lease we ask for (the router may give less; we renew at half of what it gives).
pub const LIFETIME_SECS: u32 = 3600;

/// The default gateway's IPv4 address (where a NAT-PMP router listens).
pub fn gateway() -> Option<Ipv4Addr> {
    // (a VPN's adapter routes everything "on link": its gateway reads 0.0.0.0, which is no
    // router to ask, and sending there fails at once)
    netdev::get_default_gateway().ok()?.ipv4.iter().copied().find(|ip| !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_loopback())
}

/// One request/response exchange, retried with growing waits (RFC: 250 ms, doubling).
fn ask(router: SocketAddr, req: &[u8], expect_op: u8, resp: &mut [u8]) -> Option<usize> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    let mut wait = Duration::from_millis(250);
    for _ in 0..3 {
        sock.set_read_timeout(Some(wait)).ok()?;
        sock.send_to(req, router).ok()?;
        while let Ok((n, from)) = sock.recv_from(resp) {
            // version 0, our opcode + 128, result code 0 (success)
            if from == router && n >= 4 && resp[0] == 0 && resp[1] == 128 + expect_op {
                return (u16::from_be_bytes([resp[2], resp[3]]) == 0).then_some(n);
            }
        }
        wait *= 2;
    }
    None
}

/// The router's public IPv4 address.
pub fn external_ip(router: SocketAddr) -> Option<Ipv4Addr> {
    let mut resp = [0u8; 16];
    let n = ask(router, &[0, 0], 0, &mut resp)?;
    (n >= 12).then(|| Ipv4Addr::new(resp[8], resp[9], resp[10], resp[11]))
}

/// Maps our UDP `internal` port (asking for the same port outside). Returns the external
/// port and the lease in seconds. `lifetime` 0 deletes the mapping.
pub fn map_udp(router: SocketAddr, internal: u16, lifetime: u32) -> Option<(u16, u32)> {
    let mut req = [0u8; 12];
    req[1] = 1; // map UDP
    req[4..6].copy_from_slice(&internal.to_be_bytes());
    req[6..8].copy_from_slice(&internal.to_be_bytes()); // suggested external port
    req[8..12].copy_from_slice(&lifetime.to_be_bytes());
    let mut resp = [0u8; 16];
    let n = ask(router, &req, 1, &mut resp)?;
    if n < 16 || u16::from_be_bytes([resp[8], resp[9]]) != internal {
        return None;
    }
    Some((u16::from_be_bytes([resp[10], resp[11]]), u32::from_be_bytes([resp[12], resp[13], resp[14], resp[15]])))
}

/// Public address for our `port` through NAT-PMP, and the lease (seconds).
pub fn map(router: SocketAddr, port: u16) -> Option<(SocketAddrV4, u32)> {
    let ip = external_ip(router)?;
    let (ext, lease) = map_udp(router, port, LIFETIME_SECS)?;
    Some((SocketAddrV4::new(ip, ext), lease))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// A tiny NAT-PMP router: public IP 203.0.113.7, maps any port to port + 1000.
    fn fake_router() -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 64];
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                match (n, buf[1]) {
                    (2, 0) => {
                        let mut r = [0u8; 12];
                        r[1] = 128;
                        r[8..12].copy_from_slice(&[203, 0, 113, 7]);
                        let _ = sock.send_to(&r, from);
                    }
                    (12, 1) => {
                        let internal = u16::from_be_bytes([buf[4], buf[5]]);
                        let mut r = [0u8; 16];
                        r[1] = 129;
                        r[8..10].copy_from_slice(&internal.to_be_bytes());
                        r[10..12].copy_from_slice(&(internal + 1000).to_be_bytes());
                        r[12..16].copy_from_slice(&buf[8..12]); // the lifetime asked for
                        let _ = sock.send_to(&r, from);
                    }
                    _ => {}
                }
            }
        });
        addr
    }

    #[test]
    fn maps_a_port_and_reports_the_public_address() {
        let router = fake_router();
        let (public, lease) = map(router, 4242).expect("mapped");
        assert_eq!(public, "203.0.113.7:5242".parse().unwrap());
        assert_eq!(lease, LIFETIME_SECS);
        assert_eq!(map_udp(router, 4242, 0), Some((5242, 0)), "delete");
    }

    #[test]
    fn no_router_answers_quickly_with_none() {
        // nothing listens here: three short tries, then give up
        let t = std::time::Instant::now();
        assert!(external_ip("127.0.0.1:9".parse().unwrap()).is_none());
        assert!(t.elapsed() < Duration::from_secs(3));
    }
}
