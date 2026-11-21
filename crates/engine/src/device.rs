use cpal::traits::{DeviceTrait, HostTrait};

/// Device as shown in the UI. `id` is the device name (what cpal can look up).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub label: String,
    pub has_input: bool,
    pub has_output: bool,
    /// input channels (0 for output-only devices)
    pub inputs: u16,
    /// the driver's names for the input channels, e.g. "Analog In 1", "Loopback 1 L"
    /// (ASIO only; empty when unknown)
    pub input_names: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostChoice {
    /// Steinberg ASIO (Windows, `asio` feature). Falls back to the default host if unavailable.
    Asio,
    /// The platform default: WASAPI on Windows, CoreAudio on macOS, ALSA/JACK on Linux.
    Default,
    /// DAW mode: no device of our own. The DAW owns the interface; the gachi plugin hands
    /// each block over the same-PC bridge and plays back what we mix (Windows).
    Daw,
}

pub(crate) fn host(choice: HostChoice) -> cpal::Host {
    #[cfg(all(windows, feature = "asio"))]
    if choice == HostChoice::Asio {
        match cpal::host_from_id(cpal::HostId::Asio) {
            Ok(h) => return h,
            Err(e) => tracing::warn!("ASIO unavailable ({e}), using default host"),
        }
    }
    let _ = choice;
    cpal::default_host()
}

/// Whether this build can talk to ASIO drivers.
pub const ASIO_AVAILABLE: bool = cfg!(all(windows, feature = "asio"));

/// What the last listing of the ASIO drivers found (and the first driver's input channels):
/// given out again while an engine of ours has a driver open, when no driver may be loaded
/// to ask (see `host_thread::Kept::busy`; listing loads every driver in turn).
static LISTED: std::sync::Mutex<Option<(Vec<DeviceInfo>, u16)>> = std::sync::Mutex::new(None);

/// The ASIO drivers and the first one's input channels, on the ASIO thread: asked anew
/// while no engine runs, else as last found. `None`: an engine runs and nothing was found
/// before (the program just started and is reading its device): to be asked again shortly.
fn asio_listing() -> Option<(Vec<DeviceInfo>, u16)> {
    crate::host_thread::run(crate::host_thread::SLOW, |kept| {
        let mut listed = LISTED.lock().unwrap_or_else(|e| e.into_inner());
        if !kept.busy() {
            let first = host(HostChoice::Asio)
                .default_input_device()
                .map(|d| input_channels(&d))
                .unwrap_or(0);
            *listed = Some((list_on_this_thread(HostChoice::Asio), first));
        }
        listed.clone()
    })
    .flatten()
}

/// (waits out an engine that is open for a moment: the home screen's reading of the device
/// at start lasts under 2 s)
fn asio_listing_soon() -> (Vec<DeviceInfo>, u16) {
    for _ in 0..15 {
        if let Some(l) = asio_listing() {
            return l;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Default::default()
}

pub fn list_devices(choice: HostChoice) -> Vec<DeviceInfo> {
    // ASIO drivers belong to the thread that loads them: always use the ASIO thread
    if choice == HostChoice::Asio && ASIO_AVAILABLE {
        return asio_listing_soon().0;
    }
    list_on_this_thread(choice)
}

fn list_on_this_thread(choice: HostChoice) -> Vec<DeviceInfo> {
    let host = host(choice);
    let mut out: Vec<DeviceInfo> = Vec::new();
    let mut add = |name: String, input: bool, inputs: u16, input_names: Vec<String>| {
        if let Some(d) = out.iter_mut().find(|d| d.id == name) {
            d.has_input |= input;
            d.has_output |= !input;
            d.inputs = d.inputs.max(inputs);
            if d.input_names.is_empty() {
                d.input_names = input_names;
            }
        } else {
            out.push(DeviceInfo {
                label: name.clone(),
                id: name,
                has_input: input,
                has_output: !input,
                inputs,
                input_names,
            });
        }
    };
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if let Ok(n) = d.name() {
                let inputs = input_channels(&d);
                // cpal has just loaded this device's driver: ask it for the channel names
                let names = if choice == HostChoice::Asio {
                    asio_input_names(inputs)
                } else {
                    Vec::new()
                };
                add(n, true, inputs, names);
            }
        }
    }
    if let Ok(devs) = host.output_devices() {
        devs.filter_map(|d| d.name().ok())
            .for_each(|n| add(n, false, 0, Vec::new()));
    }
    out
}

