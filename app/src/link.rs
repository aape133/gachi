//! Wired vs Wi-Fi: find the network interface that owns the local IP we use to reach the
//! server. Wi-Fi adds jitter, so the UI warns about it.

use gachi_proto::signal::Link;
use netdev::prelude::InterfaceType as T;
use std::net::{IpAddr, SocketAddr};

pub fn detect(server: SocketAddr) -> Link {
    let Some(ip) = gachi_net::local_ip_toward(server) else {
        return Link::Unknown;
    };
    if ip.is_loopback() {
        return Link::Unknown;
    }
    let owns = |i: &netdev::Interface| {
        i.ipv4.iter().any(|n| IpAddr::V4(n.addr()) == ip)
            || i.ipv6.iter().any(|n| IpAddr::V6(n.addr()) == ip)
    };
    match netdev::get_interfaces()
        .iter()
        .find(|i| owns(i))
        .map(|i| i.if_type)
    {
        Some(T::Wireless80211 | T::PeerToPeerWireless) => Link::Wifi,
        // PPPoE (the PC dials the ISP itself, plugged into the modem) runs over a cable
        Some(
            T::Ethernet
            | T::FastEthernetT
            | T::FastEthernetFx
            | T::GigabitEthernet
            | T::Ethernet3Megabit
            | T::Ppp,
        ) => Link::Wired,
        _ => Link::Unknown,
    }
}

pub fn label(link: Link) -> &'static str {
    match link {
        Link::Wired => "wired",
        Link::Wifi => "wifi",
        Link::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn detects_without_panicking() {
        // UDP connect sends nothing; this only asks the OS which interface it would use
        let link = super::detect("8.8.8.8:53".parse().unwrap());
        println!("link = {}", super::label(link));
    }
}
