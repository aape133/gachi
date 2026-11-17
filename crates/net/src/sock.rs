//! One UDP socket for IPv4 and IPv6 (dual-stack), so peers with IPv6 can often connect
//! directly with no NAT in the way.
//!
//! On a dual-stack socket the OS shows IPv4 peers as IPv4-mapped IPv6 addresses
//! (`::ffff:1.2.3.4`). Everything outside this module sees plain addresses: we map on the
//! way out and unmap on the way in, so comparing routes just works.

use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::time::Duration;

pub struct Sock {
    inner: UdpSocket,
    v6: bool,
}

impl Sock {
    /// Dual-stack `[::]:0`, or IPv4-only `0.0.0.0:0` where IPv6 is unavailable.
    pub fn bind() -> io::Result<Self> {
        match Self::bind_dual() {
            Ok(s) => Ok(s),
            Err(e) => {
                tracing::info!("no IPv6 socket ({e}), using IPv4 only");
                let inner = UdpSocket::bind("0.0.0.0:0")?;
                let s = Socket::from(inner);
                tune(&s, false);
                Ok(Self { inner: s.into(), v6: false })
            }
        }
    }

    fn bind_dual() -> io::Result<Self> {
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_only_v6(false)?;
        s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)).into())?;
        tune(&s, true);
        Ok(Self { inner: s.into(), v6: true })
    }

    pub fn is_dual_stack(&self) -> bool {
        self.v6
    }

    pub fn port(&self) -> u16 {
        self.inner.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let to = match (self.v6, addr) {
            (true, SocketAddr::V4(a)) => SocketAddr::V6(SocketAddrV6::new(a.ip().to_ipv6_mapped(), a.port(), 0, 0)),
            (false, SocketAddr::V6(_)) => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "IPv6 address on an IPv4 socket"));
            }
            (_, a) => a,
        };
        self.inner.send_to(buf, to)
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (n, from) = self.inner.recv_from(buf)?;
        Ok((n, canonical(from)))
    }

    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(t)
    }

    /// From now on no call on this socket waits: a send the system has no room for fails
    /// with `WouldBlock` (the packet is dropped) instead of holding up the caller. Audio is
    /// sent from the audio callback: a send that waited for the kernel's send buffer (full
    /// when the link stalls with many peers) stopped our own sound with it. Reading waits in
    /// [`Sock::wait_readable`] instead. (Windows; elsewhere sockets stay as they are.)
    pub fn set_nonblocking(&self) -> io::Result<()> {
        if cfg!(windows) {
            self.inner.set_nonblocking(true)
        } else {
            Ok(())
        }
    }

    /// Waits up to `timeout` for a datagram to read; true when one (or an error to read)
    /// is there. Wakes as soon as one arrives.
    #[cfg(windows)]
    pub fn wait_readable(&self, timeout: Duration) -> bool {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{WSAPoll, POLLRDNORM, WSAPOLLFD};
        let mut fd = WSAPOLLFD { fd: self.inner.as_raw_socket() as _, events: POLLRDNORM, revents: 0 };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one descriptor of our open socket, valid for the call
        let ready = unsafe { WSAPoll(&mut fd, 1, ms) };
        if ready < 0 {
            // (an error that stays must not make the receive loop spin: wait as asked)
            std::thread::sleep(timeout);
        }
        ready > 0
    }

    /// (Elsewhere the socket blocks, with a read timeout: reading itself waits.)
    #[cfg(not(windows))]
    pub fn wait_readable(&self, _timeout: Duration) -> bool {
        true
    }
}

/// Bigger kernel buffers (a burst from 7 peers must not overflow the default 64 KB and
/// turn into loss) and DSCP "Expedited Forwarding", which routers and Wi-Fi (WMM voice
/// queue) that honor it send first. Both are best effort. Checked on the wire on Windows 11
/// (`examples/dscp_probe.rs` + pktmon): IPv4 from this dual-stack socket and IPv6 both
/// leave marked 46.
fn tune(s: &Socket, v6: bool) {
    const BUF: usize = 1 << 20;
    const DSCP_EF: u32 = 46 << 2;
    let _ = s.set_recv_buffer_size(BUF);
    let _ = s.set_send_buffer_size(BUF);
    let _ = s.set_tos(DSCP_EF); // IPv4 (and IPv4-mapped traffic on most systems)
    if v6 {
        #[cfg(not(windows))]
        let _ = s.set_tclass_v6(DSCP_EF);
        // socket2 has no IPV6_TCLASS on Windows, but Windows takes it
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            use windows_sys::Win32::Networking::WinSock::{setsockopt, IPPROTO_IPV6, IPV6_TCLASS};
            let v = DSCP_EF as i32;
            // SAFETY: our open socket; a 4-byte int option value that outlives the call
            unsafe { setsockopt(s.as_raw_socket() as _, IPPROTO_IPV6, IPV6_TCLASS, &v as *const i32 as *const u8, 4) };
        }
    }
}

