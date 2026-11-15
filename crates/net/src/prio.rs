//! Thread priority for the packet receiver: audio arriving from peers should not wait
//! behind the UI or a browser. On Windows this joins MMCSS "Pro Audio", the same class
//! audio drivers use, so the scheduler runs it promptly without starving the system.

/// Raises the calling thread's priority for audio work (best effort; failures are logged).
pub fn pro_audio() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        };
        let task: Vec<u16> = "Pro Audio".encode_utf16().chain(Some(0)).collect();
        let mut index = 0u32;
        // SAFETY: `task` is a NUL-terminated UTF-16 string that outlives the call; `index` is
        // a valid out-pointer. The returned handle stays registered for the thread's life.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
        if handle.is_null() {
            // MMCSS service off: at least ask for a high priority
            // SAFETY: the pseudo-handle of the current thread is always valid.
            let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
            tracing::info!(ok = ok != 0, "MMCSS unavailable; raised thread priority instead");
        } else {
            tracing::debug!("receiver thread joined MMCSS Pro Audio");
        }
    }
}

/// Windows power throttling ("EcoQoS") for this whole process: `Some(false)` opts out (the
/// app plays live audio; Windows may otherwise slow a process whose window is in the
/// background or minimized, run it on efficiency cores, and stretch its timers),
/// `Some(true)` asks for it (for measuring what it costs), `None` leaves it to Windows.
/// Returns whether Windows took it. No-op elsewhere.
pub fn power_throttling(throttle: Option<bool>) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            PROCESS_POWER_THROTTLING_STATE,
        };
        let both = PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: if throttle.is_some() { both } else { 0 },
            StateMask: if throttle == Some(true) { both } else { 0 },
        };
        // SAFETY: the pseudo-handle of this process; a properly sized, versioned struct
        let ok = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const _,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        } != 0;
        tracing::info!(?throttle, ok, "process power throttling");
        ok
    }
    #[cfg(not(windows))]
    {
        let _ = throttle;
        false
    }
}
