//! Tauri commands called from ui/backend.js (TauriBackend).

use crate::session::{self, Intent, Session};
use crate::settings::Settings;
use crate::view::{CreateOpts, PeerPatch, RoomView, SelfPatch};
use crate::AppState;
use gachi_engine::{list_devices as engine_devices, HostChoice, FADER_MAX_DB, FADER_MIN_DB};
use gachi_proto::code;

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager, State};

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

/// Saves settings. In a room, audio changes reopen the device right away (the P2P
/// connections stay up); jitter changes apply live; server/name apply to the next room.
#[tauri::command]
pub async fn update_settings(app: AppHandle, patch: Value) -> Result<Settings, String> {
    let state = app.state::<AppState>();
    let (before, s) = {
        let mut g = state.settings.lock().unwrap();
        let before = g.clone();
        g.apply_patch(&patch)?;
        (before, g.clone())
    };
    state.save_settings();
    let _ = app.emit("settings", &s);

    // (both as JSON once: every key asked about below made them anew, a dozen times a change)
    let (was, now) = (
        serde_json::to_value(&before).ok(),
        serde_json::to_value(&s).ok(),
    );
    let changed =
        |k: &str| was.as_ref().and_then(|v| v.get(k)) != now.as_ref().and_then(|v| v.get(k));
    let mut restart = session::ENGINE_KEYS.iter().any(|k| changed(k));
    // only the input channels changed: switch them live (no device reopen, no gap)
    let input_only = restart
        && session::ENGINE_KEYS
            .iter()
            .all(|k| *k == "inputChannel" || !changed(k));
    if input_only {
        let input = crate::settings::parse_input(&s.input_channel);
        if let Some(sess) = state.session.lock().await.as_ref() {
            sess.set_input_live(input);
        }
        if let Some(p) = state.preview.lock().await.as_ref() {
            p.engine.set_input(input);
        }
        restart = false;
    }
    let jitter = changed("jitterMode") || changed("jitterPackets") || changed("jitterStyle");
    if changed("verboseLog") {
        crate::settings::VERBOSE_LOG.store(s.verbose_log, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(full = s.verbose_log, "log detail changed");
    }
    let profile = changed("name") || changed("instrument") || changed("color");
    let metro = changed("metronomeVolume");
    if changed("showNearby") {
        if let Some(sess) = state.session.lock().await.as_mut() {
            sess.show_nearby = s.show_nearby;
            sess.sync_lobby();
        }
    }
    if restart {
        // the settings screen's preview engine (outside a room) follows device changes too
        restart_preview(&app, &s).await;
    }
    if restart || jitter || profile || metro {
        let self_ui = state.self_ui.lock().unwrap().clone();
        let mut guard = state.session.lock().await;
        if let Some(sess) = guard.as_mut() {
            if metro {
                sess.metronome_volume = s.metronome_volume;
                sess.apply_metronome();
            }
            if profile {
                sess.set_my_track(&s.profile());
                sess.announce_profile(s.profile());
                sess.sync_lobby(); // the nearby list shows the host's name
            }
            if !restart && jitter {
                sess.apply_jitter_settings(&s);
            }
            drop(guard);
            if restart {
                // (with the session let go: the room can be used while the driver closes
                // and opens; see `session::reopen`)
                let (rec, opened) = session::reopen(&app, s.clone(), self_ui).await;
                if let Some(rec) = rec {
                    let _ = app.emit("recording", recording_json(rec));
                }
                match opened {
                    Ok(summary) => session::notice(
                        &app,
                        gachi_lang::tr!("app.audio_settings_applied", summary),
                    ),
                    Err(e) => {
                        session::notice(&app, gachi_lang::tr!("app.e_please_choose", e));
                        return Err(e);
                    }
                }
            }
            if let Some(sess) = state.session.lock().await.as_ref() {
                let _ = app.emit("stats", json!({ "peers": sess.views() }));
            }
        }
    }
    Ok(s)
}

/// (async, on a thread that may wait: a plain command runs on the main thread, and this
/// loads every ASIO driver there is: a slow or hanging one froze the window for up to 16 s)
#[tauri::command]
pub async fn list_devices() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(list_devices_now)
        .await
        .map_err(|e| e.to_string())
}

