//! UPnP IGD (or NAT-PMP when there is no UPnP): ask the home router to forward our UDP port, so peers can reach us directly even
//! behind NATs that defeat hole punching. Runs in the background; the session starts without
//! waiting for it and announces the extra address once the router says yes.
//!
//! The mapping is leased (renewed while the session lasts) and removed when it ends.

use igd_next::{PortMappingProtocol, SearchOptions};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LEASE_SECS: u32 = 3600;
const RENEW_EVERY: Duration = Duration::from_secs(1500);

/// How long the router is given to answer the search. Its search request says "answer within
/// 3 s" (MX: 3), and routers pick a moment in that span at random: waiting 1.5 s (as the
/// network check did) or 3 s (as a session did) missed a router that was there, now and
/// then: "the router cannot do it" on a router that can.
const SEARCH_WAIT: Duration = Duration::from_millis(3600);

/// The router found last (and when): asked first the next time, without searching.
static FOUND: Mutex<Option<(Instant, igd_next::Gateway)>> = Mutex::new(None);
const FOUND_FOR: Duration = Duration::from_secs(600);

/// One search: `to` = where the question goes (the UPnP group, or the router itself),
/// `from` = our address on the network the router is on (so that on a PC with several
/// adapters, a VPN or a virtual one, the question leaves through the right one).
fn search_once(from: Option<IpAddr>, to: SocketAddr, timeout: Duration) -> Option<igd_next::Gateway> {
    let bind = SocketAddr::new(from.unwrap_or(IpAddr::from([0, 0, 0, 0])), 0);
    let start = Instant::now();
    // the library gives up at the first answer it cannot read (another device on the
    // network that answers the search its own way): ask again while there is time
    while let Some(left) = timeout.checked_sub(start.elapsed()).filter(|d| *d > Duration::from_millis(200)) {
        match igd_next::search_gateway(SearchOptions { bind_addr: bind, broadcast_address: to, timeout: Some(left), single_search_timeout: Some(left) }) {
            Ok(gw) => return Some(gw),
            Err(e) => tracing::debug!(to = %crate::logaddr::Shown(to), from = %crate::logaddr::opt_ip(from), "UPnP search: {e}"),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Finds the router's UPnP service. Asks three ways at once and takes the first answer:
/// the router itself (most answer a direct question at once), the UPnP group from our LAN
/// address, and the group from whichever adapter Windows picks (as before). A router found
/// in the last minutes is asked directly first (it answers "what is your address?" in
/// milliseconds), so the check gives the same answer every time.
fn search(lan_ip: Option<IpAddr>) -> Option<igd_next::Gateway> {
    let remembered = FOUND.lock().ok().and_then(|f| f.clone()).filter(|(at, _)| at.elapsed() < FOUND_FOR);
    if let Some((_, gw)) = remembered {
        if gw.get_external_ip().is_ok() {
            return Some(gw);
        }
    }
    let group: SocketAddr = "239.255.255.250:1900".parse().expect("valid");
    let router = crate::natpmp::gateway().map(|ip| SocketAddr::new(ip.into(), 1900));
    let (tx, rx) = std::sync::mpsc::channel();
    let mut asked = 0;
    for (from, to) in [(lan_ip, router), (lan_ip, Some(group)), (None, Some(group))] {
        let Some(to) = to else { continue };
        // (the same question twice is no use)
        if from.is_none() && lan_ip.is_none() && to == group && asked > 0 {
            continue;
        }
        asked += 1;
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("gachi-upnp-search".into()).spawn(move || {
            let _ = tx.send(search_once(from, to, SEARCH_WAIT));
        });
    }
    drop(tx);
    let found = rx.iter().take(asked).flatten().next();
    if let (Some(gw), Ok(mut f)) = (&found, FOUND.lock()) {
        *f = Some((Instant::now(), gw.clone()));
    }
    found
}

/// Our address on the local network (the one the default route leaves through).
fn lan_ip() -> Option<IpAddr> {
    crate::local_ip_toward("1.1.1.1:53".parse().expect("valid")).filter(|ip| ip.is_ipv4())
}

/// Can the router forward a port for us, by UPnP or NAT-PMP? (Asks only: changes nothing.)
/// Both are asked at once; a UPnP router that answers ends the wait.
pub fn available() -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx2 = tx.clone();
    let _ = std::thread::Builder::new().name("gachi-upnp-check".into()).spawn(move || {
        let _ = tx.send(search(lan_ip()).is_some());
    });
    let _ = std::thread::Builder::new().name("gachi-natpmp-check".into()).spawn(move || {
        let found = crate::natpmp::gateway().and_then(|gw| crate::natpmp::external_ip(SocketAddr::new(gw.into(), crate::natpmp::PORT))).is_some();
        let _ = tx2.send(found);
    });
    rx.iter().take(2).any(|found| found)
}

/// Maps `lan_ip:port` on the router. Calls `on_mapped` with the public address once, then
/// keeps the lease alive until `stop`, and removes the mapping.
pub fn run(lan_ip: IpAddr, port: u16, stop: Arc<AtomicBool>, on_mapped: impl FnOnce(SocketAddr)) {
    let Some(gw) = search(Some(lan_ip)) else {
        tracing::info!("no UPnP router found; trying NAT-PMP");
        run_natpmp(port, stop, on_mapped);
        return;
    };
    let local = SocketAddr::new(lan_ip, port);
    let map = |ext: Option<u16>| -> Option<u16> {
        match ext {
            Some(p) => gw.add_port(PortMappingProtocol::UDP, p, local, LEASE_SECS, "gachi").ok().map(|_| p),
            None => gw.add_any_port(PortMappingProtocol::UDP, local, LEASE_SECS, "gachi").ok(),
        }
    };
    // same port outside as inside if we can, else whatever the router gives us
    let Some(ext_port) = map(Some(port)).or_else(|| map(None)) else {
        tracing::info!("UPnP router refused the port mapping");
        return;
    };
    let Ok(ext_ip) = gw.get_external_ip() else {
        let _ = gw.remove_port(PortMappingProtocol::UDP, ext_port);
        return;
    };
    let public = SocketAddr::new(ext_ip, ext_port);
    tracing::info!(public = %crate::logaddr::Shown(public), "UPnP port mapped");
    on_mapped(public);

    let mut renewed = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
        if renewed.elapsed() >= RENEW_EVERY {
            renewed = Instant::now();
            if map(Some(ext_port)).is_none() {
                tracing::warn!("could not renew the UPnP mapping");
            }
        }
    }
    let _ = gw.remove_port(PortMappingProtocol::UDP, ext_port);
    tracing::info!("UPnP port mapping removed");
}

/// The same with NAT-PMP (routers without UPnP): map, renew at half the lease, delete at the end.
fn run_natpmp(port: u16, stop: Arc<AtomicBool>, on_mapped: impl FnOnce(SocketAddr)) {
    use crate::natpmp;
    let Some(router) = natpmp::gateway().map(|gw| SocketAddr::new(gw.into(), natpmp::PORT)) else { return };
    let Some((public, lease)) = natpmp::map(router, port) else {
        tracing::info!("no NAT-PMP router either");
        return;
    };
    tracing::info!(public = %crate::logaddr::Shown(public.into()), lease, "NAT-PMP port mapped");
    on_mapped(public.into());
    let renew_every = Duration::from_secs((lease / 2).max(30) as u64);
    let mut renewed = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
        if renewed.elapsed() >= renew_every {
            renewed = Instant::now();
            if natpmp::map_udp(router, port, natpmp::LIFETIME_SECS).is_none() {
                tracing::warn!("could not renew the NAT-PMP mapping");
            }
        }
    }
    let _ = natpmp::map_udp(router, port, 0);
    tracing::info!("NAT-PMP port mapping removed");
}
