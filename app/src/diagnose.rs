//! Why a direct connection to someone failed, in words a player can act on.
//!
//! There is no relay: when the two networks cannot reach each other directly, the people
//! themselves are the fix (another network, a setting on the router or the PC). Each side
//! knows its own kind of network (`NatKind`, found out from the STUN servers when joining)
//! and tells the room; the addresses each side gives say where it is. From both, the
//! likeliest reason and what helps.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use gachi_net::stun::Mapping;
use gachi_proto::signal::NatKind;

/// Our kind of network, from what the STUN servers answered.
pub fn nat_kind(mapping: Mapping, public: Option<SocketAddr>) -> NatKind {
    match (public, mapping) {
        (None, _) => NatKind::Blocked,
        (Some(_), Mapping::Steady) => NatKind::Steady,
        (Some(_), Mapping::Varies { .. }) => NatKind::Varies,
        (Some(_), Mapping::Unknown) => NatKind::Unknown,
    }
}

/// One side of a connection that failed.
#[derive(Clone, Debug, Default)]
pub struct Side {
    /// its kind of network (`None`: an older version that does not say)
    pub nat: Option<NatKind>,
    /// its public IPv4 address, if it has one among its addresses
    pub public: Option<Ipv4Addr>,
    /// its addresses on its own network (behind the router)
    pub lan: Vec<Ipv4Addr>,
}

impl Side {
    pub fn new(nat: Option<NatKind>, candidates: &[SocketAddr]) -> Self {
        let mut side = Side {
            nat,
            ..Default::default()
        };
        for a in candidates {
            if let IpAddr::V4(ip) = a.ip() {
                if gachi_net::behind_router(a.ip()) {
                    side.lan.push(ip);
                } else if side.public.is_none() {
                    side.public = Some(ip);
                }
            }
        }
        side
    }

    /// Both behind one router: the same public address and a home-network address (not
    /// a carrier's shared 100.64/10 one, which neighbours behind one carrier NAT have too)
    /// in the same /24.
    fn same_router(&self, other: &Side) -> bool {
        let home = |a: &&Ipv4Addr| a.is_private();
        self.public.is_some()
            && self.public == other.public
            && self.lan.iter().filter(home).any(|a| {
                other
                    .lan
                    .iter()
                    .filter(home)
                    .any(|b| a.octets()[..3] == b.octets()[..3])
            })
    }
}

