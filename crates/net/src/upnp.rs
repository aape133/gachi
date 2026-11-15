//! UPnP IGD (or NAT-PMP when there is no UPnP): ask the home router to forward our UDP port, so peers can reach us directly even
//! behind NATs that defeat hole punching. Runs in the background; the session starts without
//! waiting for it and announces the extra address once the router says yes.
//!
//! The mapping is leased (renewed while the session lasts) and removed when it ends.

use igd_next::{PortMappingProtocol, SearchOptions};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LEASE_SECS: u32 = 3600;
const RENEW_EVERY: Duration = Duration::from_secs(1500);

fn search(timeout: Duration) -> Option<igd_next::Gateway> {
    igd_next::search_gateway(SearchOptions {
        timeout: Some(timeout),
        single_search_timeout: Some(timeout),
        ..Default::default()
    })
    .ok()
}

/// Can the router forward a port for us, by UPnP or NAT-PMP? (Asks only: changes nothing.)
pub fn available() -> bool {
    search(Duration::from_millis(1500)).is_some()
        || crate::natpmp::gateway()
            .and_then(|gw| crate::natpmp::external_ip(SocketAddr::new(gw.into(), crate::natpmp::PORT)))
            .is_some()
}

/// Maps `lan_ip:port` on the router. Calls `on_mapped` with the public address once, then
/// keeps the lease alive until `stop`, and removes the mapping.
pub fn run(lan_ip: IpAddr, port: u16, stop: Arc<AtomicBool>, on_mapped: impl FnOnce(SocketAddr)) {
    let Some(gw) = search(Duration::from_secs(3)) else {
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
