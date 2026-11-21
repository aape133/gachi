//! Which way of asking for DSCP "voice" priority actually marks our packets on Windows.
//! Windows ignores a plain IP_TOS by default; its QoS API (qWAVE) may mark them instead.
//!
//! Sends small UDP packets through the wired card to the router (closed ports: nothing
//! answers but "port unreachable"), every 5 ms per kind, each kind to its own port:
//!
//! | port  | how                                              |
//! |-------|--------------------------------------------------|
//! | 50001 | plain socket (no marking asked)                  |
//! | 50002 | IP_TOS = EF (46), what gachi does now            |
//! | 50003 | qWAVE, traffic type Voice                        |
//! | 50004 | qWAVE, traffic type AudioVideo                   |
//! | 50005 | gachi's own socket: dual-stack IPv6 + IP_TOS EF  |
//! | 50006 | IPv6 + IPV6_TCLASS EF, to ff02::1 (every 50 ms)  |
//!
//! Capture them with Windows' packet monitor (admin terminal) and read the DSCP field.
//! Windows 11 (2026-11): 50001 → 0, 50002 → 46, 50003 → 56, 50004 → 40, 50005 → 46,
//! 50006 → 46. IP_TOS is honored; qWAVE "Voice" means 56 (CS7, network control), not EF.
//!
//! cargo run -p gachi-net --release --example dscp_probe [-- seconds]

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}

#[cfg(windows)]
fn main() {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{SocketAddr, SocketAddrV4};
    use std::os::windows::io::AsRawSocket;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::NetworkManagement::QoS::*;
    use windows_sys::Win32::Networking::WinSock::{IN_ADDR, IN_ADDR_0, SOCKADDR, SOCKADDR_IN};

    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);
    let gw = *netdev::get_default_gateway()
        .expect("a default gateway")
        .ipv4
        .first()
        .expect("an IPv4 gateway");
    println!("to the router {gw}, for {secs} s");

    let mut qos = std::ptr::null_mut();
    let ver = QOS_VERSION {
        MajorVersion: 1,
        MinorVersion: 0,
    };
    // SAFETY: out-pointer to a local
    let have_qos = unsafe { QOSCreateHandle(&ver, &mut qos) } != 0;
    println!(
        "qWAVE handle: {}",
        if have_qos {
            "ok".to_string()
        } else {
            format!("failed ({})", std::io::Error::last_os_error())
        }
    );

    let mut socks = Vec::new();
    for (port, kind) in [
        (50001u16, "plain"),
        (50002, "IP_TOS EF"),
        (50003, "qWAVE Voice"),
        (50004, "qWAVE AudioVideo"),
    ] {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        s.bind(&SocketAddr::from(([0, 0, 0, 0], 0)).into()).unwrap();
        let to = SocketAddrV4::new(gw, port);
        let note = match port {
            50002 => match s.set_tos(46 << 2) {
                Ok(()) => "set".to_string(),
                Err(e) => format!("refused: {e}"),
            },
            50003 | 50004 if have_qos => {
                let kind = if port == 50003 {
                    QOSTrafficTypeVoice
                } else {
                    QOSTrafficTypeAudioVideo
                };
                let addr = SOCKADDR_IN {
                    sin_family: windows_sys::Win32::Networking::WinSock::AF_INET,
                    sin_port: port.to_be(),
                    sin_addr: IN_ADDR {
                        S_un: IN_ADDR_0 {
                            S_addr: u32::from_ne_bytes(gw.octets()),
                        },
                    },
                    sin_zero: [0; 8],
                };
                let mut flow = 0u32;
                // SAFETY: a bound socket of ours; a valid IPv4 address struct; out-pointer to a local
                let ok = unsafe {
                    QOSAddSocketToFlow(
                        qos,
                        s.as_raw_socket() as _,
                        &addr as *const SOCKADDR_IN as *const SOCKADDR,
                        kind,
                        QOS_NON_ADAPTIVE_FLOW,
                        &mut flow,
                    )
                } != 0;
                if ok {
                    format!("flow {flow}")
                } else {
                    format!("refused: {}", std::io::Error::last_os_error())
                }
            }
            _ => "-".to_string(),
        };
        println!("  port {port}  {kind:<17} {note}");
        socks.push((s, socket2::SockAddr::from(to)));
    }
    // 50005: exactly gachi's socket (crates/net/src/sock.rs): dual-stack IPv6 with IP_TOS,
    // sending to the router as an IPv4-mapped address
    {
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        s.set_only_v6(false).unwrap();
        s.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)).into())
            .unwrap();
        let tos = s.set_tos(46 << 2);
        // IPv6's own field (IPV6_TCLASS = 39 at IPPROTO_IPV6 = 41), for IPv6 peers
        let tclass: i32 = 46 << 2;
        let r = unsafe {
            windows_sys::Win32::Networking::WinSock::setsockopt(
                s.as_raw_socket() as _,
                41,
                39,
                &tclass as *const i32 as *const u8,
                4,
            )
        };
        println!(
            "  port 50005  dual-stack + IP_TOS  {} | IPV6_TCLASS {}",
            match &tos {
                Ok(()) => "set".to_string(),
                Err(e) => format!("refused: {e}"),
            },
            if r == 0 {
                "set".to_string()
            } else {
                format!("refused: {}", std::io::Error::last_os_error())
            }
        );
        socks.push((s, SocketAddr::from((gw.to_ipv6_mapped(), 50005)).into()));
    }
    // 50006: IPv6 with IPV6_TCLASS, to all nodes on this link (ff02::1: no router or global
    // address needed), sparingly: one in ten rounds
    let v6 = {
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        s.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)).into())
            .unwrap();
        let tclass: i32 = 46 << 2;
        let r = unsafe {
            windows_sys::Win32::Networking::WinSock::setsockopt(
                s.as_raw_socket() as _,
                41,
                39,
                &tclass as *const i32 as *const u8,
                4,
            )
        };
        let scope = netdev::get_default_interface()
            .map(|i| i.index)
            .unwrap_or(0);
        println!(
            "  port 50006  IPv6 + IPV6_TCLASS  {} (to ff02::1 on interface {scope})",
            if r == 0 { "set" } else { "refused" }
        );
        let to: socket2::SockAddr =
            std::net::SocketAddrV6::new("ff02::1".parse().unwrap(), 50006, 0, scope).into();
        (s, to)
    };
    let payload = [0x47u8; 200];
    let start = Instant::now();
    let mut n = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        for (s, to) in &socks {
            let _ = s.send_to(&payload, to);
        }
        if n.is_multiple_of(10) {
            let _ = v6.0.send_to(&payload, &v6.1);
        }
        n += 1;
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("sent {n} packets of each kind");
    if have_qos {
        unsafe { QOSCloseHandle(qos) };
    }
}
