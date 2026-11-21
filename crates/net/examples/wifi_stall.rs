//! Wi-Fi stalls, with Windows' background scanning on and off.
//!
//! Pings the router over the Wi-Fi card only (its own address as the source, so a wired
//! connection beside it does not carry them), every 2 ms, and records each round trip.
//! Alternates two settings in rounds:
//! - "default": as Windows runs Wi-Fi (it scans for other networks now and then, leaving
//!   the channel for tens of ms each time),
//! - "no scan": `gachi_net::wifi::QuietWifi`, as the app holds it in a room over Wi-Fi (no
//!   background scan, streaming mode). Windows undoes both when it is dropped, which the
//!   program checks.
//!
//! It also lists the Wi-Fi events Windows reports around each stall (scans, roaming), when
//! it may: since Windows 11 24H2 that needs the location permission (error 5 otherwise).
//! The settings themselves work without it.
//!
//! cargo run -p gachi-net --release --example wifi_stall [-- seconds per round] [rounds]
//! cargo run -p gachi-net --release --example wifi_stall -- 120 --wired [ip]   (wired card, no settings)

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(windows)]
#[allow(non_upper_case_globals)] // the WLAN constants keep their Windows names
mod imp {
    use std::net::Ipv4Addr;
    use std::time::{Duration, Instant};
    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho2Ex, ICMP_ECHO_REPLY,
    };
    use windows_sys::Win32::NetworkManagement::WiFi::*;

    /* ── one clock for pings and Wi-Fi events ── */

    use std::sync::{Mutex, OnceLock};

    fn origin() -> Instant {
        static O: OnceLock<Instant> = OnceLock::new();
        *O.get_or_init(Instant::now)
    }

    /// Wi-Fi events as Windows reports them: (seconds, source, code).
    static EVENTS: Mutex<Vec<(f64, u32, u32)>> = Mutex::new(Vec::new());

    unsafe extern "system" fn on_event(data: *mut L2_NOTIFICATION_DATA, _: *mut core::ffi::c_void) {
        if let Some(d) = data.as_ref() {
            let t = origin().elapsed().as_secs_f64();
            if let Ok(mut e) = EVENTS.lock() {
                e.push((t, d.NotificationSource, d.NotificationCode));
            }
        }
    }

    fn event_name(src: u32, code: u32) -> String {
        let c = code as i32;
        match src {
            WLAN_NOTIFICATION_SOURCE_ACM => match c {
                wlan_notification_acm_scan_complete => "scan done".into(),
                wlan_notification_acm_scan_fail => "scan failed".into(),
                wlan_notification_acm_scan_list_refresh => "scan list".into(),
                wlan_notification_acm_background_scan_enabled => "bg scan on".into(),
                wlan_notification_acm_background_scan_disabled => "bg scan off".into(),
                wlan_notification_acm_network_available => "network available".into(),
                wlan_notification_acm_power_setting_change => "power setting".into(),
                _ => format!("acm {c}"),
            },
            WLAN_NOTIFICATION_SOURCE_MSM => match c {
                wlan_notification_msm_signal_quality_change => "signal".into(),
                wlan_notification_msm_roaming_start => "roam start".into(),
                wlan_notification_msm_roaming_end => "roam end".into(),
                wlan_notification_msm_link_degraded => "link degraded".into(),
                wlan_notification_msm_link_improved => "link improved".into(),
                _ => format!("msm {c}"),
            },
            _ => format!("src {src:#x}/{c}"),
        }
    }

    /// Events from `from` to `to` seconds, signal changes left out (they come all the time).
    fn events_between(from: f64, to: f64) -> Vec<String> {
        let e = EVENTS.lock().unwrap();
        e.iter()
            .filter(|(t, s, c)| {
                *t >= from
                    && *t <= to
                    && !(*s == WLAN_NOTIFICATION_SOURCE_MSM
                        && *c as i32 == wlan_notification_msm_signal_quality_change)
            })
            .map(|(t, s, c)| format!("{} @{t:.1}", event_name(*s, *c)))
            .collect()
    }

    /* ── the WLAN settings ── */

    struct Wlan {
        handle: HANDLE,
        guid: GUID,
        name: String,
    }

    impl Wlan {
        /// The first Wi-Fi interface.
        fn open() -> Option<Self> {
            let mut ver = 0u32;
            let mut handle: HANDLE = std::ptr::null_mut();
            // SAFETY: out-pointers to locals; version 2 = Vista and later
            if unsafe { WlanOpenHandle(2, std::ptr::null(), &mut ver, &mut handle) } != 0 {
                return None;
            }
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            // SAFETY: the handle is open; the list is freed below
            if unsafe { WlanEnumInterfaces(handle, std::ptr::null(), &mut list) } != 0
                || list.is_null()
            {
                unsafe { WlanCloseHandle(handle, std::ptr::null()) };
                return None;
            }
            // SAFETY: the API returned a list with dwNumberOfItems entries
            let first = unsafe {
                let l = &*list;
                (l.dwNumberOfItems > 0).then(|| {
                    let i = &*l.InterfaceInfo.as_ptr();
                    let n = i
                        .strInterfaceDescription
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(256);
                    (
                        i.InterfaceGuid,
                        String::from_utf16_lossy(&i.strInterfaceDescription[..n]),
                    )
                })
            };
            unsafe { WlanFreeMemory(list as *const _) };
            match first {
                Some((guid, name)) => Some(Self { handle, guid, name }),
                None => {
                    unsafe { WlanCloseHandle(handle, std::ptr::null()) };
                    None
                }
            }
        }

        fn get(&self, opcode: WLAN_INTF_OPCODE) -> Option<bool> {
            let (mut size, mut data) = (0u32, std::ptr::null_mut());
            // SAFETY: out-pointers to locals; the data is freed below
            let r = unsafe {
                WlanQueryInterface(
                    self.handle,
                    &self.guid,
                    opcode,
                    std::ptr::null(),
                    &mut size,
                    &mut data,
                    std::ptr::null_mut(),
                )
            };
            if r != 0 || data.is_null() || size < 4 {
                return None;
            }
            let v = unsafe { *(data as *const i32) } != 0;
            unsafe { WlanFreeMemory(data) };
            Some(v)
        }
    }

    impl Drop for Wlan {
        fn drop(&mut self) {
            // SAFETY: opened in `open`, closed once
            unsafe { WlanCloseHandle(self.handle, std::ptr::null()) };
        }
    }

    fn state(label: &str) {
        let Some(w) = Wlan::open() else { return };
        println!(
            "  {label}: background scan {:?}, streaming mode {:?}",
            w.get(wlan_intf_opcode_background_scan_enabled),
            w.get(wlan_intf_opcode_media_streaming_mode)
        );
    }

    /* ── pings ── */

    struct Icmp(HANDLE);
    impl Drop for Icmp {
        fn drop(&mut self) {
            unsafe { IcmpCloseHandle(self.0) };
        }
    }

    fn ip(a: Ipv4Addr) -> u32 {
        u32::from_ne_bytes(a.octets())
    }

    /// One round of pings: round trips in µs (`None` = no answer within 250 ms).
    fn round(icmp: &Icmp, from: Ipv4Addr, to: Ipv4Addr, secs: f64) -> Vec<(f64, Option<u32>)> {
        let payload = [0x47u8; 200]; // about one audio packet
        let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + 256 + 8];
        let start = Instant::now();
        let mut out = Vec::new();
        let mut next = start;
        while start.elapsed().as_secs_f64() < secs {
            let t = Instant::now();
            // SAFETY: synchronous call (no event, no APC); buffers live across it
            let n = unsafe {
                IcmpSendEcho2Ex(
                    icmp.0,
                    std::ptr::null_mut(),
                    None,
                    std::ptr::null(),
                    ip(from),
                    ip(to),
                    payload.as_ptr() as *const _,
                    payload.len() as u16,
                    std::ptr::null(),
                    reply.as_mut_ptr() as *mut _,
                    reply.len() as u32,
                    250,
                )
            };
            let rtt = t.elapsed();
            let ok = n > 0 && unsafe { (*(reply.as_ptr() as *const ICMP_ECHO_REPLY)).Status } == 0;
            out.push((
                (t - origin()).as_secs_f64(),
                ok.then_some(rtt.as_micros() as u32),
            ));
            next += Duration::from_millis(2);
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        out
    }

    fn report(label: &str, pings: &[(f64, Option<u32>)]) {
        let mut ok: Vec<u32> = pings.iter().filter_map(|p| p.1).collect();
        let lost = pings.len() - ok.len();
        ok.sort_unstable();
        let q = |f: f64| {
            ok.get(((ok.len() as f64 * f) as usize).min(ok.len().saturating_sub(1)))
                .map_or(0.0, |&v| v as f64 / 1000.0)
        };
        let over = |ms: u32| {
            pings
                .iter()
                .filter(|p| p.1.is_none_or(|v| v > ms * 1000))
                .count()
        };
        // stall moments: runs of pings over 10 ms (or lost), counted once per run
        let mut moments = Vec::new();
        let mut run_start: Option<(f64, u32)> = None;
        for &(t, r) in pings {
            let bad = r.is_none_or(|v| v > 10_000);
            match (bad, run_start) {
                (true, None) => run_start = Some((t, r.unwrap_or(250_000))),
                (true, Some((s, m))) => run_start = Some((s, m.max(r.unwrap_or(250_000)))),
                (false, Some((s, m))) => {
                    moments.push((s, t - s, m));
                    run_start = None;
                }
                (false, None) => {}
            }
        }
        let secs = pings.len() as f64 * 0.002;
        println!(
            "  {label:<8} {:>6} pings in {secs:>4.0} s | ms p50 {:>5.2} p99 {:>6.2} p99.9 {:>6.2} max {:>6.1} | >5 ms {:>4}  >10 {:>4}  >20 {:>4}  >50 {:>3}  lost {lost}",
            pings.len(),
            q(0.5),
            q(0.99),
            q(0.999),
            q(1.0),
            over(5),
            over(10),
            over(20),
            over(50),
        );
        // clusters: stall moments less than 2 s apart (one scan stalls several times)
        let mut clusters: Vec<(f64, f64, usize, u32)> = Vec::new();
        for &(s, d, m) in &moments {
            match clusters.last_mut() {
                Some(c) if s - c.1 < 2.0 => {
                    c.1 = s + d;
                    c.2 += 1;
                    c.3 = c.3.max(m);
                }
                _ => clusters.push((s, s + d, 1, m)),
            }
        }
        let scans = {
            let (a, b) = (
                pings.first().map_or(0.0, |p| p.0),
                pings.last().map_or(0.0, |p| p.0),
            );
            let e = EVENTS.lock().unwrap();
            e.iter()
                .filter(|(t, s, c)| {
                    *t >= a
                        && *t <= b
                        && *s == WLAN_NOTIFICATION_SOURCE_ACM
                        && *c as i32 == wlan_notification_acm_scan_complete
                })
                .count()
        };
        println!(
            "           stall clusters: {} | Windows reported {scans} finished scans in this time",
            clusters.len()
        );
        for (a, b, n, m) in &clusters {
            let near = events_between(a - 1.0, b + 5.0);
            println!(
                "             @{a:>6.1} s  {n:>2} stalls over {:>4.0} ms, worst {:>3.0} ms  | events: {}",
                (b - a) * 1000.0,
                *m as f64 / 1000.0,
                if near.is_empty() { "none".to_string() } else { near.join(", ") }
            );
        }
    }

    pub fn main() {
        let args: Vec<String> = std::env::args()
            .skip(1)
            .filter(|a| !a.starts_with('-') && a.parse::<Ipv4Addr>().is_err())
            .collect();
        let secs: f64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(90.0);
        let rounds: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2);

        // --wired [ip]: the same pings over the wired card (to the router, or to `ip`, e.g.
        // another PC on the LAN), no Wi-Fi settings: where do stalls come from?
        let all: Vec<String> = std::env::args().collect();
        if let Some(i) = all.iter().position(|a| a == "--wired") {
            origin();
            let lan = netdev::get_default_interface().expect("a network interface");
            let from = lan
                .ipv4
                .iter()
                .map(|a| a.addr())
                .find(|a| !a.is_link_local())
                .expect("an IPv4 address");
            let to = all
                .get(i + 1)
                .and_then(|s| s.parse().ok())
                .or_else(|| netdev::get_default_gateway().ok()?.ipv4.first().copied())
                .expect("a target");
            println!(
                "wired: {} ({from}) → {to}, {secs:.0} s",
                lan.description.clone().unwrap_or_default()
            );
            let h = unsafe { IcmpCreateFile() };
            assert!(h != INVALID_HANDLE_VALUE && !h.is_null(), "ICMP handle");
            let icmp = Icmp(h);
            let p = round(&icmp, from, to, secs);
            report("wired", &p);
            return;
        }
        let wifi = netdev::get_interfaces()
            .into_iter()
            .find(|i| {
                i.if_type == netdev::prelude::InterfaceType::Wireless80211
                    && i.ipv4.iter().any(|a| !a.addr().is_link_local())
            })
            .expect("a connected Wi-Fi interface with an IPv4 address");
        // the address the router gave (not a 169.254 one Windows adds on its own)
        let from = wifi
            .ipv4
            .iter()
            .map(|a| a.addr())
            .find(|a| !a.is_link_local())
            .expect("an IPv4 address");
        // the Wi-Fi's own gateway, else the default one (one router for wired and Wi-Fi)
        let to = wifi
            .gateway
            .as_ref()
            .and_then(|g| g.ipv4.first().copied())
            .or_else(|| netdev::get_default_gateway().ok()?.ipv4.first().copied())
            .expect("a gateway");
        origin();
        let w = Wlan::open().expect("the WLAN service");
        println!(
            "Wi-Fi: {} ({from}) → router {to}; {rounds} rounds of {secs:.0} s per setting",
            w.name
        );
        // a second handle that only listens (it changes nothing)
        let listen = Wlan::open().expect("the WLAN service");
        let mut prev = 0u32;
        // SAFETY: a plain extern fn and no context; the handle stays open until the end
        let r = unsafe {
            WlanRegisterNotification(
                listen.handle,
                WLAN_NOTIFICATION_SOURCE_ACM | WLAN_NOTIFICATION_SOURCE_MSM,
                0,
                Some(on_event),
                std::ptr::null(),
                std::ptr::null(),
                &mut prev,
            )
        };
        println!(
            "listening for Wi-Fi events: {}",
            if r == 0 {
                "yes".to_string()
            } else {
                format!("no (error {r})")
            }
        );
        drop(w);
        state("before");

        // SAFETY: no arguments
        let h = unsafe { IcmpCreateFile() };
        assert!(h != INVALID_HANDLE_VALUE && !h.is_null(), "ICMP handle");
        let icmp = Icmp(h);
        let (mut all_default, mut all_off) = (Vec::new(), Vec::new());
        for r in 0..rounds {
            println!("round {}", r + 1);
            let p = round(&icmp, from, to, secs);
            report("default", &p);
            all_default.extend(p);

            // exactly what the app does in a room over Wi-Fi
            let q = gachi_net::wifi::QuietWifi::start().expect("a connected Wi-Fi card");
            println!("  (QuietWifi on for {} card(s))", q.cards);
            let p = round(&icmp, from, to, secs);
            report("no scan", &p);
            all_off.extend(p);
            drop(q);
        }
        println!("all rounds");
        report("default", &all_default);
        report("no scan", &all_off);
        state("after (handle closed)");
        drop(listen);
    }
}