/// Input channels of the host's default input device (what "system default" means).
pub fn default_input_channels(choice: HostChoice) -> u16 {
    if choice == HostChoice::Asio && ASIO_AVAILABLE {
        // (as the last listing found it: asked for right after one, and every driver is
        // loaded once then, not twice)
        let listed = LISTED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|l| l.1);
        return listed.unwrap_or_else(|| asio_listing_soon().1);
    }
    host(choice)
        .default_input_device()
        .map(|d| input_channels(&d))
        .unwrap_or(0)
}

/// Names of the loaded ASIO driver's input channels (call on the ASIO thread, right after
/// cpal touched the device, so that driver is the loaded one).
#[cfg(all(windows, feature = "asio"))]
fn asio_input_names(n: u16) -> Vec<String> {
    use asio_sys::bindings::asio_import as ai;
    let mut names = Vec::with_capacity(n as usize);
    for ch in 0..n as i32 {
        let mut info = ai::ASIOChannelInfo {
            channel: ch,
            isInput: 1,
            isActive: 0,
            channelGroup: 0,
            type_: 0,
            name: [0; 32],
        };
        // SAFETY: plain C call on the thread that owns the loaded driver; `info` is a valid,
        // initialized struct it fills in (the name is NUL-terminated within its 32 bytes).
        let ok = unsafe { ai::ASIOGetChannelInfo(&mut info) } == 0;
        if !ok {
            return Vec::new();
        }
        let bytes: Vec<u8> = info
            .name
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        names.push(driver_text(&bytes, CP_SYSTEM).trim().to_string());
    }
    names
}

#[cfg(not(all(windows, feature = "asio")))]
fn asio_input_names(_n: u16) -> Vec<String> {
    Vec::new()
}

/// The driver's control panel is open (some drivers do not return from `ASIOControlPanel`
/// until it is closed): their silence then is no hang, and the driver checks wait.
static PANEL_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn panel_open() -> bool {
    PANEL_OPEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the ASIO control panel we opened is still up, as far as we can tell (for
/// diagnostics).
pub fn asio_panel_open() -> bool {
    panel_open()
}

/// Opens the loaded ASIO driver's own control panel (buffer size, sample rate, routing: the
/// settings gachi follows but never changes itself). Needs a driver loaded, i.e. an engine
/// on it (the settings screen opens one). Returns once the panel is up; a panel the driver
/// keeps modal stays open in the background.
#[cfg(all(windows, feature = "asio"))]
pub fn open_asio_control_panel() -> Result<(), String> {
    use std::sync::atomic::Ordering::Relaxed;
    unsafe extern "C" {
        // asio.cpp (ASIO SDK, compiled into asio-sys); asio-sys binds the rest, not this one
        #[link_name = "\u{1}?ASIOControlPanel@@YAJXZ"]
        fn ASIOControlPanel() -> i32;
    }
    const ASE_NOT_PRESENT: i32 = -1000;
    if crate::host_thread::stuck() {
        return Err(gachi_lang::tr!("audio.asio_driver_not"));
    }
    PANEL_OPEN.store(true, Relaxed);
    let r = crate::host_thread::run(std::time::Duration::from_secs(2), |_| {
        // SAFETY: a plain call into the driver loaded on this (the ASIO) thread
        let e = unsafe { ASIOControlPanel() };
        PANEL_OPEN.store(false, Relaxed);
        e
    });
    match r {
        Some(0) | Some(0x3f4847a0) => Ok(()),
        Some(ASE_NOT_PRESENT) => Err(gachi_lang::tr!("audio.asio_device_not")),
        Some(e) => Err(gachi_lang::tr!("audio.driver_couldnt_open", e)),
        // still open: this driver keeps its panel modal until it is closed
        None => Ok(()),
    }
}

#[cfg(not(all(windows, feature = "asio")))]
pub fn open_asio_control_panel() -> Result<(), String> {
    Err(gachi_lang::tr!("audio.build_does_not"))
}

/// The loaded ASIO driver's own (input, output) latency in frames: what it adds beyond our
/// callbacks (its buffer, safety offset, USB transfer, often its converters too), as it
/// reports it. `None` when it does not tell. Call on the ASIO thread with streams running.
#[cfg(all(windows, feature = "asio"))]
pub(crate) fn asio_latencies() -> Option<(u32, u32)> {
    unsafe extern "C" {
        // asio.cpp (ASIO SDK, compiled into asio-sys); asio-sys does not bind it
        #[link_name = "\u{1}?ASIOGetLatencies@@YAJPEAJ0@Z"]
        fn ASIOGetLatencies(input: *mut i32, output: *mut i32) -> i32;
    }
    let (mut i, mut o) = (0i32, 0i32);
    // SAFETY: a plain query to the loaded driver on its thread; both pointers are locals
    let ok = unsafe { ASIOGetLatencies(&mut i, &mut o) } == 0;
    (ok && i > 0 && o > 0).then_some((i as u32, o as u32))
}

#[cfg(not(all(windows, feature = "asio")))]
pub(crate) fn asio_latencies() -> Option<(u32, u32)> {
    None
}

/// What the loaded ASIO driver runs now.
pub(crate) struct DriverNow {
    pub rate: u32,
    /// preferred buffer size (frames)
    pub pref: u32,
    /// (input, output) latency it reports now (frames), if it tells
    pub latency: Option<(u32, u32)>,
}

/// The loaded ASIO driver's current settings ([`DriverNow`]).
#[cfg(all(windows, feature = "asio"))]
/// `Err` when the driver does not answer within a second (it hangs).
pub(crate) fn asio_driver_now() -> Result<Option<DriverNow>, ()> {
    use asio_sys::bindings::asio_import as ai;
    crate::host_thread::run(std::time::Duration::from_secs(1), |_| {
        let mut rate = 0f64;
        let (mut min, mut max, mut pref, mut gran) = (0, 0, 0, 0);
        // SAFETY: plain queries to the loaded driver, on the thread that owns it; all
        // pointers are to locals that outlive the calls.
        let ok = unsafe {
            ai::get_sample_rate(&mut rate) == 0
                && ai::ASIOGetBufferSize(&mut min, &mut max, &mut pref, &mut gran) == 0
        };
        // the latency can change without the buffer (a "safe mode" in the panel), and some
        // drivers report more right after starting than once settled: read it every time
        ok.then(|| DriverNow {
            rate: rate.round() as u32,
            pref: pref.max(0) as u32,
            latency: asio_latencies(),
        })
    })
    .ok_or(())
}

#[cfg(not(all(windows, feature = "asio")))]
pub(crate) fn asio_driver_now() -> Result<Option<DriverNow>, ()> {
    Ok(None)
}

fn input_channels(d: &cpal::Device) -> u16 {
    let max = d
        .supported_input_configs()
        .ok()
        .and_then(|it| it.map(|c| c.channels()).max())
        .unwrap_or(0);
    max.max(d.default_input_config().map(|c| c.channels()).unwrap_or(0))
}

pub(crate) fn find_device(
    host: &cpal::Host,
    name: Option<&str>,
    input: bool,
) -> Option<cpal::Device> {
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        let found = if input {
            host.input_devices()
        } else {
            host.output_devices()
        }
        .ok()
        .and_then(|mut it| it.find(|d| d.name().map(|n| n == name).unwrap_or(false)));
        if found.is_some() {
            return found;
        }
        tracing::warn!(
            "device {name:?} not found for {}, using default",
            if input { "input" } else { "output" }
        );
    }
    if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }
}

