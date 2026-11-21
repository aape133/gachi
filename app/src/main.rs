#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod deeplink;
mod diagnose;
mod refusal;
mod gate;
mod link;
mod session;
mod settings;
mod signal;
mod update;
mod view;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
pub struct AppState {
    pub settings: Mutex<settings::Settings>,
    pub self_ui: Mutex<view::SelfUi>,
    pub session: tokio::sync::Mutex<Option<session::Session>>,
    /// audio device opened by the settings screen outside a room (input meter, test tone)
    pub preview: tokio::sync::Mutex<Option<commands::Preview>>,
    /// bumped by every create/join/cancel; a start that sees a newer value gives up
    pub join_gen: AtomicU64,
    /// creates/joins under way (they open the audio device partway through, before there is
    /// a session to see): nothing else may open the device meanwhile. An ASIO driver opened
    /// twice at once took the whole program down (leave, then join again 1.6 s later, just
    /// as the home screen's device probe ran).
    pub starting: std::sync::atomic::AtomicU32,
    /// held while a room's audio device is being reopened with the session let go (see
    /// `session::reopen`), and by a create/join from before it lets the old room go: one
    /// reopen at a time, and none still under way when the next room opens the device
    pub device_gate: tokio::sync::Mutex<()>,
    /// room code from a `gachi://join/…` link the UI has not picked up yet
    pub pending_join: Mutex<Option<String>>,
    /// where the settings are saved (app config dir)
    pub settings_path: OnceLock<PathBuf>,
}

impl AppState {
    /// A room is being created or joined right now (see `starting`).
    pub fn entering(&self) -> bool {
        self.starting.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    pub fn save_settings(&self) {
        let (Some(path), Ok(s)) = (self.settings_path.get(), self.settings.lock()) else { return };
        if let Err(e) = s.save(path) {
            tracing::warn!("could not save settings: {e}");
        }
    }
}

/// Where the log files are.
pub fn log_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("app.gachi.desktop").join("logs"))
}

/// A log file that stops growing at `LOG_MAX` bytes (an app left running for days, or a
/// fault that repeats, must not fill the disk): what comes after is dropped.
struct Capped {
    file: std::fs::File,
    left: usize,
}

const LOG_MAX: usize = 2 * 1024 * 1024;

impl std::io::Write for Capped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Ok(buf.len());
        }
        if buf.len() > self.left {
            self.left = 0;
            let _ = self.file.write_all(b"(log full: nothing more is written to this file)\n");
            return Ok(buf.len());
        }
        self.left -= buf.len();
        self.file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// `%APPDATA%\\app.gachi.desktop\\logs\\gachi-<time>.log`, one per launch; keeps the newest 5.
/// Stays on this PC: nothing is ever sent anywhere.
fn log_file() -> Option<Capped> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let mut old: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    old.sort();
    for p in old.iter().rev().skip(4) {
        let _ = std::fs::remove_file(p);
    }
    let name = format!("gachi-{}.log", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    Some(Capped { file: std::fs::File::create(dir.join(name)).ok()?, left: LOG_MAX })
}

/// WebView2 options. wry's defaults first (it drops them when given its own), then: keep
/// painting and running timers while the window is behind another one or unfocused (the
/// device latency follows the driver every 2 s and should show it), and, if the setting
/// says so, no graphics card.
///
/// And nothing of a web browser that this app has no use for: WebView2 is Edge's engine and,
/// left alone, downloads Edge's parts into the app's data folder (a video DRM module, an
/// ad-filter list, a spelling dictionary, speech recognition: ~70 MB) and keeps a disk
/// cache of pages. The app shows its own bundled page and nothing from the web.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,OptimizationHints,Translate,msEdgeTranslate,SpellcheckServiceMultilingual \
    --disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows \
    --disable-component-update --disable-background-networking --disable-sync --disable-spell-checking \
    --disk-cache-size=1048576 --media-cache-size=1048576 --disable-gpu-shader-disk-cache";

/// The app's one window. Made here rather than in tauri.conf.json: whether it uses the
/// graphics card is a saved setting.
/// `language`: the saved choice; the page gets every language file with it, before its own
/// scripts run (no loading at startup, no moment in another language).
fn main_window(app: &tauri::AppHandle, gpu: bool, language: &str) -> tauri::Result<()> {
    let builder = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
        .title("gachi")
        .inner_size(1280.0, 800.0)
        // (the narrowest the screens are laid out for: below 1040 the room's top bar takes two
        // lines, below 900 the track view stacks each row's meter over its volume; below 760 the
        // home screen's two cards stack. 620 high: the mixer's strips still show their
        // meters, faders and names, in every language)
        .min_inner_size(600.0, 620.0)
        .initialization_script(gachi_lang::page_script(language))
        // shown once the page has the saved settings in it (`window_ready`): no empty
        // window, no page that changes under one's eyes as name, theme and devices arrive
        .visible(false);
    #[cfg(windows)]
    let builder = {
        let args = if gpu { BROWSER_ARGS.to_string() } else { format!("{BROWSER_ARGS} --disable-gpu --disable-gpu-compositing") };
        builder.additional_browser_args(&args)
    };
    #[cfg(not(windows))]
    let _ = gpu;
    tracing::info!(gpu, "window");
    let window = builder.build()?;
    // (a page that never says it is ready must not leave the app invisible)
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let _ = window.show();
    });
    Ok(())
}

