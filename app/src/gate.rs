//! The host's connection to the connection server (Cloudflare Worker): it keeps the room
//! code open and receives join requests, which the host answers.
//!
//! Only the host holds this connection, and only while the code is turned on. Everyone
//! else is done with the server once they are in. When the host leaves with the code on,
//! the next host takes the code over with the `host_key` shared inside the room.

use crate::session::notice;
use crate::signal::{self, Endpoint};
use crate::AppState;
use gachi_net::joinseal::{self, JoinKey};
use gachi_proto::signal::{ErrorKind, FromWorker, ToWorker};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc::UnboundedSender;

/// The key newcomers seal their join requests to while we host (memory only; new each
/// time the app starts).
pub static JOIN_KEY: LazyLock<JoinKey> = LazyLock::new(JoinKey::generate);

/// While alive, the code stays open. Dropping it closes the connection; the server then
/// keeps the code ~30 s for a takeover (say `close` first to end it right away).
pub struct Gate {
    tx: UnboundedSender<ToWorker>,
    task: JoinHandle<()>,
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Gate {
    pub fn send(&self, msg: ToWorker) {
        let _ = self.tx.send(msg);
    }
}

pub struct Opened {
    pub code: String,
    pub host_key: String,
    pub gate: Gate,
}

/// Opens a new code (`code: None`) or reopens / takes over one.
///
/// The connection to the server is made here, when a room needs it, and not before: one
/// kept ready on the home screen made opening a room ~1.5 s quicker, but every start of the
/// app then left a connection, and an object on the server, that most of the time nobody
/// used.
pub async fn open(
    app: &AppHandle,
    server: &str,
    code: Option<String>,
    host_key: Option<String>,
    max_peers: u8,
    members: u8,
    locked: bool,
) -> Result<Opened, (ErrorKind, String)> {
    // a new code is picked by the server at random, without looking whether a room has it
    // (looking would leave something behind for every code looked at). Should it be taken,
    // which takes luck, ask again
    let tries = if code.is_none() { 3 } else { 1 };
    let mut last = (ErrorKind::BadRequest, String::new());
    for _ in 0..tries {
        match open_once(app, server, code.clone(), host_key.clone(), max_peers, members, locked).await {
            Err((ErrorKind::CodeTaken, m)) => last = (ErrorKind::CodeTaken, m),
            r => return r,
        }
    }
    Err(last)
}

async fn open_once(
    app: &AppHandle,
    server: &str,
    code: Option<String>,
    host_key: Option<String>,
    max_peers: u8,
    members: u8,
    locked: bool,
) -> Result<Opened, (ErrorKind, String)> {
    let started = Instant::now();
    let hello = ToWorker::HostOpen { code: code.clone(), host_key, max_peers, members, join_key: gachi_proto::to_hex(&JOIN_KEY.public), locked };
    let url = Endpoint::parse(server).host_url(code.as_deref());
    let (tx, mut rx) = signal::connect(&url).await.map_err(|e| (ErrorKind::BadRequest, format!("서버에 연결할 수 없어요 ({e})")))?;
    let _ = tx.send(hello);
    let (code, host_key) = match tokio::time::timeout(Duration::from_secs(8), rx.recv()).await {
        Ok(Some(FromWorker::Opened { code, host_key })) => (code, host_key),
        Ok(Some(FromWorker::Error { kind, message })) => return Err((kind, message)),
        Ok(_) => return Err((ErrorKind::BadRequest, "서버 연결이 끊겼어요".into())),
        Err(_) => return Err((ErrorKind::Timeout, "서버 응답이 없어요".into())),
    };
    tracing::info!(ms = started.elapsed().as_millis() as u64, "room code opened");
    let task = tauri::async_runtime::spawn(serve(app.clone(), rx));
    Ok(Opened { code, host_key, gate: Gate { tx, task } })
}

/// Answers join requests until the connection ends; then tries to get the code back.
async fn serve(app: AppHandle, mut rx: tokio::sync::mpsc::UnboundedReceiver<FromWorker>) {
    // the room password stretched into a key (slow: done once per code and password)
    let mut pw_cache: Option<((String, String), [u8; 32])> = None;
    while let Some(msg) = rx.recv().await {
        let state = app.state::<AppState>();
        if let FromWorker::JoinRequest { join, .. } = &msg {
            let room = {
                let guard = state.session.lock().await;
                let Some(s) = guard.as_ref() else { return };
                s.room_state.password.clone().map(|p| (s.room_state.code.clone(), p))
            };
            if let (Some(room), true) = (room, join.pw) {
                if pw_cache.as_ref().is_none_or(|(r, _)| *r != room) {
                    let (code, pw) = room.clone();
                    let key = tauri::async_runtime::spawn_blocking(move || joinseal::password_key(&code, &pw)).await;
                    if let Ok(key) = key {
                        pw_cache = Some((room, key));
                    }
                }
            }
        }
        let mut guard = state.session.lock().await;
        let Some(s) = guard.as_mut() else { return };
        match msg {
            FromWorker::JoinRequest { req, join } => {
                // only the key for this room's current code and password
                let room = s.room_state.password.clone().map(|p| (s.room_state.code.clone(), p));
                let pw = pw_cache.as_ref().filter(|(r, _)| Some(r) == room.as_ref()).map(|(_, k)| k);
                let admission = s.admit_sealed(&app, &join, pw);
                if let Some(g) = &s.gate {
                    g.send(ToWorker::Admit { req, admission });
                }
            }
            FromWorker::Error { kind, message } => {
                tracing::warn!(?kind, "connection server: {message}");
                if kind == ErrorKind::CodeTaken {
                    // another device of ours took the code over (or it expired): stop here
                    s.gate = None;
                    notice(&app, message);
                    return;
                }
            }
            FromWorker::Opened { .. } | FromWorker::Welcome { .. } | FromWorker::Nearby { .. } | FromWorker::Host { .. } => {}
        }
    }
    // the connection dropped (network hiccup): take the code back while we still can
    tracing::warn!("connection server link lost");
    tauri::async_runtime::spawn(take_over(app.clone()));
}

/// We are the host and the code should be open, but we hold no connection: open one with
/// the room's `host_key`. Retries for ~25 s (the server keeps the code 30 s). If the code is
/// gone or taken, opens a new one and tells the room.
pub fn take_over(app: AppHandle) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    // boxed: open → serve → take_over → open would otherwise be an infinitely sized future
    Box::pin(take_over_inner(app))
}