/// What to tell when no direct connection to `name` came about: why, and what helps.
pub fn no_direct_path(me: &Side, them: &Side, name: &str) -> String {
    let head = gachi_lang::tr!("direct.cant_connect_directly", name);
    let why = if me.nat == Some(NatKind::Blocked) {
        gachi_lang::tr!("direct.network_seems_block")
    } else if them.nat == Some(NatKind::Blocked) {
        gachi_lang::tr!("direct.name_s_network", name)
    } else if me.same_router(them) {
        gachi_lang::tr!("direct.youre_same_router")
    } else if me.public.is_some() && me.public == them.public {
        gachi_lang::tr!("direct.both_share_same")
    } else {
        match (me.nat, them.nat) {
            (Some(NatKind::Varies), Some(NatKind::Varies)) => {
                gachi_lang::tr!("direct.youre_both_internet")
            }
            (Some(NatKind::Varies), _) => gachi_lang::tr!("direct.internet_makes_direct"),
            (_, Some(NatKind::Varies)) => gachi_lang::tr!("direct.name_s_internet", name),
            // both ordinary: what is left is a firewall on the way (the PC's, or a strict router)
            (Some(NatKind::Steady), Some(NatKind::Steady)) => {
                gachi_lang::tr!("direct.both_connections_normally")
            }
            // (the other side does not say: an older version)
            _ => gachi_lang::tr!("direct.name_s_internet_may", name),
        }
    };
    format!("{head} {why}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(nat: Option<NatKind>, addrs: &[&str]) -> Side {
        let addrs: Vec<SocketAddr> = addrs.iter().map(|a| a.parse().unwrap()).collect();
        Side::new(nat, &addrs)
    }

    #[test]
    fn our_kind_of_network_from_the_stun_answers() {
        let a: SocketAddr = "203.0.113.7:4000".parse().unwrap();
        assert_eq!(nat_kind(Mapping::Steady, Some(a)), NatKind::Steady);
        assert_eq!(
            nat_kind(Mapping::Varies { step: 1 }, Some(a)),
            NatKind::Varies
        );
        assert_eq!(nat_kind(Mapping::Unknown, Some(a)), NatKind::Unknown);
        assert_eq!(nat_kind(Mapping::Unknown, None), NatKind::Blocked);
    }

    #[test]
    fn the_reason_fits_what_each_side_knows() {
        use NatKind::*;
        let home_a = side(Some(Steady), &["192.168.0.7:4000", "203.0.113.7:4000"]);
        let home_b = side(Some(Steady), &["192.168.1.9:4000", "198.51.100.3:4000"]);
        let hotspot = side(Some(Varies), &["172.20.10.2:4000", "198.51.100.200:4000"]);
        let blocked = side(Some(Blocked), &["10.1.2.3:4000"]);
        let old = side(None, &["192.168.5.5:4000", "198.51.100.77:4000"]);
        let say =
            |me: &Side, them: &Side| gachi_lang::with("ko-KR", || no_direct_path(me, them, "지민"));

        assert!(say(&blocked, &home_b).contains("내 네트워크가 직접 연결(UDP)을 막고"));
        assert!(say(&home_a, &blocked).contains("지민님 네트워크가 직접 연결(UDP)을 막고"));
        assert!(say(&hotspot, &home_b).contains("내 인터넷이 직접 연결이 어려운"));
        assert!(say(&home_a, &hotspot).contains("지민님 인터넷이 직접 연결이 어려운"));
        let hotspot_b = side(Some(Varies), &["192.168.43.5:4000", "198.51.100.201:4000"]);
        assert!(say(&hotspot, &hotspot_b).contains("두 분 모두"));
        assert!(say(&home_a, &home_b).contains("방화벽"));
        assert!(say(&home_a, &old).contains("지민님 인터넷이"));
        // behind the same router (same public address, same LAN): a firewall on a PC
        let same_lan = side(Some(Steady), &["192.168.0.8:4000", "203.0.113.7:4000"]);
        assert!(say(&home_a, &same_lan).contains("같은 공유기"));
        // the same public address but different networks: one carrier NAT or building
        let neighbour = side(Some(Steady), &["10.20.30.40:4000", "203.0.113.7:4000"]);
        assert!(say(&home_a, &neighbour).contains("같은 공인 주소"));
        // two phones behind one carrier NAT: shared 100.64/10 addresses, not one router
        let cgn_a = side(Some(Steady), &["100.70.1.5:4000", "198.51.100.9:4000"]);
        let cgn_b = side(Some(Steady), &["100.70.1.9:4000", "198.51.100.9:4000"]);
        assert!(say(&cgn_a, &cgn_b).contains("같은 공인 주소"));
        // every message names the person and says what to do
        for (me, them) in [
            (&blocked, &home_b),
            (&home_a, &hotspot),
            (&home_a, &home_b),
            (&home_a, &old),
        ] {
            let m = say(me, them);
            assert!(m.starts_with("지민님과 직접 연결이 안 돼요."), "{m}");
        }
    }

    #[test]
    fn every_reason_is_said_in_english_and_japanese_too() {
        use gachi_lang::with;
        use NatKind::*;
        let sides = [
            side(Some(Steady), &["192.168.0.7:4000", "203.0.113.7:4000"]),
            side(Some(Steady), &["192.168.0.9:4000", "203.0.113.7:4000"]),
            side(Some(Steady), &["10.20.30.40:4000", "203.0.113.7:4000"]),
            side(Some(Varies), &["172.20.10.2:4000", "198.51.100.200:4000"]),
            side(Some(Blocked), &["10.1.2.3:4000"]),
            side(None, &["192.168.5.5:4000", "198.51.100.77:4000"]),
        ];
        let hangul = |s: &str| s.chars().any(|c| ('\u{ac00}'..='\u{d7a3}').contains(&c));
        for me in &sides {
            for them in &sides {
                let en = with("en-US", || no_direct_path(me, them, "Sam"));
                let ja = with("ja-JP", || no_direct_path(me, them, "Sam"));
                assert!(
                    en.starts_with("Can't connect directly to Sam.") && !hangul(&en),
                    "{en}"
                );
                assert!(
                    ja.starts_with("Samさんと直接接続できません。") && !hangul(&ja),
                    "{ja}"
                );
            }
        }
    }
}