fn list_devices_now() -> Value {
    // `inputs` = input channel count, so the UI can offer every channel / pair / mic+loopback mix
    let map = |host| {
        // (the listing first: the first driver's channels come from it)
        let devices = engine_devices(host);
        let default_inputs = gachi_engine::device::default_input_channels(host);
        // "system default" on ASIO is the first driver: borrow its channel names
        let default_names = devices
            .iter()
            .find(|d| d.has_input && d.inputs == default_inputs)
            .map(|d| d.input_names.clone())
            .unwrap_or_default();
        let mut list = vec![
            json!({ "id": "", "label": gachi_lang::tr!("app.default_asio_device"), "inputs": default_inputs, "inputNames": default_names }),
        ];
        list.extend(devices.into_iter().map(
            |d| json!({ "id": d.id, "label": d.label, "inputs": d.inputs, "inputNames": d.input_names }),
        ));
        list
    };
    // ASIO only: no ASIO driver installed → no devices (the UI shows how to get one)
    let devices = if gachi_engine::device::ASIO_AVAILABLE {
        map(HostChoice::Asio)
    } else {
        vec![]
    };
    let has_asio = devices.len() > 1; // more than the "first driver" entry
    json!({ "devices": if has_asio { devices } else { vec![] }, "hasAsio": has_asio })
}

