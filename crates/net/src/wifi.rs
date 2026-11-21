//! Quieter Wi-Fi while in a room: Windows' background scan for other networks off, and its
//! streaming mode on, for every connected Wi-Fi card.
//!
//! A background scan takes the card off our channel for tens of ms at a time. Measured on
//! a Wi-Fi 6E card (`examples/wifi_stall.rs`, 12 min per setting): pings later than 10 ms
//! 487 → 117, p99.9 11.8 → 5.6 ms, no more multi-second shaky stretches. Those are what make
//! the automatic jitter buffer grow. Stalls over 50 ms (other causes) stay as they were.
//!
//! Both settings belong to our WLAN handle: Windows puts them back as soon as it is closed
//! (on drop, or if the app ends in any way). No administrator rights needed. While on, the
//! list of nearby networks refreshes less often.

/// Holds the settings while alive.
pub struct QuietWifi {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    /// Wi-Fi cards it applied to
    pub cards: usize,
}

// SAFETY: a WLAN client handle may be used and closed from any thread.
unsafe impl Send for QuietWifi {}
unsafe impl Sync for QuietWifi {}

impl QuietWifi {
    /// Turns it on for each connected Wi-Fi card; `None` when there is none (or no WLAN
    /// service, or Windows refused).
    #[cfg(windows)]
    pub fn start() -> Option<Self> {
        use windows_sys::Win32::NetworkManagement::WiFi::*;
        let mut ver = 0u32;
        let mut handle = std::ptr::null_mut();
        // SAFETY: out-pointers to locals; version 2 = Vista and later
        if unsafe { WlanOpenHandle(2, std::ptr::null(), &mut ver, &mut handle) } != 0 {
            return None;
        }
        // owns the handle from here: any early return closes it
        let mut quiet = Self { handle, cards: 0 };
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        // SAFETY: the handle is open; the list is freed below
        if unsafe { WlanEnumInterfaces(handle, std::ptr::null(), &mut list) } != 0 || list.is_null()
        {
            return None; // drop closes the handle
        }
        let mut cards = 0;
        // SAFETY: the list holds dwNumberOfItems entries
        let items = unsafe {
            std::slice::from_raw_parts(
                (*list).InterfaceInfo.as_ptr(),
                (*list).dwNumberOfItems as usize,
            )
        };
        for i in items
            .iter()
            .filter(|i| i.isState == wlan_interface_state_connected)
        {
            let set = |opcode, on: bool| {
                let v = on as i32;
                // SAFETY: a BOOL-sized value for a BOOL opcode, on an interface of this handle
                unsafe {
                    WlanSetInterface(
                        handle,
                        &i.InterfaceGuid,
                        opcode,
                        4,
                        &v as *const i32 as *const _,
                        std::ptr::null(),
                    ) == 0
                }
            };
            let scan_off = set(wlan_intf_opcode_background_scan_enabled, false);
            let streaming = set(wlan_intf_opcode_media_streaming_mode, true);
            if scan_off || streaming {
                cards += 1;
            }
            tracing::debug!(scan_off, streaming, "Wi-Fi card quieted");
        }
        // SAFETY: allocated by WlanEnumInterfaces
        unsafe { WlanFreeMemory(list as *const _) };
        // hand over this same value (a copy of the handle would leave `quiet` to close it
        // here, and Windows would undo both settings at once)
        quiet.cards = cards;
        (cards > 0).then_some(quiet)
    }

    #[cfg(not(windows))]
    pub fn start() -> Option<Self> {
        None
    }
}

impl Drop for QuietWifi {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: opened in `start`, closed once; Windows restores both settings
        unsafe {
            windows_sys::Win32::NetworkManagement::WiFi::WlanCloseHandle(
                self.handle,
                std::ptr::null(),
            );
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::Win32::NetworkManagement::WiFi::*;

    /// Background scan of the first connected card as handle `h` sees it, or (`None`) as a
    /// fresh handle does. Windows answers per handle: another program's handle keeps seeing
    /// its own (default) value while ours has it off.
    fn scan_on(through: Option<&QuietWifi>) -> Option<bool> {
        let (mut ver, mut h) = (0u32, std::ptr::null_mut());
        // SAFETY: plain WLAN queries with out-pointers to locals; what the API allocates is
        // freed with WlanFreeMemory, and a handle opened here is closed here
        unsafe {
            match through {
                Some(q) => h = q.handle,
                None => {
                    if WlanOpenHandle(2, std::ptr::null(), &mut ver, &mut h) != 0 {
                        return None;
                    }
                }
            }
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            let mut out = None;
            if WlanEnumInterfaces(h, std::ptr::null(), &mut list) == 0 && !list.is_null() {
                let items = std::slice::from_raw_parts(
                    (*list).InterfaceInfo.as_ptr(),
                    (*list).dwNumberOfItems as usize,
                );
                if let Some(i) = items
                    .iter()
                    .find(|i| i.isState == wlan_interface_state_connected)
                {
                    let (mut size, mut data) = (0u32, std::ptr::null_mut());
                    let r = WlanQueryInterface(
                        h,
                        &i.InterfaceGuid,
                        wlan_intf_opcode_background_scan_enabled,
                        std::ptr::null(),
                        &mut size,
                        &mut data,
                        std::ptr::null_mut(),
                    );
                    if r == 0 && !data.is_null() {
                        out = Some(*(data as *const i32) != 0);
                        WlanFreeMemory(data);
                    }
                }
                WlanFreeMemory(list as *const _);
            }
            if through.is_none() {
                WlanCloseHandle(h, std::ptr::null());
            }
            out
        }
    }

    #[test]
    fn quiet_while_held_and_back_after() {
        let before = scan_on(None);
        let Some(q) = QuietWifi::start() else {
            assert!(before.is_none(), "a connected card, but nothing was set");
            println!("no connected Wi-Fi card here: nothing to check");
            return;
        };
        assert!(q.cards >= 1);
        assert_eq!(
            scan_on(Some(&q)),
            Some(false),
            "background scan off while held"
        );
        drop(q);
        assert_eq!(scan_on(None), before, "back as before once let go");
    }
}