/// `::ffff:1.2.3.4` → `1.2.3.4`.
pub fn canonical(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => a,
        },
        a => a,
    }
}

/// The IPv4 addresses of this machine's other network adapters (up, not loopback or
/// link-local), besides `main`: a second network card, or a VPN both players are on
/// (Tailscale, ZeroTier, Hamachi, a company VPN). Two people whose routers do not let them
/// through to each other can still meet there.
pub fn other_ipv4s(main: Option<IpAddr>) -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for i in netdev::get_interfaces() {
        if !i.is_up() || i.is_loopback() {
            continue;
        }
        for n in &i.ipv4 {
            let ip = n.addr();
            if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || Some(IpAddr::V4(ip)) == main || out.contains(&ip) {
                continue;
            }
            out.push(ip);
        }
    }
    out.truncate(MAX_OTHER);
    out
}

/// At most this many extra adapter addresses are announced (peers knock on each).
pub const MAX_OTHER: usize = 2;

/// Our global IPv6 address, if this machine has IPv6 internet. (UDP `connect` sends nothing;
/// it only asks the OS which source address it would use.)
pub fn global_ipv6() -> Option<Ipv6Addr> {
    let probe = UdpSocket::bind("[::]:0").ok()?;
    probe.connect("[2001:4860:4860::8888]:53").ok()?;
    let SocketAddr::V6(local) = probe.local_addr().ok()? else { return None };
    let ip = *local.ip();
    let seg0 = ip.segments()[0];
    // global unicast is 2000::/3 (excludes link-local fe80::/10, ULA fc00::/7, loopback)
    ((seg0 & 0xe000) == 0x2000).then_some(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_stack_talks_to_ipv4_with_plain_addresses() {
        let a = Sock::bind().unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        b.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let a_port = a.port();
        a.send_to(b"hi", b.local_addr().unwrap()).unwrap();
        let mut buf = [0u8; 8];
        let (n, from) = b.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hi");
        assert_eq!(from.port(), a_port);
        b.send_to(b"yo", from).unwrap();
        a.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let (_, back) = a.recv_from(&mut buf).unwrap();
        assert_eq!(back, b.local_addr().unwrap(), "seen as plain IPv4, not ::ffff:…");
    }

    #[test]
    fn canonical_unmaps() {
        let mapped: SocketAddr = "[::ffff:10.0.0.1]:5000".parse().unwrap();
        assert_eq!(canonical(mapped), "10.0.0.1:5000".parse::<SocketAddr>().unwrap());
        let v6: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        assert_eq!(canonical(v6), v6);
    }

    /// A non-blocking socket waits for a datagram in `wait_readable`: no longer than asked
    /// when none comes, and wakes at once when one does. Reading returns at once.
    #[cfg(windows)]
    #[test]
    fn waiting_for_a_datagram_ends_when_it_comes() {
        use std::time::Instant;
        let a = Sock::bind().unwrap();
        let b = Sock::bind().unwrap();
        a.set_nonblocking().unwrap();
        let to_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), a.port());
        let mut buf = [0u8; 64];
        let t = Instant::now();
        assert_eq!(a.recv_from(&mut buf).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(t.elapsed() < Duration::from_millis(5), "reading waited");
        let t = Instant::now();
        assert!(!a.wait_readable(Duration::from_millis(100)));
        assert!(t.elapsed() >= Duration::from_millis(80), "woke after {:?} with nothing there", t.elapsed());
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            b.send_to(b"hi", to_a).unwrap();
        });
        let t = Instant::now();
        assert!(a.wait_readable(Duration::from_secs(2)));
        assert!(t.elapsed() < Duration::from_millis(500), "woke {:?} after the datagram", t.elapsed());
        assert_eq!(a.recv_from(&mut buf).unwrap().0, 2);
        sender.join().unwrap();
    }
}