/// Reachability (STUN) + whether our public port matches the local one (a hint that P2P
/// will be easy), IPv6 and UPnP. Read-only: nothing is mapped on the router here.
#[tauri::command]
pub async fn check_network() -> Result<Value, String> {
    let (probe, link, ipv6, upnp) = tauri::async_runtime::spawn_blocking(move || {
        let (ipv6, upnp) = std::thread::scope(|s| {
            let v6 = s.spawn(gachi_net::has_ipv6);
            let up = s.spawn(gachi_net::upnp::available);
            (v6.join().unwrap_or(false), up.join().unwrap_or(false))
        });
        let servers: Vec<String> = gachi_net::stun::DEFAULT_SERVERS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let probe = gachi_net::probe(&servers).ok().flatten();
        let link = probe.map_or(gachi_proto::signal::Link::Unknown, |(_, public, _)| {
            crate::link::detect(public)
        });
        (probe, link, ipv6, upnp)
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(
        json!({ "link": crate::link::label(link), "ipv6": ipv6, "nat": nat_kind(probe), "upnp": upnp }),
    )
}

/// What sits between us and the internet, from our local address and the public one STUN
/// saw: `none` (plugged straight into the modem or wall jack: our address is public),
/// `symmetric` (two servers saw us at different ports: every destination gets its own, so
/// peers behind ordinary routers may not get through), `cone` (both saw the same address,
/// or the router kept our port: P2P will be easy), `unknown` (one answer, port changed).
fn nat_kind(
    probe: Option<(
        std::net::SocketAddr,
        std::net::SocketAddr,
        gachi_net::stun::Mapping,
    )>,
) -> &'static str {
    use gachi_net::stun::Mapping;
    match probe {
        None => "offline",
        Some((local, public, _)) if local.ip() == public.ip() => "none",
        Some((_, _, Mapping::Varies { .. })) => "symmetric",
        Some((_, _, Mapping::Steady)) => "cone",
        Some((local, public, _)) if local.port() == public.port() => "cone",
        Some(_) => "unknown",
    }
}

#[cfg(test)]
mod nat_tests {
    #[test]
    fn a_public_address_on_the_pc_means_no_router() {
        use gachi_net::stun::Mapping;
        let with = |l: &str, g: &str, m: Mapping| {
            super::nat_kind(Some((l.parse().unwrap(), g.parse().unwrap(), m)))
        };
        let p = |l: &str, g: &str| with(l, g, Mapping::Unknown);
        // two servers agree: fine even though the router changed the port
        assert_eq!(
            with("192.168.0.7:4000", "203.0.113.5:61022", Mapping::Steady),
            "cone"
        );
        assert_eq!(
            with(
                "192.168.0.7:4000",
                "203.0.113.5:61022",
                Mapping::Varies { step: 1 }
            ),
            "symmetric"
        );
        assert_eq!(p("203.0.113.5:4000", "203.0.113.5:4000"), "none");
        assert_eq!(p("192.168.0.7:4000", "203.0.113.5:4000"), "cone");
        assert_eq!(p("192.168.0.7:4000", "203.0.113.5:61022"), "unknown");
        // carrier NAT hands the PC a 100.64/10 address: still a NAT
        assert_eq!(p("100.72.1.9:4000", "203.0.113.5:61022"), "unknown");
        assert_eq!(super::nat_kind(None), "offline");
    }
}

#[tauri::command]
pub async fn create_room(app: AppHandle, opts: CreateOpts) -> Result<RoomView, String> {
    let gen = app
        .state::<AppState>()
        .join_gen
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    // (no longer than the password box takes when joining: a longer one could not be typed in)
    let password: String = opts.password.chars().take(session::PASSWORD_MAX).collect();
    let password = (!password.is_empty()).then_some(password);
    let name = if opts.name.trim().is_empty() {
        session::new_room_name()
    } else {
        opts.name
    };
    session::start(
        app,
        Intent::Create {
            name,
            max_peers: opts.max_peers,
            password,
        },
        gen,
    )
    .await
}

/// Errors come back as `{ kind, message }`; kind `password_required` / `wrong_password`
/// makes the UI ask for the room password and call again with it.
#[tauri::command]
pub async fn join_room(
    app: AppHandle,
    code: String,
    password: Option<String>,
) -> Result<RoomView, Value> {
    let fail = |kind: &str, message: &str| json!({ "kind": kind, "message": message });
    let code = code::normalize(&code)
        .ok_or_else(|| fail("bad_code", &gachi_lang::tr!("app.doesnt_look_like")))?;
    let gen = app
        .state::<AppState>()
        .join_gen
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    let password = password.filter(|p| !p.is_empty());
    session::start(app, Intent::Join { code, password }, gen)
        .await
        .map_err(|e| match e.as_str() {
            session::ERR_PASSWORD_REQUIRED => fail(&e, &gachi_lang::tr!("app.room_has_password")),
            session::ERR_WRONG_PASSWORD => fail(&e, &gachi_lang::tr!("app.wrong_password")),
            _ => fail("other", &e),
        })
}

#[tauri::command]
pub fn cancel_join(state: State<'_, AppState>) {
    state.join_gen.fetch_add(1, Ordering::SeqCst);
}

/// Starts the app again (a setting that only applies at start, like GPU acceleration).
/// Leaves the room first, so the others hear we left rather than wait for us to go silent.
#[tauri::command]
pub async fn restart_app(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    drop(state.session.lock().await.take());
    drop(state.preview.lock().await.take());
    // let the goodbye packets go out
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    app.restart();
}

#[tauri::command]
pub async fn leave_room(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    // Nothing opens the audio device until the room's engine has let go of it. The page asks
    // for the settings' preview the moment a room is left with the settings open (a room that
    // ended under them: closed by its host, put out, the connection lost) and in DAW mode;
    // that came while the room's engine was still closing, and two engines on one ASIO
    // driver took the program down: the driver's thread wrote into buffers just freed.
    // (`set_preview` and `probe_device` take this lock first, and wait here.)
    // (and a reopening of the room's own device under way is waited out: it would hand its
    // new engine to a room that is gone, and hold the driver meanwhile)
    let _gate = state.device_gate.lock().await;
    let _no_preview = state.preview.lock().await;
    let old = state.session.lock().await.take();
    // stops audio, closes the socket and the signaling connection. Off the async runtime:
    // letting go of the device waits on its driver (seconds, if the driver hangs)
    let _ = tauri::async_runtime::spawn_blocking(move || drop(old)).await;
    Ok(())
}

/// Host: puts a member out of the room (and keeps them out).
#[tauri::command]
pub async fn kick_peer(app: AppHandle, id: String) -> Result<(), String> {
    let id: u16 = id.parse().map_err(|_| "bad peer id")?;
    let state = app.state::<AppState>();
    let mut guard = state.session.lock().await;
    guard.as_mut().ok_or("not in a room")?.kick(&app, id)
}

#[tauri::command]
pub async fn set_peer(app: AppHandle, id: String, patch: PeerPatch) -> Result<(), String> {
    let id: u16 = id.parse().map_err(|_| "bad peer id")?;
    let state = app.state::<AppState>();
    let mut guard = state.session.lock().await;
    let s = guard.as_mut().ok_or("not in a room")?;
    {
        let p = s.peers.get_mut(&id).ok_or("unknown peer")?;
        let ui = &mut p.ui;
        if let Some(v) = patch.volume {
            ui.volume_db = v.clamp(FADER_MIN_DB, FADER_MAX_DB)
        }
        if let Some(v) = patch.pan {
            ui.pan = v.clamp(-100.0, 100.0)
        }
        if let Some(v) = patch.mute {
            ui.mute = v
        }
        if let Some(v) = patch.solo {
            ui.solo = v
        }
        if let Some(v) = patch.rec {
            ui.rec = v
        }
        if let Some(v) = patch.jitter_auto {
            ui.jitter_auto = v
        }
        if let Some(v) = patch.jitter_packets {
            ui.jitter_packets = v.clamp(1, 12)
        }
        Session::apply_peer(p);
    }
    s.update_jitter_targets();
    let _ = app.emit("stats", json!({ "peers": s.views() }));
    Ok(())
}

#[tauri::command]
pub async fn set_self(app: AppHandle, patch: SelfPatch) -> Result<(), String> {
    let state = app.state::<AppState>();
    let ui = {
        let mut g = state.self_ui.lock().unwrap();
        if let Some(v) = patch.gain {
            g.gain_db = v.clamp(-24.0, 24.0)
        }
        // (what is sent is not made louder by its fader: over 0 dB it would break up, and the
        // input's gain is for that)
        if let Some(v) = patch.level {
            g.level_db = v.clamp(FADER_MIN_DB, 0.0)
        }
        if let Some(v) = patch.muted {
            g.muted = v
        }
        if let Some(v) = patch.monitor {
            g.monitor = v
        }
        if let Some(v) = patch.monitor_db {
            g.monitor_db = v.clamp(FADER_MIN_DB, FADER_MAX_DB)
        }
        if let Some(v) = patch.own {
            g.own = v
        }
        if let Some(v) = patch.rec {
            g.rec = v
        }
        g.clone()
    };
    if let Some(s) = state.session.lock().await.as_ref() {
        s.apply_self(&ui);
    }
    Ok(())
}

#[tauri::command]
pub async fn set_master(app: AppHandle, db: f32) -> Result<(), String> {
    let state = app.state::<AppState>();
    let ui = {
        let mut g = state.self_ui.lock().unwrap();
        g.master_db = db.clamp(FADER_MIN_DB, FADER_MAX_DB);
        g.clone()
    };
    if let Some(s) = state.session.lock().await.as_ref() {
        s.apply_self(&ui);
    }
    Ok(())
}

#[tauri::command]
pub async fn set_metronome(
    app: AppHandle,
    on: bool,
    bpm: f32,
    beats: Option<u8>,
    count_in: Option<u8>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut guard = state.session.lock().await;
    let s = guard.as_mut().ok_or("not in a room")?;
    // the host sets the start on its clock and tells everyone (us included, via an event)
    s.request_metronome(&app, on, bpm, beats.unwrap_or(4), count_in.unwrap_or(0));
    Ok(())
}

/// The settings screen's own audio engine (outside a room): feeds the input meter and the
/// test tone, and lets you hear yourself if monitoring is on. Nothing is sent anywhere.
pub struct Preview {
    engine: gachi_engine::Engine,
    task: tauri::async_runtime::JoinHandle<()>,
    /// opened only to read the device's format and latency for the home screen (see
    /// `probe_device`): closes itself unless the settings screen takes it over
    probe: bool,
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Nowhere;
impl gachi_engine::PacketSink for Nowhere {
    fn send(&mut self, _: &gachi_engine::Frame) {}
}

fn open_preview(app: &AppHandle, settings: &Settings) -> Result<Preview, String> {
    let engine =
        gachi_engine::Engine::start(settings.engine_config(), Box::new(Nowhere)).map_err(|e| {
            tracing::warn!(device = %settings.device, "audio device (settings preview): {e}");
            session::open_error(settings, &e)
        })?;
    if let Some(n) = crate::session::device_note(settings, &engine.info) {
        session::notice(app, n);
    }
    let ui = app.state::<AppState>().self_ui.lock().unwrap().clone();
    let p = &engine.self_params;
    p.input_gain.store(ui.input_gain());
    p.muted.store(ui.muted, Ordering::Relaxed);
    p.monitor.store(ui.monitor, Ordering::Relaxed);
    p.monitor_gain
        .store(gachi_engine::db_to_gain(ui.monitor_db));
    p.own_on.store(ui.own, Ordering::Relaxed);
    let app2 = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        // (twice a second: the figures, and every fourth time the driver. The meters are asked
        // for by the page, see `take_levels`; in DAW mode this runs for as long as the home
        // screen is up, and woke 30 times a second to do nothing 14 times out of 15)
        let mut iv = tokio::time::interval(std::time::Duration::from_millis(500));
        // (the first tick comes at once: the device has not run a block yet)
        iv.tick().await;
        let mut n = 0u32;
        let mut seen = (0u32, 0u32);
        let mut hung = false;
        loop {
            let mut restart = false;
            iv.tick().await;
            n = n.wrapping_add(1);
            let state = app2.state::<AppState>();
            let guard = state.preview.lock().await;
            let Some(p) = guard.as_ref() else { return };
            let e = &p.engine;
            {
                // the device's real format (ASIO: whatever its driver runs)
                let (i, o) = e.real_buffers();
                let _ = app2.emit("stats", {
                    let (lat_in, lat_out, reported) = e.device_latency_ms();
                    json!({
                        "peers": [],
                        "localMs": e.local_latency_ms(),
                        "bufferFrames": i.max(o),
                        "packetFrames": e.packet_frames(),
                        "sampleRate": e.info.sample_rate,
                        "plugin": e.plugin_connected(),
                        "pluginWaiting": e.plugin_waiting(),
                        "latency": { "inMs": lat_in, "outMs": lat_out, "reported": reported },
                    })
                });
            }
            // a new rate/buffer in the driver panel, or the device stopped: reopen
            if n.is_multiple_of(4) || n == 2 {
                match e.driver_check() {
                    // the driver hangs: reopening would hang too. Say so once
                    gachi_engine::DriverCheck::NoAnswer => {
                        if !hung {
                            hung = true;
                            session::notice(&app2, session::driver_hung());
                        }
                    }
                    check => {
                        hung = false;
                        if check == gachi_engine::DriverCheck::Changed || e.is_broken(&mut seen) {
                            restart = true;
                        }
                    }
                }
            }
            if restart {
                let settings = state.settings.lock().unwrap().clone();
                drop(guard);
                tauri::async_runtime::spawn(restart_preview_after_change(app2.clone(), settings));
                return;
            }
        }
    });
    Ok(Preview {
        engine,
        task,
        probe: false,
    })
}

