//! Invite links.
//!
//! - `gachi://join/K7M-Q2X` opens the app and joins (registered as a URL scheme).
//! - `https://<server>/j/K7M-Q2X` is what people share in messengers (clickable everywhere);
//!   the server's page hands it over to `gachi://`.
//!
//! A link that arrives before the UI is ready is kept until the UI asks for it.

use crate::signal::Endpoint;
use crate::AppState;
use gachi_proto::code;
use tauri::{AppHandle, Emitter, Manager};

/// The room code in a `gachi://join/CODE` (or `https://…/j/CODE`) link.
pub fn code_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let mut parts = rest.split(['/', '?', '#']).filter(|p| !p.is_empty());
    let first = parts.next()?;
    let raw = if first.eq_ignore_ascii_case("join") {
        parts.next()?
    } else {
        // https://host/j/CODE: skip the host
        let mut p = parts.skip_while(|p| !p.eq_ignore_ascii_case("j"));
        p.next()?;
        p.next()?
    };
    code::normalize(raw)
}

/// Shareable https link for a room on `server`.
pub fn invite_url(server: &str, room_code: &str) -> String {
    let e = Endpoint::parse(server);
    let scheme = if e.secure { "https" } else { "http" };
    format!("{scheme}://{}/j/{}", e.host_port, code::display(room_code))
}

/// QR code (SVG) for an invite link, made locally (no web service sees the link).
#[tauri::command]
pub fn qr_svg(text: String) -> Result<String, String> {
    if text.len() > 512 {
        return Err("too long".into());
    }
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M).map_err(|e| e.to_string())?;
    Ok(code
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(128, 128)
        .quiet_zone(true)
        .dark_color(qrcode::render::svg::Color("#0c0d10"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build())
}

/// Called for every opened link (at startup or while running).
pub fn handle(app: &AppHandle, url: &str) {
    let Some(code) = code_from_url(url) else {
        tracing::warn!("ignoring a link without a room code");
        return;
    };
    // (the log gets the start of the code only: with the whole of it, a log passed on
    // would let its reader into the room)
    tracing::info!(code = %format!("{}…", code.chars().take(3).collect::<String>()), "join link");
    *app.state::<AppState>().pending_join.lock().unwrap() = Some(code.clone());
    let _ = app.emit("join-link", serde_json::json!({ "code": code }));
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// The UI asks once it is ready (and after handling a `join-link` event, to clear it).
#[tauri::command]
pub fn take_pending_join(app: AppHandle) -> Option<String> {
    app.state::<AppState>().pending_join.lock().unwrap().take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_links() {
        assert_eq!(code_from_url("gachi://join/K7M-Q2X").as_deref(), Some("K7MQ2X"));
        assert_eq!(code_from_url("gachi://join/k7mq2x/").as_deref(), Some("K7MQ2X"));
        assert_eq!(code_from_url("https://jam.example.com:7700/j/K7M-Q2X").as_deref(), Some("K7MQ2X"));
        assert_eq!(code_from_url("gachi://join/").as_deref(), None);
        assert_eq!(code_from_url("gachi://settings").as_deref(), None);
        assert_eq!(code_from_url("gachi://join/!!!!").as_deref(), None);
    }

    #[test]
    fn qr_is_svg() {
        let svg = qr_svg("https://jam.example.com:7700/j/K7M-Q2X".into()).unwrap();
        assert!(svg.contains("<svg") && svg.contains("#0c0d10"));
    }

    #[test]
    fn invite_urls() {
        assert_eq!(invite_url("jam.example.com:7700", "K7MQ2X"), "https://jam.example.com:7700/j/K7M-Q2X");
        assert_eq!(invite_url("127.0.0.1:7700", "K7MQ2X"), "http://127.0.0.1:7700/j/K7M-Q2X");
    }
}