fn main() {
    use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};
    use tracing_subscriber::prelude::*;
    let writer = match log_file() {
        Some(f) => BoxMakeWriter::new(std::io::stderr.and(Mutex::new(f))),
        None => BoxMakeWriter::new(std::io::stderr),
    };
    let layer = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(writer);
    match tracing_subscriber::EnvFilter::try_from_default_env() {
        // a developer's own filter (RUST_LOG)
        Ok(filter) => tracing_subscriber::registry().with(layer.with_filter(filter)).init(),
        // what happened (info and up); the developer's detail only when the settings ask
        // for the full log, looked up at every event so the switch works at once
        Err(_) => {
            let filter = tracing_subscriber::filter::dynamic_filter_fn(|meta, _| {
                let level = *meta.level();
                level <= tracing::Level::INFO
                    || (level <= tracing::Level::DEBUG && meta.target().starts_with("gachi") && settings::VERBOSE_LOG.load(std::sync::atomic::Ordering::Relaxed))
            });
            tracing_subscriber::registry().with(layer.with_filter(filter)).init();
        }
    }
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "gachi starting");
    // live audio: no Windows power throttling, even with the window in the background
    // (forced on, it slowed sending and receiving by ~40% on a desktop PC; laptops with
    // efficiency cores lose more). Only this process; the plugin never does this to a DAW.
    gachi_net::prio::power_throttling(Some(false));
    // TLS for the signaling connection (wss://); `ring` needs no extra build tools
    let _ = rustls::crypto::ring::default_provider().install_default();
    signal::warm_tls();

    let builder = tauri::Builder::default();
    // Release builds: one window; a second launch (e.g. clicking an invite link) hands its
    // link to the running app. Debug builds allow several windows for testing on one PC.
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        if let Some(w) = tauri::Manager::get_webview_window(app, "main") {
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
    }));
    builder
        .plugin(tauri_plugin_deep_link::init())
        .setup(|app| {
            use tauri_plugin_deep_link::DeepLinkExt;
            // saved settings (name, devices, theme…)
            if let Ok(dir) = tauri::Manager::path(app).app_config_dir() {
                let path = dir.join("settings.json");
                let state = tauri::Manager::state::<AppState>(app);
                let loaded = settings::Settings::load(&path);
                settings::VERBOSE_LOG.store(loaded.verbose_log, std::sync::atomic::Ordering::Relaxed);
                *state.settings.lock().unwrap() = loaded;
                let _ = state.settings_path.set(path);
            }
            let (gpu, language) = {
                let s = tauri::Manager::state::<AppState>(app).settings.lock().unwrap().clone();
                (s.gpu, s.language)
            };
            main_window(app.handle(), gpu, &language)?;
            // installers register gachi:// themselves; dev builds register for this user at runtime
            #[cfg(all(debug_assertions, any(windows, target_os = "linux")))]
            if let Err(e) = app.deep_link().register_all() {
                tracing::warn!("could not register gachi:// links: {e}");
            }
            let handle = app.handle().clone();
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                for url in urls {
                    deeplink::handle(&handle, url.as_str());
                }
            }
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    deeplink::handle(&handle, url.as_str());
                }
            });
            Ok(())
        })
        // closing: the window goes at once; saying goodbye to the room and closing the audio
        // device follow (a few hundred ms that need no window)
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let _ = window.hide();
            }
        })
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::update_settings,
            commands::list_devices,
            commands::check_network,
            commands::create_room,
            commands::join_room,
            commands::cancel_join,
            commands::leave_room,
            commands::set_peer,
            commands::kick_peer,
            commands::set_self,
            commands::set_master,
            commands::send_chat,
            commands::set_metronome,
            commands::play_test_tone,
            commands::set_recording,
            commands::open_recordings,
            commands::take_levels,
            commands::open_logs,
            commands::open_repo,
            commands::window_ready,
            commands::app_version,
            commands::check_update,
            commands::open_release,
            commands::set_preview,
            commands::probe_device,
            commands::restart_app,
            commands::open_control_panel,
            commands::set_room,
            commands::nearby_rooms,
            deeplink::take_pending_join,
            deeplink::qr_svg,
        ])
        .build(tauri::generate_context!())
        .expect("error while building gachi")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // closing the window must still say goodbye: the net layer tells every peer we
                // left on purpose (so the next host takes over right away) and the host's
                // connection to the connection server closes
                let state = tauri::Manager::state::<AppState>(app);
                let session = tauri::async_runtime::block_on(async { state.session.lock().await.take() });
                let to_server = session.as_ref().is_some_and(|s| s.gate.is_some());
                drop(session);
                drop(tauri::async_runtime::block_on(async { state.preview.lock().await.take() }));
                // the process ends right after this: give the host's connection the moment
                // it needs to say it is closing (the window is hidden already)
                if to_server {
                    tauri::async_runtime::block_on(tokio::time::sleep(std::time::Duration::from_millis(250)));
                }
            }
        });
}