/// Opens the ASIO driver's own control panel (buffer size, sample rate). The settings screen
/// has the driver open (its preview engine); changes made there are picked up by the usual
/// driver check and the device reopened.
#[tauri::command]
pub async fn open_control_panel() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(gachi_engine::open_asio_control_panel)
        .await
        .map_err(|e| e.to_string())?
}

/// Opens/closes the preview engine. Ignored while in a room (the room's engine is used).
/// Asking for it while it runs keeps it as it is (DAW mode keeps it up on the home screen
/// so the plugin can connect: reopening would cut the plugin off for a moment).
#[tauri::command]
pub async fn set_preview(app: AppHandle, on: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut preview = state.preview.lock().await;
    if on && preview.is_some() {
        // the settings screen keeps what a home-screen probe opened
        if let Some(p) = preview.as_mut() {
            p.probe = false;
        }
        return Ok(());
    }
    // a home-screen probe closes itself once it has read the device: closing it now would
    // cut it off before that (at start the app asks for both at once)
    if !on && preview.as_ref().is_some_and(|p| p.probe) {
        return Ok(());
    }
    drop(preview.take()); // close first: ASIO devices open only once
    if on && state.session.lock().await.is_none() && !state.entering() {
        let settings = state.settings.lock().unwrap().clone();
        *preview = Some(open_preview(&app, &settings)?);
    }
    Ok(())
}