/// The system's own code page (`CP_ACP`): what a driver writes its names in.
#[allow(dead_code)]
const CP_SYSTEM: u32 = 0;

/// A name as an ASIO driver gives it: bytes in Windows' code page for programs that do not
/// speak Unicode (949 on a Korean system, 932 on a Japanese one), not UTF-8. Read as UTF-8,
/// a Korean or Japanese channel name came out as question marks. Plain ASCII (nearly every
/// driver) and names that are UTF-8 after all stay as they are.
#[cfg(windows)]
#[allow(dead_code)]
fn driver_text(bytes: &[u8], code_page: u32) -> String {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let mut wide = vec![0u16; bytes.len()];
    // SAFETY: both buffers are ours, with the lengths given; a character never takes more
    // UTF-16 units than it took bytes
    let n = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            wide.len() as i32,
        )
    };
    if n <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    String::from_utf16_lossy(&wide[..n as usize])
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn driver_text(bytes: &[u8], _code_page: u32) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(all(test, windows))]
mod name_tests {
    use super::driver_text;

    #[test]
    fn a_drivers_names_are_read_in_its_code_page() {
        assert_eq!(driver_text(b"Analogue 1", 0), "Analogue 1");
        // "마이크 1" in code page 949 (Korean), "マイク 1" in 932 (Japanese)
        assert_eq!(
            driver_text(&[0xB8, 0xB6, 0xC0, 0xCC, 0xC5, 0xA9, b' ', b'1'], 949),
            "마이크 1"
        );
        assert_eq!(
            driver_text(&[0x83, 0x7D, 0x83, 0x43, 0x83, 0x4E, b' ', b'1'], 932),
            "マイク 1"
        );
        // a driver that writes UTF-8 after all
        assert_eq!(driver_text("마이크".as_bytes(), 949), "마이크");
    }
}