async fn take_over_inner(app: AppHandle) {
    let state = app.state::<AppState>();
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut wait = Duration::from_millis(300);
    loop {
        let (server, code, key, max, members, locked) = {
            let mut guard = state.session.lock().await;
            let Some(s) = guard.as_mut() else { return };
            // (cut off from the room: it has another host by now, and its code is theirs.
            // Reopening it here would take it away from them)
            if !s.is_host || !s.room_state.open || s.cut_off {
                return;
            }
            // a stale gate from before the drop: let it go
            s.gate = None;
            let locked = s.room_state.password.is_some();
            (s.server.clone(), s.room_state.code.clone(), s.room_state.host_key.clone(), s.room_state.max_peers, s.member_count(), locked)
        };
        match open(&app, &server, Some(code.clone()), key, max, members, locked).await {
            Ok(o) => {
                install(&app, o).await;
                return;
            }
            Err((ErrorKind::CodeTaken, _)) => {
                // the code expired and someone else has it now: a new code for this room
                match open(&app, &server, None, None, max, members, locked).await {
                    Ok(o) => {
                        let new = o.code.clone();
                        install(&app, o).await;
                        notice(&app, format!("방 코드가 {}(으)로 바뀌었어요.", gachi_proto::code::display(&new)));
                    }
                    Err((_, e)) => notice(&app, format!("입장 코드를 다시 열지 못했어요 ({e})")),
                }
                return;
            }
            Err((_, e)) if Instant::now() > deadline => {
                notice(&app, format!("입장 코드를 다시 열지 못했어요 ({e}). 합주는 계속돼요."));
                return;
            }
            Err(_) => {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(4));
            }
        }
    }
}

/// Stores a freshly opened connection in the session and tells the room.
pub async fn install(app: &AppHandle, o: Opened) {
    let state = app.state::<AppState>();
    let mut guard = state.session.lock().await;
    let Some(s) = guard.as_mut() else { return };
    s.room_state.code = o.code;
    s.room_state.host_key = Some(o.host_key);
    s.room_state.open = true;
    s.gate = Some(o.gate);
    s.broadcast_room_state();
    let _ = app.emit("room", s.room_view());
}

/// Host: turn the code on/off, change the room size, or swap to a new code.
pub async fn configure(app: AppHandle, open_code: Option<bool>, max_peers: Option<u8>, new_code: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    // apply what needs no network first
    let (server, code, key, max, members, want_open, reconnect, locked) = {
        let mut guard = state.session.lock().await;
        let s = guard.as_mut().ok_or("방에 있지 않아요")?;
        if !s.is_host {
            return Err("방장만 바꿀 수 있어요".into());
        }
        if let Some(n) = max_peers {
            // never below the people already here
            s.room_state.max_peers = n.clamp(s.member_count().max(2), 8);
            if let Some(g) = &s.gate {
                g.send(ToWorker::Capacity { max_peers: s.room_state.max_peers, members: s.member_count() });
            }
        }
        let want_open = open_code.unwrap_or(s.room_state.open);
        let close_now = (!want_open || new_code) && s.gate.is_some();
        if close_now {
            if let Some(g) = s.gate.take() {
                g.send(ToWorker::Close); // forget the old code right away
            }
        }
        s.room_state.open = want_open && s.gate.is_some() && !new_code;
        if new_code {
            s.room_state.host_key = None;
        }
        let reconnect = want_open && (s.gate.is_none());
        s.broadcast_room_state();
        let _ = app.emit("room", s.room_view());
        let code = (!new_code).then(|| s.room_state.code.clone());
        let locked = s.room_state.password.is_some();
        (s.server.clone(), code, s.room_state.host_key.clone(), s.room_state.max_peers, s.member_count(), want_open, reconnect, locked)
    };
    if !want_open || !reconnect {
        return Ok(());
    }
    match open(&app, &server, code, key, max, members, locked).await {
        Ok(o) => {
            install(&app, o).await;
            Ok(())
        }
        Err((ErrorKind::CodeTaken, _)) => {
            // our old code is in use elsewhere now: open with a new one
            let o = open(&app, &server, None, None, max, members, locked).await.map_err(|(_, e)| e)?;
            let new = o.code.clone();
            install(&app, o).await;
            notice(&app, format!("예전 코드는 다른 방이 쓰고 있어서 새 코드 {}(으)로 열었어요.", gachi_proto::code::display(&new)));
            Ok(())
        }
        Err((_, e)) => Err(e),
    }
}