/// Home screen (no room, settings closed, ASIO): the device is closed there so other
/// programs can have it, so whatever its control panel changed meanwhile (buffer size, a
/// safe mode) goes unseen. Opens it for a moment: its preview sends the format and, after
/// a driver check, the latency the driver reports; then it is closed again. Does nothing
/// when the device is busy (another program has it) or already open.
#[tauri::command]
pub async fn probe_device(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    {
        let mut preview = state.preview.lock().await;
        // (asked while holding the preview: a join that starts now waits for it and closes
        // what is opened here; one that started before is seen)
        if preview.is_some() || state.entering() || state.session.lock().await.is_some() {
            return Ok(());
        }
        let settings = state.settings.lock().unwrap().clone();
        if settings.daw_mode() {
            return Ok(());
        }
        match open_preview(&app, &settings) {
            Ok(mut p) => {
                p.probe = true;
                *preview = Some(p);
            }
            Err(e) => {
                tracing::debug!("device probe: {e}");
                return Ok(());
            }
        }
    }
    // stats go out every 0.5 s; the driver is asked again at 1 s: the one at 1.5 s has it
    tokio::time::sleep(std::time::Duration::from_millis(1700)).await;
    let mut preview = state.preview.lock().await;
    if preview.as_ref().is_some_and(|p| p.probe) {
        drop(preview.take());
    }
    Ok(())
}

/// The device changed under the settings screen (driver panel, unplugged): open it again.
async fn restart_preview_after_change(app: AppHandle, settings: Settings) {
    restart_preview(&app, &settings).await;
    session::notice(&app, gachi_lang::tr!("app.audio_device_settings"));
}

/// Device settings changed while the settings screen is open outside a room.
async fn restart_preview(app: &AppHandle, settings: &Settings) {
    let state = app.state::<AppState>();
    let mut preview = state.preview.lock().await;
    if preview.take().is_some() {
        match open_preview(app, settings) {
            Ok(p) => *preview = Some(p),
            Err(e) => session::notice(app, e),
        }
    }
}

/// 1.5 s of 1 kHz on the output. Outside a room, opens the device just for the tone.
#[tauri::command]
pub async fn play_test_tone(app: AppHandle) -> Result<(), String> {
    const SECS: f32 = 1.5;
    let state = app.state::<AppState>();
    // in a room: on the room's engine (none for the moment its device is being reopened)
    if let Some(s) = state.session.lock().await.as_ref() {
        if let Some(e) = s.engine.as_ref() {
            e.play_test_tone(SECS);
        }
        return Ok(());
    }
    // The preview is held for as long as the tone's own engine lives: whoever opens the
    // device next (a room being entered, the settings, the home screen's reading of it)
    // waits here for the tone to end. Left free, a room made during the tone found the
    // driver taken and did not open (and before engines refused that: the program crashed).
    let preview = state.preview.lock().await;
    if let Some(p) = preview.as_ref() {
        p.engine.play_test_tone(SECS);
        return Ok(());
    }
    if state.entering() || state.session.lock().await.is_some() {
        return Ok(());
    }
    let cfg = state.settings.lock().unwrap().engine_config();
    tauri::async_runtime::spawn_blocking(move || {
        let engine = gachi_engine::Engine::start(cfg, Box::new(Nowhere))
            .map_err(|e| gachi_lang::tr!("app.cant_open_audio", e))?;
        engine.play_test_tone(SECS);
        std::thread::sleep(std::time::Duration::from_secs_f32(SECS + 0.2));
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())??;
    drop(preview);
    Ok(())
}

fn recordings_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let docs = app
        .path()
        .document_dir()
        .map_err(|e| gachi_lang::tr!("app.cant_find_documents", e))?;
    let base = docs.join("gachi");
    // the folder is named in the user's language; one already there (an earlier
    // version's "녹음", or another language's) keeps being used
    let names = ["녹음", "Recordings", "録音"];
    let mine = gachi_lang::tr!("rec.folder");
    let found = std::iter::once(mine.as_str())
        .chain(names)
        .map(|n| base.join(n))
        .find(|d| d.is_dir());
    Ok(found.unwrap_or_else(|| base.join(mine)))
}

pub fn recording_json(r: Result<gachi_engine::RecordingResult, String>) -> Value {
    match r {
        Ok(r) => json!({
            "recording": false,
            "dir": r.dir.display().to_string(),
            "files": r.files.len(),
            "seconds": r.seconds,
            "dropped": r.dropped_blocks,
        }),
        Err(e) => json!({ "recording": false, "error": e }),
    }
}

/// Starts/stops recording one WAV per person into Documents/gachi/녹음/<date time>/.
#[tauri::command]
pub async fn set_recording(app: AppHandle, on: bool) -> Result<Value, String> {
    let base = recordings_dir(&app)?;
    let state = app.state::<AppState>();
    let (format, mix) = {
        let g = state.settings.lock().unwrap();
        (g.rec_format(), g.rec_mix)
    };
    let mut guard = state.session.lock().await;
    let s = guard
        .as_mut()
        .ok_or_else(|| gachi_lang::tr!("app.join_room_record"))?;
    if on {
        let dir = s.start_recording(base, format, mix)?;
        Ok(json!({ "recording": true, "dir": dir.display().to_string() }))
    } else {
        // the files are written out with the session let go: until they were (seconds, for a
        // long recording), the meters stood still and nothing in the room could be done
        let stopped = s
            .stop_recording_begin()
            .unwrap_or_else(|| Err(gachi_lang::tr!("app.not_recording")));
        drop(guard);
        let r = match stopped {
            Ok(files) => tauri::async_runtime::spawn_blocking(move || files.finish())
                .await
                .unwrap_or_else(|e| Err(e.to_string())),
            Err(e) => Err(e),
        };
        let v = recording_json(r);
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            return Err(e.to_string());
        }
        Ok(v)
    }
}

/// The meters since they were last asked for: the room's, else the settings screen's (its
/// device open outside a room), else nothing.
///
/// Asked for by the page, about 30 times a second while it shows them, instead of sent to
/// it: an event is a script run in the page, and WebView2 kept something of each one (in a
/// room the page grew by some 30 KB a second, about 100 MB an hour). An answer to a command
/// is not a script. A page that cannot be seen does not ask.
///
/// Never waits: with the room or the device busy (a tick of theirs, a device reopening)
/// there is nothing this time, and the peaks stay for the next.
#[tauri::command]
pub fn take_levels(state: State<'_, AppState>) -> Option<Value> {
    if let Ok(guard) = state.session.try_lock() {
        if let Some(s) = guard.as_ref() {
            return Some(s.levels());
        }
    } else {
        return None;
    }
    let guard = state.preview.try_lock().ok()?;
    let e = &guard.as_ref()?.engine;
    let inputs = e.input_meters.take(e.info.input_channels as usize);
    Some(
        json!({ "self": e.self_meter.take(), "master": e.master_meter.take(), "peers": {}, "inputs": inputs }),
    )
}

/// Opens the recordings folder in Explorer/Finder.
#[tauri::command]
pub fn open_recordings(app: AppHandle) -> Result<(), String> {
    let dir = recordings_dir(&app)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(&dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// The page has drawn itself with the saved settings: show the window.
#[tauri::command]
pub fn window_ready(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// The app's version, for the settings' "about" page.
#[tauri::command]
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Where the source is (shown and opened from the settings' "about" page).
pub const REPOSITORY: &str = "https://github.com/aape133/gachi";

/// Opens the project's page in the browser (this one address only: the page cannot ask
/// for another).
#[tauri::command]
pub fn open_repo() -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(REPOSITORY)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Is a newer version out? (asks GitHub for the latest release; see `update`)
#[tauri::command]
pub async fn check_update() -> Result<Option<crate::update::Update>, String> {
    let r = crate::update::check().await;
    match &r {
        Ok(Some(u)) => tracing::info!(version = %u.version, "a newer version is out"),
        Ok(None) => tracing::info!("update check: this is the latest"),
        Err(e) => tracing::info!(error = %e, "update check failed"),
    }
    r
}

/// Opens the latest release's page in the browser (this one address only).
#[tauri::command]
pub fn open_release() -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(format!("{REPOSITORY}/releases/latest"))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Opens the folder with the log files (to send one along with a problem report).
#[tauri::command]
pub fn open_logs() -> Result<(), String> {
    let dir = crate::log_dir().ok_or_else(|| gachi_lang::tr!("app.cant_find_log"))?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::process::Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Host: turn the code on/off, change the room size, or switch to a new code.
#[tauri::command]
pub async fn set_room(
    app: AppHandle,
    open: Option<bool>,
    max_peers: Option<u8>,
    new_code: Option<bool>,
) -> Result<(), String> {
    crate::gate::configure(app, open, max_peers, new_code.unwrap_or(false)).await
}

#[tauri::command]
pub async fn send_chat(app: AppHandle, text: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let guard = state.session.lock().await;
    let s = guard.as_ref().ok_or("not in a room")?;
    s.send_chat(&text); // peer to peer, encrypted; the server never sees chat
    Ok(())
}

/// Rooms open on our network (same public address and subnet), for the home screen.
#[tauri::command]
pub async fn nearby_rooms(
    state: State<'_, AppState>,
) -> Result<Vec<gachi_proto::signal::NearbyRoom>, String> {
    use gachi_proto::signal::FromWorker;
    let server = state.settings.lock().unwrap().server.clone();
    let lan = session::lan_prefix();
    // (no router to recognise each other by: nothing is nearby)
    let Some(gw) = gachi_net::router_key() else {
        return Ok(Vec::new());
    };
    let url = crate::signal::Endpoint::parse(&server).nearby_url(lan.as_deref(), Some(&gw));
    let (_tx, mut rx) = crate::signal::connect(&url).await?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
        Ok(Some(FromWorker::Nearby { rooms })) => Ok(rooms),
        Ok(Some(FromWorker::Error { kind, message })) => {
            Err(crate::refusal::localize(kind, &message))
        }
        _ => Err(gachi_lang::tr!("app.no_answer_server")),
    }
}
