//! One room session: UDP transport + audio engine + the room itself (members, host,
//! metronome, code), and the background tasks that keep the UI updated.
//!
//! The connection server is only used to get in: the host keeps a connection to it while
//! the code is on (see `gate`), newcomers only until they are admitted. Everything else
//! (chat, introductions, metronome, room settings) goes peer to peer.

use crate::settings::Settings;
use crate::view::{Breakdown, ConnView, PeerUi, PeerView, RoomView, SelfUi};
use crate::{gate, link, signal, AppState};
use gachi_net::crypto::{Cipher, PeerKey};
use gachi_engine::{db_to_gain, DriverCheck, Engine, PeerHandle, RecordingResult, SELF_TRACK};
use gachi_net::{Gathered, Net, NetConfig, NetEvent};
use gachi_proto::audio::SampleFormat;
use gachi_proto::peer::{clip_chat, PeerMsg, RoomState};
use gachi_proto::signal::{
    Admission, AudioInfo, ErrorKind, FromWorker, JoinRequest, Link, PeerInfo, Profile, RoomInfo, SealedJoin, SealedWelcome,
    ToWorker, Welcome,
};
use gachi_proto::PeerId;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager};

pub enum Intent {
    Create { name: String, max_peers: u8, password: Option<String> },
    Join { code: String, password: Option<String> },
}

/// `start` errors that the UI handles specially (it asks for the room password).
pub const ERR_PASSWORD_REQUIRED: &str = "password_required";
pub const ERR_WRONG_PASSWORD: &str = "wrong_password";

pub struct PeerEntry {
    pub info: PeerInfo,
    pub handle: Option<PeerHandle>,
    pub ui: PeerUi,
    /// the peer's address once punching got through
    pub route: Option<SocketAddr>,
    /// when they were let into the room (as we heard of it)
    pub added: Instant,
    /// went silent (no goodbye); cleared when we hear from them again
    pub lost: bool,
    /// when it went silent
    pub lost_since: Option<Instant>,
    /// our own internet was down when it went silent
    pub lost_offline: bool,
    /// automatic jitter target in ms, held: rises at once, comes down slowly (see
    /// `hold_auto`). Kept in time, not packets: the peer's packet size can differ from ours
    /// and change while it plays.
    pub auto_ms: f32,
    pub raised_at: Instant,
    pub lowered_at: Instant,
    /// the stream has been measured since it (re)started: the target followed the
    /// measurement directly once
    pub settled: bool,
    /// `PeerNetStats::restarts` we have seen (a new one means: settle again)
    pub restarts_seen: u32,
    /// we send this peer every packet twice (it reported loss), since when
    pub copies_since: Option<Instant>,
    /// with copies on: since when its reports have been clean
    pub clean_since: Option<Instant>,
    /// the loss it reported when the copies were switched on (percent)
    pub copies_before: f32,
    /// reports in a row, with copies on, that were clearly worse than that
    pub copies_worse: u8,
    /// copies made it worse: not again before this
    pub copies_blocked: Option<Instant>,
    /// chat messages taken from it since when (see `CHAT_MAX`)
    pub chats: (Instant, u8),
}

/// Addresses taken from one peer (LAN, other adapters, public, guessed ports, IPv6, the
/// port the router opened): we knock on each.
const MAX_CANDIDATES: usize = 12;

/// Chat messages taken from one member in `CHAT_WINDOW`: more than anyone types, and an
/// end to a member's app filling everybody's chat (and screen) as fast as it can send.
const CHAT_MAX: u8 = 20;
const CHAT_WINDOW: Duration = Duration::from_secs(10);

/// The code is turned off this long after it was opened: a room left on overnight does not
/// keep its code (and its place on the connection server) for ever. The room itself goes
/// on, and the host can turn the code on again.
const CODE_OPEN_FOR: Duration = Duration::from_secs(12 * 3600);

/// A room's name as shown (and passed on): plain text, of a sensible length.
const ROOM_NAME_MAX: usize = 40;
fn clip_room_name(name: &str) -> String {
    let name: String = gachi_proto::peer::clean_text(name).trim().chars().take(ROOM_NAME_MAX).collect();
    if name.is_empty() { "새 합주실".into() } else { name }
}

/// The room as the host says it is, within what a room can be. The host is a member like
/// any other (whoever has been in longest): its word decides, but a broken or hostile one
/// must not hand us a room of 255, a name that fills the screen, or a list without end
/// (we keep all of it, and run the room by it if we become host).
fn clean_room_state(mut state: RoomState, ours: &RoomState) -> RoomState {
    state.name = clip_room_name(&state.name);
    state.max_peers = state.max_peers.clamp(2, gachi_proto::signal::ROOM_MAX);
    state.code = gachi_proto::code::normalize(&state.code).unwrap_or_else(|| ours.code.clone());
    state.host_key = state.host_key.filter(|k| k.len() == 32 && k.bytes().all(|b| b.is_ascii_hexdigit()));
    state.password = state.password.filter(|p| p.len() <= 256);
    let over = state.banned.len().saturating_sub(MAX_BANNED);
    state.banned.drain(..over);
    state
}

/// The peer reports this much loss of our audio (percent): start sending it copies.
const COPIES_ON_LOSS: f32 = 0.3;
/// Copies stay on at least this long, and stop after this long of clean reports.
const COPIES_HOLD: Duration = Duration::from_secs(60);

/// Shown when the ASIO driver stops answering.
pub const DRIVER_HUNG: &str = "오디오 드라이버가 응답하지 않아요. 오디오 장치를 다시 연결하거나 앱을 다시 시작해 주세요.";
/// A report under this (percent) counts as clean.
const COPIES_CLEAN_LOSS: f32 = 0.05;
/// With copies on, this many reports in a row (2 s each) clearly worse than before them:
/// the copies are the problem (the link is full, and twice the packets overfill it).
const COPIES_WORSE_REPORTS: u8 = 3;
/// Then no copies for this long.
const COPIES_BACKOFF: Duration = Duration::from_secs(300);

/// Loss (percent) with copies on that is clearly worse than the loss before them: more
/// than double, and at least 2 points more.
fn copies_hurt(before: f32, now: f32) -> bool {
    now >= before * 2.0 && now >= before + 2.0
}

/// New peers start with this jitter target (ms) until 2 s of their stream are measured;
/// then it goes straight to what the stream needs.
const START_MS: f32 = 10.7;
/// The automatic target comes down only after this long without needing more…
const HOLD_DOWN: Duration = Duration::from_secs(5);
/// …then halfway toward what is needed, every this often (a few steps to settle).
const STEP_DOWN: Duration = Duration::from_secs(1);
/// Room on top of the measured spread: `MARGIN_PACKETS` of the sender's packets, plus
/// `MARGIN_OUT` of our output buffer, plus `MARGIN_MS`. Fitted with the jitter simulation
/// (`crates/engine/examples/jitter_sim.rs`) over packets of 16/32/64 frames and ASIO
/// buffers of 32/64/128: every wired case then played 3 × 60 s with no dropout and
/// nothing concealed, network spikes included. A whole packet must arrive before it can
/// play, so bigger packets need more room; a bigger output buffer takes more at once.
const MARGIN_PACKETS: f32 = 1.6;
const MARGIN_OUT: f32 = 0.25;
const MARGIN_MS: f32 = 0.15;

/// A peer silent this long (after the net layer's 10 s "lost") is treated as gone: their
/// app probably crashed. Short enough that a new host still takes over the room code (the
/// connection server keeps it 30 s), long enough to ride out a Wi-Fi hiccup.
const GONE_AFTER: Duration = Duration::from_secs(10);

/// A member we never got through to in this long is taken off our list: let in, then their
/// app died before the first packet (they stayed in the room for good, since "went silent"
/// needs a connection to go silent on), or the two networks cannot reach each other (then
/// there was nothing to hear from them anyway). Well past the punch timeout (8 s) and the
/// addresses a router's port mapping adds a few seconds later.
const NEVER_CAME: Duration = Duration::from_secs(30);

/// The room metronome. `start_us` is beat 0 on `owner`'s clock (the host who set it);
/// `local_us` caches it converted to our clock, for when that host has left.
#[derive(Clone, Copy, Debug)]
pub struct Metro {
    bpm: f32,
    beats: u8,
    start_us: u64,
    owner: PeerId,
    local_us: Option<i64>,
}

pub struct Session {
    // field order = drop order: stop audio before the socket it sends on
    /// `None` while restarting, or if the device could not be reopened
    pub engine: Option<Engine>,
    pub net: Net,
    /// host only, while the code is on: our connection to the connection server
    pub gate: Option<gate::Gate>,
    /// everyone went silent because *we* lost the internet (see `is_cut_off`). The room goes
    /// on without us: we do not become its host, do not touch its code, and ask the UI to
    /// join again (`rejoin_asked`: once).
    pub cut_off: bool,
    rejoin_asked: bool,
    /// code, size, password, host key… (the host's word; shared in the room)
    pub room_state: RoomState,
    pub me: PeerId,
    /// host = the member with the lowest id (earliest to join); everyone works it out alike
    pub is_host: bool,
    pub started_at: u64,
    pub peers: BTreeMap<PeerId, PeerEntry>,
    /// connection server address (from the settings)
    pub server: String,
    my_profile: Profile,
    /// our addresses (they can grow, e.g. when the router maps a port)
    my_candidates: Vec<SocketAddr>,
    link: Link,
    /// in a room over Wi-Fi: Windows' background scan off and streaming mode on, until the
    /// session ends (dropping it puts both back)
    _quiet_wifi: Option<gachi_net::wifi::QuietWifi>,
    metronome: Option<Metro>,
    /// last tempo and beats, kept while the metronome is off (so turning it back on, or
    /// the screen after turning it off, keeps the user's tempo instead of 120)
    last_tempo: (f32, u8),
    /// click volume 0–100 (local setting)
    pub metronome_volume: f32,
    /// track names for recordings (id → name; [`SELF_TRACK`] is me)
    rec_names: Arc<Mutex<HashMap<PeerId, String>>>,
    sample_rate: u32,
    buffer_size: u32,
    packet_frames: u16,
    /// why we are not sending what the settings asked for (shown after a restart)
    fallback_note: Option<String>,
    global_jitter: Option<u32>,
    /// what a bad moment per 30 s is worth in delay (ms): the automatic jitter buffer's
    /// leaning, from the settings (see `gachi_net::buffer_level`)
    jitter_worth: f32,
    /// host: list the room for people on the same network (settings: showNearby)
    pub show_nearby: bool,
    /// the input buffer (ms) we last told the room
    announced_in_ms: f32,
    /// audio callback counts at the last health check (to notice a stalled device)
    cb_seen: (u32, u32),
    /// the ASIO driver stopped answering (told the user once)
    driver_hung: bool,
    /// when we last tried to reopen a broken device
    reopen_at: Option<Instant>,
    /// host: recent join attempts with a wrong password (limits guessing)
    bad_tries: Vec<Instant>,
    /// host: since when the code has been on (see `CODE_OPEN_FOR`)
    pub code_since: Option<Instant>,
    /// host: members just put out, still here for a moment so the message reaches them
    kicked: Vec<(PeerId, Instant)>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // the net layer says bye to everyone. A host with others still here leaves the code
        // to the server, which keeps it ~30 s for the next host; a host leaving an empty room
        // closes it, so it does not linger on the nearby list or take join requests
        if let Some(g) = self.gate.take() {
            if leaving_closes_code(self.is_host, self.peers.len()) {
                g.send(ToWorker::Close); // queued: the writer sends it before closing the socket
            }
        }
        self.tasks.iter().for_each(|t| t.abort());
    }
}

/// Leaving: close the room code at once (nobody is left to take it over), or leave it to
/// the server's grace period so the next host can.
fn leaving_closes_code(is_host: bool, others: usize) -> bool {
    is_host && others == 0
}

/// Awaits `fut`, but gives up within ~0.1 s once the join is cancelled.
async fn unless_cancelled<F: std::future::Future>(fut: F, cancelled: &impl Fn() -> bool) -> Result<F::Output, String> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return Ok(out),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if cancelled() {
                    return Err("cancelled".to_string());
                }
            }
        }
    }
}

/// Over Wi-Fi: keep Windows from scanning for other networks while we play (see
/// `gachi_net::wifi`).
fn quiet_wifi(link: Link) -> Option<gachi_net::wifi::QuietWifi> {
    if link != Link::Wifi {
        return None;
    }
    let q = gachi_net::wifi::QuietWifi::start();
    match &q {
        Some(q) => tracing::info!(cards = q.cards, "Wi-Fi: background scan off, streaming mode on while in the room"),
        None => tracing::info!("Wi-Fi: could not turn the background scan off"),
    }
    q
}

/// The arrival spread a wired link showed in our tests (ms): what the Wi-Fi tip compares with.
const WIRED_SPREAD_MS: f32 = 1.5;

/// Most the automatic jitter buffer grows to (ms). A bad Wi-Fi link needs ~25 ms.
const MAX_AUTO_MS: f32 = 40.0;
/// Most the jitter buffer holds as a target (packets; half its slots).
const MAX_TARGET_PACKETS: f32 = 128.0;

/// A member put out stays connected this long, so that the message reaches them.
const KICK_LINGER: Duration = Duration::from_millis(700);
/// Addresses kept on a room's list of people put out.
const MAX_BANNED: usize = 24;

/// What an address is banned as: itself (IPv4) or its /64 (IPv6: one home has a whole /64).
/// `None` for addresses that are not one person's on the internet (a LAN address like
/// 192.168.0.5 is used in every home: banning it would lock out everybody).
fn ban_key(ip: std::net::IpAddr) -> Option<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv6Addr};
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            let shared = v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || (a == 100 && (64..128).contains(&b));
            (!shared).then_some(ip)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            ((s[0] & 0xe000) == 0x2000).then(|| IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0)))
        }
    }
}

/// Did everyone go silent because *our* connection dropped? `silent`: for each silent peer,
/// when it went silent and whether our own internet was down then. Yes when all of the
/// `peers` are silent and either our internet was seen down, or two or more went silent
/// together (within `TOGETHER`): people do not crash in step.
fn is_cut_off(peers: usize, silent: &[(Instant, bool)]) -> bool {
    const TOGETHER: Duration = Duration::from_secs(3);
    if peers == 0 || silent.len() < peers {
        return false;
    }
    let first = silent.iter().map(|s| s.0).min();
    let last = silent.iter().map(|s| s.0).max();
    let together = peers >= 2 && first.zip(last).is_some_and(|(a, b)| b - a < TOGETHER);
    together || silent.iter().any(|s| s.1)
}

/// The settings' leaning as what a bad moment per 30 s is worth in delay (ms).
fn jitter_worth(settings: &Settings) -> f32 {
    match settings.jitter_style.as_str() {
        "fast" => gachi_net::WORTH_FAST,
        "steady" => gachi_net::WORTH_STEADY,
        _ => gachi_net::WORTH_BALANCED,
    }
}

/// See `Session::auto_ms`: spread and jitter of the stream, the sender's packet length and
/// our output buffer, all in ms.
fn auto_target_ms(spread_ms: f32, jitter_ms: f32, packet_ms: f32, out_ms: f32) -> f32 {
    let by_spread = spread_ms + MARGIN_PACKETS * packet_ms + MARGIN_OUT * out_ms + MARGIN_MS;
    let by_jitter = jitter_ms * 2.5;
    let by_output = out_ms + packet_ms;
    let max = MAX_AUTO_MS.max(out_ms + 4.0 * packet_ms);
    by_spread.max(by_jitter).max(by_output).clamp(packet_ms, max)
}

/// A target in ms as packets of `packet_ms` (quarter packets, rounded up: never less).
fn to_packets(ms: f32, packet_ms: f32) -> f32 {
    ((ms / packet_ms * 4.0).ceil() / 4.0).clamp(1.0, MAX_TARGET_PACKETS)
}

/// Names and instruments from others: trimmed, at most 12 characters, never empty.
fn clip_profile(p: Profile) -> Profile {
    let clean = |t: &str| -> String { gachi_proto::peer::clean_text(t).trim().chars().take(12).collect() };
    let (name, instrument) = (clean(&p.name), clean(&p.instrument));
    let color = p.color.filter(|c| (*c as usize) < gachi_proto::signal::COLOR_HUES.len());
    Profile { name: if name.is_empty() { "손님".into() } else { name }, instrument, color }
}

/// A peer's key as the net layer takes it (with its AES support), once the room told us.
fn peer_key(info: &PeerInfo) -> Option<PeerKey> {
    info.key.map(|public| PeerKey { public, aes: info.aes })
}

fn sample_format(settings: &Settings) -> SampleFormat {
    if settings.quality == "pcm16" {
        SampleFormat::Pcm16
    } else {
        SampleFormat::Pcm24
    }
}

/// The device we got is not the one picked in the settings (it could not be opened, and the
/// system default was used instead).
pub fn device_note(settings: &Settings, info: &gachi_engine::StreamInfo) -> Option<String> {
    if settings.daw_mode() {
        return None; // no device of our own
    }
    let wanted = settings.device.trim();
    (!wanted.is_empty() && info.input_device != wanted && info.output_device != wanted).then(|| {
        format!("'{wanted}'를 열 수 없어서 '{}'를 대신 쓰고 있어요. 다른 프로그램이 장치를 쓰고 있지 않은지 확인해 보세요.", info.output_device)
    })
}

/// Opens the ASIO device. We send uncompressed PCM (24 or 16 bit): no codec, no codec delay.
fn start_engine(net: &Net, settings: &Settings) -> Result<Engine, String> {
    let sender = net.sender_with(sample_format(settings));
    Engine::start(settings.engine_config(), Box::new(sender)).map_err(|e| {
        tracing::warn!(device = %settings.device, daw = settings.daw_mode(), "audio device: {e}");
        open_error(settings, &e)
    })
}

/// What to tell the user when the ASIO device (or, in DAW mode, the bridge) does not open.
pub fn open_error(settings: &Settings, e: &gachi_engine::EngineError) -> String {
    if settings.daw_mode() {
        return format!("VST 연결을 열 수 없어요 ({e}). gachi가 두 개 실행 중이면 하나를 닫아 주세요.");
    }
    format!(
        "ASIO 장치를 열 수 없어요 ({e}). 다른 프로그램(DAW 등)이 이 장치를 쓰고 있다면 닫고 다시 시도해 주세요. \
         드라이버가 한 프로그램만 허용하는 장치일 수 있어요."
    )
}

/// A time on our monotonic clock (`gachi_net::now_us`) as wall-clock time.
fn mono_to_system(us: i64) -> SystemTime {
    // (a day at most either way: the time may come from a peer, and a wild one must not
    // overflow the clock arithmetic)
    const DAY_US: i64 = 86_400_000_000;
    let delta = us.saturating_sub(gachi_net::now_us() as i64).clamp(-DAY_US, DAY_US);
    let now = SystemTime::now();
    if delta >= 0 {
        now + Duration::from_micros(delta as u64)
    } else {
        now - Duration::from_micros((-delta) as u64)
    }
}

/// Beat 0 is this far in the future when someone turns the metronome on, so every peer
/// has the message before the first click.
const METRONOME_LEAD_US: u64 = 300_000;

/// Volume slider (0–100) → click amplitude (perceptual-ish curve, 100 = 0.8).
pub fn metro_gain(volume: f32) -> f32 {
    let v = (volume / 100.0).clamp(0.0, 1.0);
    v * v * 0.8
}

/// Settings that need the audio engine reopened when they change.
pub const ENGINE_KEYS: &[&str] =
    &["device", "inputChannel", "quality", "packetSize", "connection"];

impl Session {
    pub fn add_peer(&mut self, mut info: PeerInfo) {
        if info.id == self.me || self.peers.contains_key(&info.id) {
            return;
        }
        // members come by the host's word (the welcome, `Introduce`). No room is bigger than
        // this, whatever the host says: every member costs a mixer slot, and packets sent to
        // each of its addresses, which a hostile host could point anywhere
        if self.peers.len() + 1 >= gachi_proto::signal::ROOM_MAX as usize {
            tracing::warn!(id = info.id, "the room is full: member not added");
            return;
        }
        // (the same limits as for someone we let in ourselves)
        info.profile = clip_profile(info.profile);
        info.candidates.truncate(MAX_CANDIDATES);
        let handle = self.engine.as_mut().map(|e| {
            let (tx, handle) = e.add_peer(info.id);
            self.net.add_peer(info.id, tx);
            handle
        });
        if handle.is_none() {
            // no engine right now: still connect, audio gets attached on restart
            let (tx, _) = rtrb::RingBuffer::new(1);
            self.net.add_peer(info.id, tx);
        }
        if let (false, Some(key)) = (info.candidates.is_empty(), peer_key(&info)) {
            self.net.set_candidates(info.id, info.candidates.clone(), info.token, key);
        }
        self.rec_names.lock().unwrap().insert(info.id, info.profile.name.clone());
        let id = info.id;
        let now = Instant::now();
        self.peers.insert(
            id,
            PeerEntry {
                info,
                handle,
                ui: PeerUi::default(),
                route: None,
                added: now,
                lost: false,
                lost_since: None,
                lost_offline: false,
                auto_ms: START_MS,
                raised_at: now,
                lowered_at: now,
                settled: false,
                restarts_seen: 0,
                copies_since: None,
                copies_before: 0.0,
                copies_worse: 0,
                copies_blocked: None,
                clean_since: None,
                chats: (now, 0),
            },
        );
        self.apply_peer_rate(id);
        // introduce ourselves (queued until the link is up)
        self.send_to(id, &PeerMsg::Status { audio: self.my_audio() });
    }

    /// The host: the member with the lowest id (the earliest to join).
    pub fn host_id(&self) -> PeerId {
        self.peers.keys().copied().chain([self.me]).min().unwrap_or(self.me)
    }

    /// Recomputes who is host after someone joined or left; tells the UI if it changed.
    fn refresh_host(&mut self, app: &AppHandle) {
        let host = self.host_id();
        let was_host_id = self.peers.iter().find(|(_, p)| p.info.is_host).map(|(&id, _)| id).or(self.is_host.then_some(self.me));
        for (&id, p) in self.peers.iter_mut() {
            p.info.is_host = id == host;
        }
        self.is_host = host == self.me;
        if was_host_id != Some(host) {
            let who = if self.is_host {
                "내가".to_string()
            } else {
                self.peers.get(&host).map(|p| format!("{}님이", p.info.profile.name)).unwrap_or_default()
            };
            notice(app, format!("이제 {who} 방장이에요."));
            // keep the code reachable: the new host takes it over with the room's key
            if self.is_host && self.room_state.open && self.gate.is_none() {
                tauri::async_runtime::spawn(gate::take_over(app.clone()));
            }
            // a new host keeps the metronome going on its own clock
            if self.is_host {
                if let Some(m) = self.metronome {
                    if let Some(local) = self.metro_local_us(&m) {
                        let m = Metro { start_us: local.max(0) as u64, owner: self.me, local_us: Some(local), ..m };
                        self.metronome = Some(m);
                        self.broadcast(&self.metro_msg(&m, true, self.me));
                    }
                }
            }
            let _ = app.emit("room", self.room_view());
        }
    }

    /// Sends a room message to one peer (reliable, encrypted, peer to peer).
    pub fn send_to(&self, id: PeerId, msg: &PeerMsg) {
        self.net.send_control(id, msg.to_bytes());
    }

    /// Sends a room message to everyone in the room.
    pub fn broadcast(&self, msg: &PeerMsg) {
        let bytes = msg.to_bytes();
        for &id in self.peers.keys() {
            self.net.send_control(id, bytes.clone());
        }
    }

    pub fn send_chat(&self, text: &str) {
        let text = clip_chat(text);
        if !text.is_empty() {
            self.broadcast(&PeerMsg::Chat { text });
        }
    }

    /// A peer left (goodbye) or the server said so.
    fn peer_gone(&mut self, app: &AppHandle, id: PeerId) {
        if let Some(p) = self.remove_peer(id) {
            let _ = app.emit("peer-left", json!({ "id": id.to_string(), "name": p.info.profile.name }));
            self.refresh_host(app);
            self.send_capacity();
            let _ = app.emit("room", self.room_view());
        }
    }

    /// An address someone was put out from (see `ban_key`)?
    fn is_banned(&self, ip: std::net::IpAddr) -> bool {
        ban_key(ip).is_some_and(|k| self.room_state.banned.contains(&k))
    }

    /// Host: puts a member out of the room and keeps them out. Everyone is told to drop
    /// them (so they are out even if their app ignores it); their public addresses go on the
    /// room's list, and nobody is let in from there again. They can still be seen leaving a
    /// moment later: the message has to reach them first.
    pub fn kick(&mut self, app: &AppHandle, id: PeerId) -> Result<(), String> {
        if !self.is_host {
            return Err("방장만 내보낼 수 있어요".into());
        }
        let Some(p) = self.peers.get(&id) else { return Err("이미 나간 사람이에요".into()) };
        let name = p.info.profile.name.clone();
        let addrs: Vec<std::net::IpAddr> = p.info.candidates.iter().map(|a| a.ip()).chain(p.route.map(|a| a.ip())).collect();
        for key in addrs.into_iter().filter_map(ban_key) {
            if !self.room_state.banned.contains(&key) {
                self.room_state.banned.push(key);
            }
        }
        // (the latest: a list that grew without end would not fit a room message)
        let over = self.room_state.banned.len().saturating_sub(MAX_BANNED);
        self.room_state.banned.drain(..over);
        self.broadcast(&PeerMsg::Kick { id });
        self.broadcast_room_state();
        if !self.kicked.iter().any(|(k, _)| *k == id) {
            self.kicked.push((id, Instant::now()));
            notice(app, format!("{name}님을 내보냈어요. 이 방에 다시 들어올 수 없어요."));
        }
        Ok(())
    }

    /// Peers silent for too long are treated as gone (their app probably crashed). Returns
    /// true if anyone was removed.
    pub fn drop_silent_peers(&mut self, app: &AppHandle) -> bool {
        // members put out: gone once the message has had time to reach them
        let due: Vec<PeerId> = self.kicked.iter().filter(|(_, t)| t.elapsed() > KICK_LINGER).map(|(id, _)| *id).collect();
        if !due.is_empty() {
            self.kicked.retain(|(id, _)| !due.contains(id));
            for id in due {
                self.peer_gone(app, id);
            }
            return true;
        }
        // it is we who dropped out: the others are still there, playing. Taking them for
        // gone made us "host" of an empty room, and as host we took the room's code away
        // from the real one. Instead: come back in through the code, as anyone would.
        if self.cut_off {
            let waited = self.peers.values().any(|p| p.lost_since.is_some_and(|t| t.elapsed() > GONE_AFTER));
            if waited && !self.rejoin_asked {
                self.rejoin_asked = true;
                tracing::warn!("still cut off: asking the UI to join the room again");
                let _ = app.emit("room-lost", json!({ "code": self.room_state.code, "password": self.room_state.password }));
            }
            return false;
        }
        let gone: Vec<(PeerId, String)> = self
            .peers
            .iter()
            .filter(|(_, p)| p.lost_since.is_some_and(|t| t.elapsed() > GONE_AFTER))
            .map(|(&id, p)| (id, p.info.profile.name.clone()))
            .collect();
        for (id, name) in &gone {
            notice(app, format!("{name}님이 오래 응답이 없어서 나간 것으로 처리했어요."));
            self.peer_gone(app, *id);
        }
        let never: Vec<(PeerId, String)> = self
            .peers
            .iter()
            .filter(|(_, p)| p.route.is_none() && p.added.elapsed() > NEVER_CAME)
            .map(|(&id, p)| (id, p.info.profile.name.clone()))
            .collect();
        for (id, name) in &never {
            tracing::warn!(id, "never connected: taken off the list");
            notice(app, format!("{name}님과 끝내 연결되지 않아서 목록에서 뺐어요."));
            self.peer_gone(app, *id);
        }
        !gone.is_empty() || !never.is_empty()
    }

    /// Host: the code has been on for `CODE_OPEN_FOR`: turn it off (as the switch in the room
    /// panel does) and say so. true if it was.
    pub fn close_old_code(&mut self, app: &AppHandle) -> bool {
        if !self.is_host || !self.room_state.open || !self.code_since.is_some_and(|t| t.elapsed() > CODE_OPEN_FOR) {
            return false;
        }
        self.code_since = None;
        if let Some(g) = self.gate.take() {
            g.send(ToWorker::Close); // forget the code right away
        }
        self.room_state.open = false;
        self.broadcast_room_state();
        let _ = app.emit("room", self.room_view());
        tracing::info!("the code was on for 12 hours: turned off");
        notice(app, "입장 코드를 켠 지 12시간이 지나서 껐어요. 합주는 그대로 계속돼요. 사람을 더 부르려면 방 설정에서 코드를 다시 켜 주세요.".into());
        true
    }

    /// People in the room, us included.
    pub fn member_count(&self) -> u8 {
        (self.peers.len() + 1).min(u8::MAX as usize) as u8
    }

    /// Host: tell the connection server how full we are (it turns newcomers away when full).
    fn send_capacity(&self) {
        if let Some(g) = &self.gate {
            g.send(ToWorker::Capacity { max_peers: self.room_state.max_peers, members: self.member_count() });
        }
    }

    /// Host: share the room state (members need it to take over as host).
    pub fn broadcast_room_state(&self) {
        if self.is_host {
            self.broadcast(&PeerMsg::RoomState(self.room_state.clone()));
            self.sync_lobby();
        }
    }

    /// Host with the code on: tells the connection server whether (and how) to show the
    /// room to people on the same network.
    pub fn sync_lobby(&self) {
        let (true, Some(gate)) = (self.is_host, &self.gate) else { return };
        let lobby = self.show_nearby.then(|| gachi_proto::signal::Lobby {
            name: self.room_state.name.clone(),
            host: self.my_profile.name.clone(),
            lan: lan_prefix(),
            gw: gachi_net::router_key(),
            locked: self.room_state.password.is_some(),
        });
        gate.send(gachi_proto::signal::ToWorker::Lobby { lobby });
    }

    /// What a newcomer needs to know about us.
    fn my_info(&self) -> PeerInfo {
        PeerInfo {
            id: self.me,
            profile: self.my_profile.clone(),
            is_host: self.is_host,
            candidates: self.my_candidates.clone(),
            token: self.net.token,
            key: Some(self.net.public_key),
            aes: gachi_net::crypto::aes_in_hardware(),
            audio: Some(self.my_audio()),
            seats: gachi_proto::signal::ROOM_MAX,
        }
    }

    /// The biggest this room can be made: what every member's version can take part in
    /// (versions up to 0.1.2 mix 8 others at most).
    pub fn room_cap(&self) -> u8 {
        self.peers.values().map(|p| gachi_proto::signal::seats_of(p.info.seats)).min().unwrap_or(gachi_proto::signal::ROOM_MAX).min(gachi_proto::signal::ROOM_MAX)
    }

    /// Host: a sealed join request (passed on by the connection server). Opens it with our
    /// join key and the room password's key (`pw`: `joinseal::password_key` of this room),
    /// admits the newcomer, and seals the answer for it alone.
    pub fn admit_sealed(&mut self, app: &AppHandle, sj: &SealedJoin, pw: Option<&[u8; 32]>) -> Admission {
        let reject = |kind, message: &str| Admission::Reject { kind, message: message.into() };
        if !self.is_host || !self.room_state.open {
            return reject(ErrorKind::NotFound, "방을 찾을 수 없어요.");
        }
        if self.member_count() >= self.room_state.max_peers {
            return reject(ErrorKind::Full, "방이 가득 찼어요.");
        }
        let locked = self.room_state.password.is_some();
        if locked && !sj.pw {
            return reject(ErrorKind::PasswordRequired, "비밀번호가 있는 방이에요.");
        }
        let now = Instant::now();
        self.bad_tries.retain(|t| now - *t < BAD_TRIES_WINDOW);
        if self.bad_tries.len() >= BAD_TRIES_MAX {
            return reject(ErrorKind::RateLimited, "비밀번호가 여러 번 틀렸어요. 1분 뒤에 다시 시도해 주세요.");
        }
        let (Some(eph), Some(sealed)) = (gachi_proto::key_from_hex(&sj.eph), gachi_proto::from_hex(&sj.sealed)) else {
            return reject(ErrorKind::BadRequest, "잘못된 요청이에요.");
        };
        let pw = if locked { pw } else { None };
        if locked && pw.is_none() {
            return reject(ErrorKind::Timeout, "방장이 아직 준비 중이에요. 다시 시도해 주세요.");
        }
        let Some((plain, answer_key)) = crate::gate::JOIN_KEY.open(&eph, pw, &sealed) else {
            if locked {
                self.bad_tries.push(now);
                tracing::info!(tries = self.bad_tries.len(), "join with a wrong password");
                return reject(ErrorKind::WrongPassword, "비밀번호가 맞지 않아요.");
            }
            return reject(ErrorKind::BadRequest, "잘못된 요청이에요.");
        };
        let Ok(join) = serde_json::from_slice::<JoinRequest>(&plain) else {
            return reject(ErrorKind::BadRequest, "잘못된 요청이에요.");
        };
        // a room opened for more than an older version can mix: it would hear only some
        if self.room_state.max_peers > gachi_proto::signal::seats_of(join.seats) {
            tracing::info!(room = self.room_state.max_peers, "join refused: the newcomer's version is too old for a room this big");
            return reject(ErrorKind::Full, "이 방은 8명보다 크게 열려 있어서 gachi 새 버전이 필요해요. 업데이트한 뒤 다시 들어와 주세요.");
        }
        if join.candidates.iter().any(|a| self.is_banned(a.ip())) {
            tracing::info!("join refused: put out of this room before");
            return reject(ErrorKind::NotFound, "이 방에는 들어갈 수 없어요.");
        }
        let welcome = self.admit(app, join);
        let sealed = answer_key.seal(&serde_json::to_vec(&welcome).expect("Welcome serializes"));
        Admission::Accept { welcome: SealedWelcome { sealed: gachi_proto::to_hex(&sealed) } }
    }

    /// Host: lets an (opened, checked) newcomer in: gives it an id and the member list, and
    /// introduces it to everyone.
    fn admit(&mut self, app: &AppHandle, join: JoinRequest) -> Welcome {
        let id = self.room_state.next_id.max(self.peers.keys().copied().chain([self.me]).max().unwrap_or(1) + 1);
        self.room_state.next_id = id + 1;
        let newcomer = PeerInfo {
            id,
            profile: clip_profile(join.profile),
            is_host: false,
            candidates: join.candidates.into_iter().take(MAX_CANDIDATES).collect(),
            token: join.token,
            key: Some(join.key),
            aes: join.aes,
            audio: None,
            seats: join.seats,
        };
        // everyone already here (us first), for the newcomer to connect to
        let mut peers = vec![self.my_info()];
        peers.extend(self.peers.values().map(|p| PeerInfo { is_host: false, ..p.info.clone() }));
        let welcome = Welcome {
            you: id,
            room: RoomInfo {
                code: self.room_state.code.clone(),
                name: self.room_state.name.clone(),
                max_peers: self.room_state.max_peers,
                has_password: self.room_state.password.is_some(),
            },
            peers,
        };
        // introduce the newcomer to the room, then add them ourselves
        self.broadcast(&PeerMsg::Introduce { peer: newcomer.clone() });
        self.add_peer(newcomer);
        self.broadcast_room_state(); // next_id moved; the newcomer gets the state too
        self.send_capacity();
        if let Some(v) = self.view_of(id) {
            let _ = app.emit("peer-joined", json!({ "peer": v, "silent": false }));
        }
        let _ = app.emit("room", self.room_view());
        welcome
    }

    fn on_peer_status(&mut self, app: &AppHandle, id: PeerId, mut audio: AudioInfo) {
        // only rates sound cards run at, and a delay that is a number (else: not known)
        if !(8_000..=384_000).contains(&audio.sample_rate) {
            audio.sample_rate = 0;
        }
        if !audio.input_ms.is_finite() || !(0.0..=1000.0).contains(&audio.input_ms) {
            audio.input_ms = 0.0;
        }
        if let Some(p) = self.peers.get_mut(&id) {
            let changed = p.info.audio.map(|a| a.sample_rate) != Some(audio.sample_rate);
            if changed && audio.sample_rate != 0 && audio.sample_rate != self.sample_rate {
                notice(
                    app,
                    format!(
                        "{}님은 {} Hz, 나는 {} Hz라서 변환해서 들려줘요. 같은 값으로 맞추면 음질이 가장 좋아요.",
                        p.info.profile.name, audio.sample_rate, self.sample_rate
                    ),
                );
            }
            p.info.audio = Some(audio);
        }
        self.apply_peer_rate(id);
        let _ = app.emit("stats", json!({ "peers": self.views(), "localMs": self.local_ms() }));
    }

    fn on_peer_profile(&mut self, app: &AppHandle, id: PeerId, profile: Profile) {
        // the same limits as when it joined (a peer's later changes are not trusted more)
        let profile = clip_profile(profile);
        if let Some(p) = self.peers.get_mut(&id) {
            p.info.profile = profile.clone();
            self.rec_names.lock().unwrap().insert(id, profile.name);
        }
        let _ = app.emit("room", self.room_view());
    }

    /// A room message from a peer (see `gachi_proto::peer`).
    fn on_peer_msg(&mut self, app: &AppHandle, from: PeerId, msg: PeerMsg) {
        match msg {
            PeerMsg::Chat { text } => {
                let Some(p) = self.peers.get_mut(&from) else { return };
                if p.chats.0.elapsed() > CHAT_WINDOW {
                    p.chats = (Instant::now(), 0);
                }
                p.chats.1 = p.chats.1.saturating_add(1);
                if p.chats.1 > CHAT_MAX {
                    return;
                }
                let _ = app.emit("chat", json!({ "from": from.to_string(), "text": clip_chat(&text) }));
            }
            PeerMsg::Profile { profile } => self.on_peer_profile(app, from, profile),
            PeerMsg::Status { audio } => self.on_peer_status(app, from, audio),
            PeerMsg::RoomState(state) => {
                // only the host's word counts
                if from == self.host_id() {
                    let was_open = self.room_state.open;
                    self.room_state = clean_room_state(state, &self.room_state);
                    if was_open != self.room_state.open {
                        notice(
                            app,
                            if self.room_state.open {
                                "방장이 입장 코드를 켰어요. 코드를 아는 사람은 들어올 수 있어요.".into()
                            } else {
                                "방장이 입장 코드를 껐어요. 이제 새로운 사람은 들어올 수 없어요.".into()
                            },
                        );
                    }
                    let _ = app.emit("room", self.room_view());
                }
            }
            PeerMsg::Introduce { peer } => {
                if from == self.host_id() && peer.id != self.me {
                    let id = peer.id;
                    self.add_peer(peer);
                    if let Some(v) = self.view_of(id) {
                        let _ = app.emit("peer-joined", json!({ "peer": v, "silent": false }));
                    }
                    let _ = app.emit("room", self.room_view());
                }
            }
            PeerMsg::LinkReport { raw_loss } => self.on_link_report(from, raw_loss),
            PeerMsg::Kick { id } => {
                // only the host puts people out
                if from != self.host_id() {
                    return;
                }
                if id == self.me {
                    tracing::info!("put out of the room by the host");
                    let _ = app.emit("kicked", json!({}));
                } else if let Some(p) = self.peers.get(&id) {
                    notice(app, format!("방장이 {}님을 내보냈어요.", p.info.profile.name));
                    self.peer_gone(app, id);
                }
            }
            PeerMsg::Candidates { mut addrs } => {
                // as many as when it joined: we send a few packets to each
                addrs.truncate(MAX_CANDIDATES);
                if let Some(p) = self.peers.get(&from) {
                    if let Some(key) = peer_key(&p.info) {
                        let token = p.info.token;
                        self.set_candidates(from, addrs, token, key);
                    }
                }
            }
            PeerMsg::MetronomeRequest { on, bpm, beats } => {
                if self.is_host {
                    self.set_room_metronome(app, on, bpm, beats, from);
                }
            }
            PeerMsg::Metronome { on, bpm, beats, start_us, by } => {
                // only the host runs the room metronome
                if from != self.host_id() {
                    return;
                }
                // the host's numbers, within what our own controls allow (a broken or hostile
                // host must not be able to stop our sound with a NaN tempo)
                let bpm = if bpm.is_finite() { bpm.clamp(30.0, 300.0) } else { 120.0 };
                let beats = beats.clamp(1, 12);
                self.last_tempo = (bpm, beats);
                self.metronome = on.then_some(Metro { bpm, beats, start_us, owner: from, local_us: None });
                self.apply_metronome();
                self.emit_metronome(app, on, by);
            }
        }
    }

    /// Turns the room metronome on/off (we are host): set it, apply it, tell everyone.
    fn set_room_metronome(&mut self, app: &AppHandle, on: bool, bpm: f32, beats: u8, by: PeerId) {
        let bpm = if bpm.is_finite() { bpm.clamp(30.0, 300.0) } else { 120.0 };
        let beats = beats.clamp(1, 12);
        let start = gachi_net::now_us() + METRONOME_LEAD_US;
        let m = Metro { bpm, beats, start_us: start, owner: self.me, local_us: Some(start as i64) };
        self.last_tempo = (bpm, beats);
        self.metronome = on.then_some(m);
        self.broadcast(&self.metro_msg(&m, on, by));
        self.apply_metronome();
        self.emit_metronome(app, on, by);
    }

    /// Our metronome as a message on our own clock (we must be host).
    fn metro_msg(&self, m: &Metro, on: bool, by: PeerId) -> PeerMsg {
        let start = self.metro_local_us(m).unwrap_or(m.start_us.min(i64::MAX as u64) as i64).max(0) as u64;
        PeerMsg::Metronome { on, bpm: m.bpm, beats: m.beats, start_us: start, by }
    }

    /// Someone (maybe us) wants the metronome changed: the host decides and broadcasts.
    pub fn request_metronome(&mut self, app: &AppHandle, on: bool, bpm: f32, beats: u8) {
        if self.is_host {
            self.set_room_metronome(app, on, bpm, beats, self.me);
        } else {
            self.send_to(self.host_id(), &PeerMsg::MetronomeRequest { on, bpm, beats });
        }
    }

    /// Beat 0 on our clock, if we can tell (needs a clock offset to the owner).
    fn metro_local_us(&self, m: &Metro) -> Option<i64> {
        if m.owner == self.me {
            return Some(m.start_us as i64);
        }
        match self.net.stats(m.owner).and_then(|s| s.clock_offset_us) {
            Some(offset) => Some((m.start_us.min(i64::MAX as u64) as i64).saturating_sub(offset)),
            None => m.local_us,
        }
    }

    fn emit_metronome(&self, app: &AppHandle, on: bool, by: PeerId) {
        let (bpm, beats, start_ms) = match self.metronome {
            Some(m) => {
                let local = self.metro_local_us(&m).unwrap_or(gachi_net::now_us() as i64);
                let ms = mono_to_system(local).duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                (m.bpm, m.beats, ms)
            }
            None => (self.last_tempo.0, self.last_tempo.1, 0),
        };
        let by_name = self.peers.get(&by).map(|p| p.info.profile.name.clone());
        let _ = app.emit(
            "metronome",
            json!({ "on": on, "bpm": bpm, "beats": beats, "startMs": start_ms, "byMe": by == self.me, "byName": by_name }),
        );
    }

    /// One code for the whole room from everyone's public key (see `room_safety_code`).
    /// `None` until every member's key is known.
    pub fn safety_code(&self) -> Option<String> {
        let mut members = vec![(self.me, self.net.public_key)];
        for (&id, p) in &self.peers {
            members.push((id, p.info.key?));
        }
        Some(gachi_net::crypto::room_safety_code(&members))
    }

    /// Resample a peer whose sample rate differs from ours (known from its status).
    pub fn apply_peer_rate(&self, id: PeerId) {
        let Some(p) = self.peers.get(&id) else { return };
        let Some(audio) = p.info.audio else { return };
        if audio.sample_rate == 0 {
            return;
        }
        self.net.set_peer_rate(id, audio.sample_rate);
        if let Some(h) = &p.handle {
            h.params.rate_ratio.store(audio.sample_rate as f32 / self.sample_rate as f32);
        }
    }

    pub fn set_candidates(&mut self, id: PeerId, addrs: Vec<SocketAddr>, token: u32, key: PeerKey) {
        if let Some(p) = self.peers.get_mut(&id) {
            p.info.candidates = addrs.clone();
            p.info.token = token;
            p.info.key = Some(key.public);
            p.info.aes = key.aes;
            self.net.set_candidates(id, addrs, token, key);
        }
    }

    pub fn remove_peer(&mut self, id: PeerId) -> Option<PeerEntry> {
        self.net.remove_peer(id);
        if let Some(e) = self.engine.as_mut() {
            e.remove_peer(id);
        }
        self.peers.remove(&id)
    }

    /// Drains transport events. Returns true if anything changed.
    pub fn poll_net_events(&mut self, app: &AppHandle) -> bool {
        let mut changed = false;
        while let Some(ev) = self.net.try_event() {
            changed = true;
            match ev {
                NetEvent::Connected { id, addr } => {
                    if let Some(p) = self.peers.get_mut(&id) {
                        p.route = Some(addr);
                    }
                    // someone put out before, back from an address it did not own up to
                    if self.is_host && self.is_banned(addr.ip()) {
                        tracing::info!(id, "connected from a banned address: put out again");
                        let _ = self.kick(app, id);
                    }
                    // the host brings a newcomer's metronome in step
                    if let (true, Some(m)) = (self.is_host, self.metronome) {
                        self.send_to(id, &self.metro_msg(&m, true, self.me));
                    }
                }
                NetEvent::PeerBack { id } => {
                    if let Some(p) = self.peers.get_mut(&id) {
                        if p.lost {
                            p.lost = false;
                            p.lost_since = None;
                            p.lost_offline = false;
                            notice(app, format!("{}님이 다시 연결됐어요.", p.info.profile.name));
                        }
                    }
                    // back in the room: a host whose code connection dropped meanwhile reopens it
                    if std::mem::take(&mut self.cut_off) && self.is_host && self.room_state.open && self.gate.is_none() {
                        tauri::async_runtime::spawn(gate::take_over(app.clone()));
                    }
                }
                NetEvent::Control { id, data } => match PeerMsg::from_bytes(&data) {
                    Some(msg) => self.on_peer_msg(app, id, msg),
                    None => tracing::warn!(id, "unreadable room message"),
                },
                NetEvent::PeerLeft { id } => self.peer_gone(app, id),
                NetEvent::PeerLost { id, online } => {
                    // keep them for a while: a network hiccup often heals (see `drop_silent_peers`)
                    if let Some(p) = self.peers.get_mut(&id) {
                        if !p.lost {
                            p.lost = true;
                            p.lost_since = Some(Instant::now());
                            p.lost_offline = !online;
                            if online {
                                notice(app, format!("{}님 연결이 끊겼어요. 다시 연결되기를 기다리는 중이에요.", p.info.profile.name));
                            }
                        }
                    }
                    let silent: Vec<(Instant, bool)> = self.peers.values().filter_map(|p| Some((p.lost_since?, p.lost_offline))).collect();
                    if !self.cut_off && is_cut_off(self.peers.len(), &silent) {
                        self.cut_off = true;
                        tracing::warn!("everyone went silent at once: our own connection dropped");
                        notice(app, "내 인터넷 연결이 끊겼어요. 다시 연결되기를 기다리는 중이에요.".into());
                    }
                }
                NetEvent::PunchFailed { id } => {
                    if let Some(p) = self.peers.get(&id) {
                        // our own router is the hard kind: say so, and what helps
                        let hard = matches!(self.net.mapping, gachi_net::stun::Mapping::Varies { .. });
                        notice(
                            app,
                            if hard {
                                format!(
                                    "{}님과 직접 연결이 안 돼요. 내 인터넷이 직접 연결이 어려운 유형(대칭형 NAT: 모바일 핫스팟, 회사망 등)이에요. 집 유선 인터넷으로 바꾸거나, 공유기의 UPnP 또는 IPv6를 켜면 연결될 수 있어요.",
                                    p.info.profile.name
                                )
                            } else {
                                format!(
                                    "{}님과 직접 연결이 안 돼요. 상대 인터넷이 직접 연결이 어려운 유형(모바일 핫스팟, 회사망 등)일 수 있어요. 상대가 집 유선 인터넷으로 바꾸거나, 두 사람 다 IPv6를 켜거나, 내 공유기의 UPnP를 켜면 연결될 수 있어요.",
                                    p.info.profile.name
                                )
                            },
                        );
                    }
                }
                NetEvent::Candidates(addrs) => {
                    // e.g. the router opened a port for us: let everyone try the new address
                    self.my_candidates = addrs.clone();
                    self.broadcast(&PeerMsg::Candidates { addrs });
                }
            }
        }
        changed
    }

    pub fn all_connected(&self) -> bool {
        self.peers.values().all(|p| p.route.is_some())
    }

    pub fn apply_self(&self, ui: &SelfUi) {
        let Some(e) = &self.engine else { return };
        let p = &e.self_params;
        p.input_gain.store(db_to_gain(ui.gain_db));
        p.muted.store(ui.muted, Ordering::Relaxed);
        p.monitor.store(ui.monitor, Ordering::Relaxed);
        p.monitor_gain.store(db_to_gain(ui.monitor_db));
        p.own_on.store(ui.own, Ordering::Relaxed);
        p.master.store(db_to_gain(ui.master_db));
    }

    pub fn apply_peer(p: &PeerEntry) {
        let Some(h) = &p.handle else { return };
        let params = &h.params;
        params.gain.store(db_to_gain(p.ui.volume_db));
        params.pan.store(p.ui.pan / 100.0);
        params.mute.store(p.ui.mute, Ordering::Relaxed);
        params.solo.store(p.ui.solo, Ordering::Relaxed);
    }

    /// What we tell the others about our setup.
    /// The audio device broke (unplugged, driver reset, callbacks stopped): reopen it with
    /// the current settings, at most every 5 s, and say so once it works again.
    fn check_audio(&mut self, app: &AppHandle) {
        let (broken, changed) = match &self.engine {
            Some(e) => match e.driver_check() {
                // the driver hangs: reopening would only wait on it too. Say so once and
                // keep the app working (leaving the room still works)
                DriverCheck::NoAnswer => {
                    if !self.driver_hung {
                        self.driver_hung = true;
                        notice(app, DRIVER_HUNG.into());
                    }
                    return;
                }
                check => {
                    self.driver_hung = false;
                    let changed = check == DriverCheck::Changed;
                    (changed || e.is_broken(&mut self.cb_seen), changed)
                }
            },
            // our last reopen failed (still unplugged?): keep trying
            None => (self.reopen_at.is_some(), false),
        };
        // a new rate/buffer from the driver panel: follow it right away
        if changed {
            self.reopen_at = None;
        }
        if !broken || self.reopen_at.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.reopen_at = Some(Instant::now());
        tracing::warn!("audio device stopped working; reopening");
        let state = app.state::<AppState>();
        let settings = state.settings.lock().unwrap().clone();
        let self_ui = state.self_ui.lock().unwrap().clone();
        self.cb_seen = (0, 0);
        match self.restart_engine(&settings, &self_ui) {
            Ok((summary, rec)) => {
                if rec.is_some() {
                    notice(app, "오디오 장치가 다시 연결되면서 녹음을 저장하고 멈췄어요.".into());
                }
                let why = if changed { "오디오 장치 설정이 바뀌어서" } else { "오디오 장치가 멈춰서" };
                notice(app, format!("{why} 다시 열었어요: {summary}"));
            }
            Err(e) => notice(app, format!("오디오 장치가 멈췄어요. 장치 연결을 확인해 주세요 ({e})")),
        }
    }

    /// New input channels without reopening the device.
    pub fn set_input_live(&self, input: gachi_engine::InputChannels) {
        if let Some(e) = &self.engine {
            e.set_input(input);
        }
    }

    /// Our own input + output latency (ms): what the ASIO driver reports (its buffer plus
    /// safety offset and USB transfer), else the callback sizes.
    pub fn local_ms(&self) -> f32 {
        let (i, o) = self.device_latency();
        (i + o) as f32 / self.sample_rate as f32 * 1000.0
    }

    /// (input, output) latency in frames: the driver's report, else the callbacks.
    fn device_latency(&self) -> (u32, u32) {
        self.engine.as_ref().map_or((self.buffer_size, self.buffer_size), |e| e.device_latency())
    }

    /// (input, output) frames per audio callback, as the device really runs.
    fn real_buffers(&self) -> (u32, u32) {
        self.engine.as_ref().map_or((self.buffer_size, self.buffer_size), |e| e.real_buffers())
    }

    pub fn my_audio(&self) -> AudioInfo {
        AudioInfo {
            // what our input really takes (the others count it in their totals)
            input_ms: self.device_latency().0 as f32 / self.sample_rate as f32 * 1000.0,
            sample_rate: self.sample_rate,
            link: self.link,
        }
    }

    /// Our audio setup, to the room.
    pub fn announce_status(&self) {
        self.broadcast(&PeerMsg::Status { audio: self.my_audio() });
    }

    /// New name/instrument, to the room.
    pub fn announce_profile(&mut self, profile: Profile) {
        self.my_profile = profile.clone();
        self.broadcast(&PeerMsg::Profile { profile });
    }

    /// Jitter mode/size: no restart needed.
    pub fn apply_jitter_settings(&mut self, settings: &Settings) {
        self.global_jitter = (settings.jitter_mode == "manual").then_some(settings.jitter_packets);
        self.jitter_worth = jitter_worth(settings);
        self.update_jitter_targets();
    }

    /// Reopens the audio device with new settings. The network side (routes, punching,
    /// stats) is untouched; each peer's audio is re-attached to the new mixer.
    /// A running recording is finished first (its result is returned with the summary).
    pub fn restart_engine(
        &mut self,
        settings: &Settings,
        self_ui: &SelfUi,
    ) -> Result<(String, Option<Result<RecordingResult, String>>), String> {
        let recording = self.engine.as_mut().and_then(|e| e.stop_recording());
        // close first: ASIO drivers can only be opened once
        drop(self.engine.take());
        for p in self.peers.values_mut() {
            p.handle = None;
        }
        let engine = start_engine(&self.net, settings)?;
        self.fallback_note = device_note(settings, &engine.info);
        self.sample_rate = engine.info.sample_rate;
        self.buffer_size = engine.info.buffer_size.unwrap_or(settings.buffer_size);
        self.packet_frames = settings.packet_size;
        let summary = format!(
            "{} · {} Hz · 버퍼 {} ({:.1} ms){}",
            engine.info.output_device,
            self.sample_rate,
            self.buffer_size,
            engine.local_latency_ms(),
            self.fallback_note.as_ref().map(|n| format!(". {n}")).unwrap_or_default()
        );
        self.engine = Some(engine);
        let engine = self.engine.as_mut().expect("just set");
        for (&id, p) in self.peers.iter_mut() {
            let (tx, handle) = engine.add_peer(id);
            self.net.set_peer_output(id, tx);
            p.handle = Some(handle);
            Self::apply_peer(p);
        }
        let ids: Vec<PeerId> = self.peers.keys().copied().collect();
        for id in ids {
            self.apply_peer_rate(id); // our rate may have changed
        }
        self.apply_self(self_ui);
        self.apply_jitter_settings(settings);
        self.apply_metronome();
        self.announce_status();
        Ok((summary, recording))
    }

    /// Pushes the room metronome into the engine (after a change, a clock-offset update or
    /// a restart). Waits quietly until we know our clock offset to the host.
    pub fn apply_metronome(&mut self) {
        let local = self.metronome.and_then(|m| self.metro_local_us(&m));
        if let (Some(m), Some(l)) = (self.metronome.as_mut(), local) {
            m.local_us = Some(l);
        }
        let Some(e) = &self.engine else { return };
        e.metronome.gain.store(metro_gain(self.metronome_volume));
        match (self.metronome, local) {
            (Some(m), Some(local)) => {
                e.metronome.beats_per_bar.store(m.beats as u32, Ordering::Relaxed);
                e.set_metronome(true, m.bpm, mono_to_system(local))
            }
            (Some(_), None) => {} // no clock offset yet: start clicking once we have one
            (None, _) => e.set_metronome(false, 120.0, SystemTime::now()),
        }
    }

    /// Starts per-track recording into Documents/gachi/녹음/<date time>.
    pub fn start_recording(&mut self, base: PathBuf, my_name: &str) -> Result<PathBuf, String> {
        let engine = self.engine.as_mut().ok_or("오디오 장치가 열려 있지 않아요")?;
        let dir = base.join(chrono::Local::now().format("%Y-%m-%d %H-%M-%S").to_string());
        self.rec_names.lock().unwrap().insert(SELF_TRACK, my_name.to_string());
        engine.start_recording(dir.clone(), self.rec_names.clone())?;
        Ok(dir)
    }

    pub fn stop_recording(&mut self) -> Option<Result<RecordingResult, String>> {
        self.engine.as_mut()?.stop_recording()
    }

    pub fn set_my_name(&self, name: &str) {
        self.rec_names.lock().unwrap().insert(SELF_TRACK, name.to_string());
    }

    /// Frames per packet we send: what the engine uses now (automatic follows the device's
    /// block), else the setting.
    pub fn my_packet_frames(&self) -> u32 {
        match self.engine.as_ref().map(|e| e.packet_frames()) {
            Some(n) if n > 0 => n,
            _ if self.packet_frames > 0 => self.packet_frames as u32,
            _ => gachi_engine::auto_packet_frames(self.buffer_size as usize) as u32,
        }
    }

    fn packet_ms(&self) -> f32 {
        self.my_packet_frames() as f32 / self.sample_rate as f32 * 1000.0
    }

    /// The length of this peer's packets (ms): what its jitter buffer counts in. Ours until
    /// its first packet tells.
    fn peer_packet_ms(&self, st: &gachi_net::PeerNetStats) -> f32 {
        if st.packet_ms > 0.0 { st.packet_ms } else { self.packet_ms() }
    }

    /// Jitter buffer size (ms) from what the stream shows: the spread its packets arrive
    /// with, plus a margin that grows with its packet size and our output buffer (see
    /// `MARGIN_PACKETS`). Never less than one output callback plus a packet, nor less than
    /// the averaged jitter suggests.
    fn auto_ms(&self, st: &gachi_net::PeerNetStats) -> f32 {
        let out_ms = self.real_buffers().1 as f32 / self.sample_rate as f32 * 1000.0;
        // the lateness worth covering, at this session's leaning (until 2 s are measured:
        // the plain worst case)
        let spread = gachi_net::buffer_level(st.seconds(), self.jitter_worth).unwrap_or(st.spread_ms);
        auto_target_ms(spread, st.jitter_ms, self.peer_packet_ms(st), out_ms)
    }

    /// The target in this peer's packets: manual per peer, else room-wide manual, else auto.
    fn packets_for(&self, p: &PeerEntry, packet_ms: f32) -> f32 {
        if !p.ui.jitter_auto {
            p.ui.jitter_packets as f32
        } else {
            self.global_jitter.map_or_else(|| to_packets(p.auto_ms, packet_ms), |g| g as f32)
        }
    }

    /// Pushes each peer's jitter target into the mixer (auto mode follows measured jitter).
    pub fn update_jitter_targets(&mut self) {
        let now = Instant::now();
        let ids: Vec<PeerId> = self.peers.keys().copied().collect();
        for id in ids {
            let st = self.net.stats(id).unwrap_or_default();
            let want = self.auto_ms(&st);
            let pm = self.peer_packet_ms(&st);
            let Some(p) = self.peers.get_mut(&id) else { continue };
            if st.restarts != p.restarts_seen {
                // they changed their audio (e.g. WASAPI → ASIO): measure afresh
                p.restarts_seen = st.restarts;
                p.settled = false;
            }
            if st.settled && !p.settled {
                // first 2 s of this stream measured: go straight to what it needs (the
                // safe start value would otherwise take a while to come down)
                p.settled = true;
                p.auto_ms = want;
                p.raised_at = now;
                p.lowered_at = now;
            } else if p.settled {
                hold_auto(p, want, now);
            } else if want > p.auto_ms {
                p.auto_ms = want; // still measuring: only ever more
            }
            let Some(p) = self.peers.get(&id) else { continue };
            let packets = self.packets_for(p, pm);
            if let Some(h) = &p.handle {
                // hundredths of a packet (the jitter buffer steers to fractions)
                h.params.jitter_target.store((packets * 100.0).round() as u32, Ordering::Relaxed);
            }
        }
    }

    /// Every ~2 s: tell each peer how much of its audio goes missing on the way to us.
    fn send_link_reports(&self) {
        for (&id, p) in &self.peers {
            if p.route.is_none() || p.lost {
                continue;
            }
            if let Some(st) = self.net.stats(id) {
                self.send_to(id, &PeerMsg::LinkReport { raw_loss: st.raw_loss_pct });
            }
        }
    }

    /// A peer says our audio gets lost on the way: send it every packet twice (a little
    /// later, so a short burst of loss does not take both). Off again after a clean minute.
    fn on_link_report(&mut self, from: PeerId, raw_loss: f32) {
        let now = Instant::now();
        let Some(p) = self.peers.get_mut(&from) else { return };
        if !raw_loss.is_finite() {
            return;
        }
        match p.copies_since {
            None if raw_loss >= COPIES_ON_LOSS && p.copies_blocked.is_none_or(|until| now >= until) => {
                p.copies_since = Some(now);
                p.clean_since = None;
                p.copies_before = raw_loss;
                p.copies_worse = 0;
                p.copies_blocked = None;
                tracing::info!(peer = from, raw_loss, "lossy link: sending every packet twice");
                self.net.set_redundancy(from, true);
            }
            Some(since) => {
                // loss that got much worse with the copies: a full link, which twice the
                // packets only fill further. Back to single packets, and stay there a while
                p.copies_worse = if copies_hurt(p.copies_before, raw_loss) { p.copies_worse + 1 } else { 0 };
                if p.copies_worse >= COPIES_WORSE_REPORTS {
                    p.copies_since = None;
                    p.clean_since = None;
                    p.copies_worse = 0;
                    p.copies_blocked = Some(now + COPIES_BACKOFF);
                    tracing::info!(peer = from, before = p.copies_before, raw_loss, "copies made the loss worse: single packets");
                    self.net.set_redundancy(from, false);
                } else if raw_loss >= COPIES_CLEAN_LOSS {
                    p.clean_since = None;
                } else {
                    let clean = *p.clean_since.get_or_insert(now);
                    if now - clean >= COPIES_HOLD && now - since >= COPIES_HOLD {
                        p.copies_since = None;
                        p.clean_since = None;
                        tracing::info!(peer = from, "link clean again: single packets");
                        self.net.set_redundancy(from, false);
                    }
                }
            }
            None => {}
        }
    }

    /// Every ~2 s in the log: our audio as the driver really runs it, and per peer what the
    /// latency display is made of (to check the numbers against reality).
    fn log_stats(&self) {
        if let Some(e) = &self.engine {
            let fmt = |r: Option<(u32, u32)>| r.map_or("-".to_string(), |(a, b)| format!("{a}..{b}"));
            tracing::info!(
                target: "gachi::stats",
                "self in_dev={:?} rate={} buf_reported={:?} buf_used={} in_cb_frames={} out_cb_frames={} peers={}",
                e.info.input_device,
                e.info.sample_rate,
                e.info.buffer_size,
                self.buffer_size,
                fmt(e.input_cb.range()),
                fmt(e.output_cb.range()),
                self.peers.len()
            );
            // how long our callbacks take against the time one buffer lasts, and how evenly
            // the driver calls them (µs, this 2 s window)
            let (li, lo) = (e.input_cb.load.take(), e.output_cb.load.take());
            let frames = e.output_cb.range().map_or(self.buffer_size, |(_, max)| max);
            let budget_us = frames as f64 * 1e6 / e.info.sample_rate.max(1) as f64;
            let show = |w: gachi_engine::LoadWindow| {
                format!("{:.0}/{:.0}/{:.0}/{:.0} gap {:.0}..{:.0}", w.p50_us, w.p99_us, w.p999_us, w.max_us, w.gap_min_us, w.gap_max_us)
            };
            tracing::info!(
                target: "gachi::stats",
                "self load_us(p50/p99/p99.9/max) in={} out={} budget={budget_us:.0} calls={}/{}",
                show(li),
                show(lo),
                li.count,
                lo.count
            );
        }
        for (&id, p) in &self.peers {
            let Some(v) = self.view_of(id) else { continue };
            let st = self.net.stats(id).unwrap_or_default();
            let jb = p.handle.as_ref().map(|h| {
                use std::sync::atomic::Ordering::Relaxed;
                (
                    h.stats.depth.load(Relaxed),
                    h.stats.target.load(Relaxed),
                    h.stats.underruns.load(Relaxed),
                    h.stats.concealed.load(Relaxed),
                    h.stats.late.load(Relaxed),
                    h.stats.drift_ppm.load(Relaxed),
                )
            });
            let b = &v.breakdown;
            tracing::info!(
                target: "gachi::stats",
                "peer id={id} route={} lost={} link={} their_in_ms={:?} their_rate={:?} rtt={:.2} jitter={:.2} spread={:.2} spread_p={:.2} loss={:.1}% raw_loss={:.1}% copies={} \
                 target={} (auto {}) jb(depth,target,underruns,concealed,late,drift_ppm)={:?} \
                 shown total={:.1} in={:.1} net={:.2} jit={:.1} out={:.1}",
                gachi_net::logaddr::opt(p.route),
                p.lost,
                v.conn.link,
                p.info.audio.map(|a| a.input_ms),
                p.info.audio.map(|a| a.sample_rate),
                st.rtt_ms.unwrap_or(-1.0),
                st.jitter_ms,
                st.spread_ms,
                st.spread_p_ms,
                st.loss_pct,
                st.raw_loss_pct,
                p.copies_since.is_some(),
                v.packets,
                v.auto_packets,
                jb,
                v.total,
                b.in_buf,
                b.net,
                b.jitter,
                b.out_buf
            );
        }
    }

    pub fn view_of(&self, id: PeerId) -> Option<PeerView> {
        let p = self.peers.get(&id)?;
        let st = self.net.stats(id).unwrap_or_default();
        let pm = self.peer_packet_ms(&st);
        let auto = to_packets(p.auto_ms, pm);
        let packets = self.packets_for(p, pm);
        let frame_ms = |n: u32| n as f32 / self.sample_rate as f32 * 1000.0;
        let (my_in, my_out) = self.device_latency();
        let breakdown = Breakdown {
            // their input once they've told us, else assume it matches ours. A packet leaves
            // when it is full: with packets longer than their callbacks, the first sound in
            // one waits the whole packet
            in_buf: p.info.audio.map(|a| a.input_ms).unwrap_or_else(|| frame_ms(my_in)).max(pm),
            net: st.rtt_ms.map(|r| r / 2.0).unwrap_or(0.0),
            jitter: packets * pm,
            out_buf: frame_ms(my_out),
        };
        // the buffer a wired link would need, for the Wi-Fi tip (wired links measured
        // ~1.5 ms of spread in our tests)
        let out_cb_ms = self.real_buffers().1 as f32 / self.sample_rate as f32 * 1000.0;
        let jitter_if_wired = to_packets(auto_target_ms(WIRED_SPREAD_MS, 0.0, pm, out_cb_ms), pm) * pm;
        Some(PeerView {
            id: id.to_string(),
            name: p.info.profile.name.clone(),
            instrument: p.info.profile.instrument.clone(),
            // the colour they picked (older versions: one made from the id)
            hue: p.info.profile.color.and_then(|c| gachi_proto::signal::COLOR_HUES.get(c as usize)).map_or((id as u32 * 67) % 360, |h| *h as u32),
            is_host: p.info.is_host,
            connected: p.route.is_some() && !p.lost,
            conn: ConnView {
                kind: "p2p",
                ipv6: p.route.is_some_and(|a| a.is_ipv6()),
                link: link::label(p.info.audio.map(|a| a.link).unwrap_or_default()),
                encrypted: p.info.key.is_some() && p.route.is_some(),
                cipher: Cipher::pick(gachi_net::crypto::aes_in_hardware(), p.info.aes).label(),
            },
            stream: crate::view::StreamView {
                bits: st.bits,
                channels: st.channels,
                sample_rate: p.info.audio.map_or(0, |a| a.sample_rate),
                packet_frames: st.packet_frames,
            },
            loss: st.loss_pct,
            outage_ms: st.last_outage.filter(|(at, _)| at.elapsed() < Duration::from_secs(30)).map_or(0.0, |(_, ms)| ms),
            jitter_ms: st.jitter_ms,
            packets,
            auto_packets: auto,
            jitter_auto: p.ui.jitter_auto,
            volume: p.ui.volume_db,
            pan: p.ui.pan,
            mute: p.ui.mute,
            solo: p.ui.solo,
            total: breakdown.total(),
            rtt: st.rtt_ms.unwrap_or(0.0),
            // the figures are estimates until the round trip and 2 s of the stream are measured
            measured: st.rtt_ms.is_some() && p.settled,
            jitter_if_wired,
            breakdown,
        })
    }

    pub fn views(&self) -> Vec<PeerView> {
        self.peers.keys().filter_map(|&id| self.view_of(id)).collect()
    }

    pub fn room_view(&self) -> RoomView {
        RoomView {
            code: self.room_state.code.clone(),
            name: self.room_state.name.clone(),
            max_peers: self.room_state.max_peers,
            max_cap: self.room_cap(),
            open: self.room_state.open,
            is_host: self.is_host,
            started_at: self.started_at,
            peers: self.views(),
            invite_url: crate::deeplink::invite_url(&self.server, &self.room_state.code),
            safety_code: self.safety_code(),
        }
    }

    pub fn levels(&self) -> serde_json::Value {
        let peers: serde_json::Map<String, serde_json::Value> = self
            .peers
            .iter()
            .map(|(id, p)| (id.to_string(), json!(p.handle.as_ref().map_or([0.0; 2], |h| h.meter.take()))))
            .collect();
        let (me, master, inputs) = match &self.engine {
            Some(e) => (e.self_meter.take(), e.master_meter.take(), e.input_meters.take(e.info.input_channels as usize)),
            None => ([0.0; 2], [0.0; 2], Vec::new()),
        };
        json!({ "self": me, "master": master, "peers": peers, "inputs": inputs })
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A system message in the chat panel.
pub fn notice(app: &AppHandle, text: String) {
    let _ = app.emit("notice", json!({ "text": text }));
}

/// Wrong-password joins the host takes per window before it refuses all for a while (the
/// connection server also limits attempts per address).
const BAD_TRIES_MAX: usize = 5;
const BAD_TRIES_WINDOW: Duration = Duration::from_secs(60);

fn stun_servers() -> Vec<String> {
    match std::env::var("GACHI_STUN") {
        Ok(v) => v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Err(_) => gachi_net::stun::DEFAULT_SERVERS.iter().map(|s| s.to_string()).collect(),
    }
}

/// Gets us into a room: gathers our addresses, then either opens a new code (create) or
/// asks the host through the connection server (join), then opens UDP + audio and starts
/// punching to everyone. Emits `connect-step` events for the UI.
pub async fn start(app: AppHandle, intent: Intent, gen: u64) -> Result<RoomView, String> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let self_ui = state.self_ui.lock().unwrap().clone();
    let step = |id: &str, st: &str, detail: String| {
        let _ = app.emit("connect-step", json!({ "id": id, "state": st, "detail": detail }));
    };
    let cancelled = || state.join_gen.load(Ordering::SeqCst) != gen;
    let cancel_err = || "cancelled".to_string();

    // from here until this returns the audio device is ours to open: said before the
    // preview is closed, so that whoever would open one after that sees it
    struct Entering<'a>(&'a std::sync::atomic::AtomicU32);
    impl Drop for Entering<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    state.starting.fetch_add(1, Ordering::SeqCst);
    let _entering = Entering(&state.starting);

    // one session at a time, and the settings screen's preview must release the device
    let old = state.session.lock().await.take();
    drop(old);
    drop(state.preview.lock().await.take());

    // 1. our addresses (STUN) and keys, then the connection server
    step("signal", "doing", String::new());
    let t = Instant::now();
    let endpoint = signal::Endpoint::parse(&settings.server);
    if endpoint.insecure_remote() {
        notice(&app, "연결 서버와의 연결이 암호화되지 않았어요(ws://). 입장 요청은 따로 봉인되지만, 누가 어떤 방에 들어가는지는 보일 수 있어요.".into());
    }
    let servers = stun_servers();
    let gathering = tauri::async_runtime::spawn_blocking(move || {
        let g = Gathered::gather(&servers)?;
        let link = g.public.map_or(Link::Unknown, link::detect);
        Ok::<_, std::io::Error>((g, link))
    });
    let (gathered, my_link) = unless_cancelled(gathering, &cancelled)
        .await?
        .map_err(|e| e.to_string())?
    .map_err(|e| format!("네트워크를 열 수 없어요 ({e})"))?;
    if cancelled() {
        return Err(cancel_err());
    }
    let profile = settings.profile();
    let candidates_n = gathered.candidates.len();
    let mut gathered = Some(gathered);

    let (me, room_state, peers, gate, media) = match &intent {
        Intent::Create { name, max_peers, password } => {
            let max = (*max_peers).clamp(2, gachi_proto::signal::ROOM_MAX);
            // the code request travels to a far server: open UDP and audio meanwhile
            let code_task = {
                let (app, server) = (app.clone(), settings.server.clone());
                let locked = password.is_some();
                tauri::async_runtime::spawn(async move { gate::open(&app, &server, None, None, max, 1, locked).await })
            };
            let media = open_media(&step, gathered.take().expect("gathered once"), 1, &settings, candidates_n, 0);
            let o = match unless_cancelled(code_task, &cancelled).await {
                Ok(done) => done.map_err(|e| e.to_string())?.map_err(|(_, e)| e)?,
                // the code may still open after this; nobody holds it, it is never listed
                // nearby, and the server forgets it after its grace period
                Err(e) => return Err(e),
            };
            let media = media?;
            let room_state = RoomState {
                banned: Vec::new(),
                code: o.code.clone(),
                name: clip_room_name(name),
                open: true,
                max_peers: max,
                password: password.clone(),
                host_key: Some(o.host_key.clone()),
                next_id: 2,
            };
            (1, room_state, Vec::new(), Some(o.gate), Some(media))
        }
        Intent::Join { code, password } => {
            let gathered = gathered.as_ref().expect("not taken on join");
            let url = endpoint.join_url(code);
            let (tx, mut rx) = unless_cancelled(signal::connect(&url), &cancelled).await?.map_err(|e| format!("연결 서버에 연결할 수 없어요 ({e})"))?;
            let refused = |kind, message| match kind {
                ErrorKind::PasswordRequired => ERR_PASSWORD_REQUIRED.to_string(),
                ErrorKind::WrongPassword => ERR_WRONG_PASSWORD.to_string(),
                _ => message,
            };
            // first the host's join key (and whether the room has a password)
            let (host_key, locked) = match unless_cancelled(tokio::time::timeout(Duration::from_secs(10), rx.recv()), &cancelled).await? {
                Ok(Some(FromWorker::Host { join_key, locked })) => {
                    let key = gachi_proto::key_from_hex(&join_key).ok_or("방장의 키가 올바르지 않아요")?;
                    (key, locked)
                }
                Ok(Some(FromWorker::Error { kind, message })) => return Err(refused(kind, message)),
                Ok(_) => return Err("연결 서버와의 연결이 끊겼어요".into()),
                Err(_) => return Err("연결 서버가 응답하지 않아요. 잠시 후 다시 시도해 주세요.".into()),
            };
            let password = match (locked, password) {
                (true, None) => return Err(ERR_PASSWORD_REQUIRED.into()),
                (true, Some(p)) => Some(p.clone()),
                (false, _) => None,
            };
            let join = JoinRequest {
                profile: profile.clone(),
                candidates: gathered.candidates.clone(),
                token: gathered.token,
                key: gathered.public_key,
                aes: gachi_net::crypto::aes_in_hardware(),
                seats: gachi_proto::signal::ROOM_MAX,
            };
            // seal it to the host (the password goes into the key, never on the wire)
            let code = code.clone();
            let (sealed, answer_key) = tauri::async_runtime::spawn_blocking(move || {
                let pw = password.map(|p| gachi_net::joinseal::password_key(&code, &p));
                let plain = serde_json::to_vec(&join).expect("JoinRequest serializes");
                let (eph, sealed, answer) = gachi_net::joinseal::seal(&host_key, pw.as_ref(), &plain)?;
                let sj = SealedJoin { eph: gachi_proto::to_hex(&eph), pw: pw.is_some(), sealed: gachi_proto::to_hex(&sealed) };
                Some((sj, answer))
            })
            .await
            .map_err(|e| e.to_string())?
            .ok_or("방장의 키가 올바르지 않아요")?;
            let _ = tx.send(ToWorker::Join { join: sealed });
            let welcome = match unless_cancelled(tokio::time::timeout(Duration::from_secs(15), rx.recv()), &cancelled).await? {
                Ok(Some(FromWorker::Welcome { welcome })) => welcome,
                Ok(Some(FromWorker::Error { kind, message })) => return Err(refused(kind, message)),
                Ok(_) => return Err("연결 서버와의 연결이 끊겼어요".into()),
                Err(_) => return Err("방장이 응답하지 않아요. 잠시 후 다시 시도해 주세요.".into()),
            };
            drop(tx); // done with the connection server
            // only the host (who knows the password, if there is one) could seal this
            let welcome: Welcome = gachi_proto::from_hex(&welcome.sealed)
                .and_then(|b| answer_key.open(&b))
                .and_then(|b| serde_json::from_slice(&b).ok())
                .ok_or("방장의 응답을 확인할 수 없어요 (보안 검증 실패). 다시 시도해 주세요.")?;
            let room_state = RoomState {
                banned: Vec::new(),
                code: welcome.room.code.clone(),
                name: clip_room_name(&welcome.room.name),
                open: true,
                max_peers: welcome.room.max_peers.clamp(2, gachi_proto::signal::ROOM_MAX),
                password: None,
                host_key: None,
                next_id: 0,
            };
            (welcome.you, room_state, welcome.peers, None, None)
        }
    };
    if cancelled() {
        return Err(cancel_err());
    }
    step("signal", "done", format!("{} ms", t.elapsed().as_millis()));

    // 2. UDP and audio (a new room opened them already, while waiting for its code)
    let (net, engine) = match media {
        Some(m) => m,
        None => open_media(&step, gathered.take().expect("gathered once"), me, &settings, candidates_n, peers.len())?,
    };
    if let Some(n) = device_note(&settings, &engine.info) {
        notice(&app, n);
    }
    let sample_rate = engine.info.sample_rate;
    let buffer_size = engine.info.buffer_size.unwrap_or(settings.buffer_size);

    let mut session = Session {
        engine: Some(engine),
        my_candidates: net.candidates.clone(),
        net,
        gate,
        cut_off: false,
        rejoin_asked: false,
        room_state,
        me,
        is_host: matches!(intent, Intent::Create { .. }),
        started_at: now_ms(),
        peers: BTreeMap::new(),
        server: settings.server.clone(),
        my_profile: profile,
        link: my_link,
        _quiet_wifi: quiet_wifi(my_link),
        metronome: None,
        last_tempo: (120.0, 4),
        metronome_volume: settings.metronome_volume,
        rec_names: Arc::new(Mutex::new(HashMap::from([(SELF_TRACK, settings.name.clone())]))),
        sample_rate,
        buffer_size,
        packet_frames: settings.packet_size,
        fallback_note: None,
        global_jitter: (settings.jitter_mode == "manual").then_some(settings.jitter_packets),
        jitter_worth: jitter_worth(&settings),
        show_nearby: settings.show_nearby,
        announced_in_ms: 0.0,
        cb_seen: (0, 0),
        driver_hung: false,
        reopen_at: None,
        bad_tries: Vec::new(),
        code_since: matches!(intent, Intent::Create { .. }).then(Instant::now),
        kicked: Vec::new(),
        tasks: Vec::new(),
    };
    session.sync_lobby();
    session.apply_self(&self_ui);
    for p in peers {
        session.add_peer(p);
    }
    // host = lowest id (the creator, or whoever has been here longest)
    session.is_host = session.host_id() == session.me;
    let host = session.host_id();
    for (&id, p) in session.peers.iter_mut() {
        p.info.is_host = id == host;
    }

    // 3. hole punching toward everyone already in the room
    step("punch", "doing", String::new());
    let deadline = Instant::now() + NetConfig::DEFAULT_PUNCH_TIMEOUT + Duration::from_millis(500);
    while !session.all_connected() && Instant::now() < deadline {
        session.poll_net_events(&app);
        if cancelled() {
            return Err(cancel_err());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let direct = session.peers.values().filter(|p| p.route.is_some()).count();
    let detail = if session.peers.is_empty() {
        "대기 중".to_string()
    } else {
        format!("{direct}/{}명 직결", session.peers.len())
    };
    step("punch", "done", detail);

    // 4. first RTT samples
    step("measure", "doing", String::new());
    if !session.peers.is_empty() {
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    let views = session.views();
    let avg = if views.is_empty() { 0.0 } else { views.iter().map(|v| v.total).sum::<f32>() / views.len() as f32 };
    step("measure", "done", if views.is_empty() { "-".into() } else { format!("평균 {avg:.1} ms") });
    if cancelled() {
        return Err(cancel_err());
    }

    let view = session.room_view();
    session.tasks.push(tauri::async_runtime::spawn(ticker(app.clone())));
    *state.session.lock().await = Some(session);
    Ok(view)
}

/// ~30 Hz meters, ~2 Hz stats (immediately when a peer connects).
async fn ticker(app: AppHandle) {
    let mut iv = tokio::time::interval(Duration::from_millis(33));
    // held up for a while (reopening a device takes a second, the PC slept): go on from now.
    // Making up for every tick missed sent the meters dozens of times in a row, and after a
    // sleep thousands of times
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut n: u32 = 0;
    loop {
        iv.tick().await;
        n = n.wrapping_add(1);
        let state = app.state::<AppState>();
        let mut guard = state.session.lock().await;
        let Some(s) = guard.as_mut() else { return };
        let changed = s.poll_net_events(&app) | s.drop_silent_peers(&app) | s.close_old_code(&app);
        if let Some(e) = s.engine.as_mut() {
            e.collect_garbage();
        }
        let _ = app.emit("levels", s.levels());
        // the figures in the log: every 2 s when the full log is on, else every 30 s (what
        // goes wrong is logged when it happens either way)
        let full = crate::settings::VERBOSE_LOG.load(Ordering::Relaxed);
        if n.is_multiple_of(if full { 60 } else { 900 }) {
            s.log_stats();
        }
        if n.is_multiple_of(60) {
            s.check_audio(&app);
            s.send_link_reports();
            // the device's real buffer is known once its callbacks ran: tell the room
            // (WASAPI usually runs ~10 ms whatever was asked)
            let now_ms = s.my_audio().input_ms;
            if (now_ms - s.announced_in_ms).abs() > 0.05 {
                s.announced_in_ms = now_ms;
                s.announce_status();
            }
        }
        if changed || n.is_multiple_of(15) {
            s.update_jitter_targets();
            // clock offsets to the host get better over the first seconds: follow them
            if s.metronome.is_some() {
                s.apply_metronome();
            }
            let _ = app.emit(
                "stats",
                json!({
                    "peers": s.views(),
                    "localMs": s.local_ms(),
                    "latency": s.engine.as_ref().map(|e| {
                        let (i, o, reported) = e.device_latency_ms();
                        json!({ "inMs": i, "outMs": o, "reported": reported })
                    }),
                    "bufferFrames": s.real_buffers().0.max(s.real_buffers().1),
                    "packetFrames": s.my_packet_frames(),
                    "sampleRate": s.sample_rate,
                    "plugin": s.engine.as_ref().and_then(|e| e.plugin_connected()),
                    "pluginWaiting": s.engine.as_ref().is_some_and(|e| e.plugin_waiting()),
                    "feedback": s.engine.as_ref().is_some_and(|e| e.feedback()),
                }),
            );
        }
    }
}

/// The held automatic jitter target: follows `want` up at once (the network got worse:
/// don't let it run dry), down only after `HOLD_DOWN` without needing more, halfway per
/// `STEP_DOWN`. Following every measurement window directly made the target (and the
/// pitch correction chasing it) swing.
fn hold_auto(p: &mut PeerEntry, want: f32, now: Instant) {
    if want >= p.auto_ms {
        p.auto_ms = want;
        p.raised_at = now;
    } else if now.duration_since(p.raised_at) >= HOLD_DOWN && now.duration_since(p.lowered_at) >= STEP_DOWN {
        // halfway down each step (21 → 4 ms in about 5 s once the hold is over); the last
        // quarter millisecond goes at once
        let next = want + (p.auto_ms - want) / 2.0;
        p.auto_ms = if next - want < 0.25 { want } else { next };
        p.lowered_at = now;
    }
}

/// Opens our UDP socket (as peer `me`) and the audio device.
fn open_media(
    step: &impl Fn(&str, &str, String),
    gathered: Gathered,
    me: PeerId,
    settings: &Settings,
    candidates_n: usize,
    members: usize,
) -> Result<(Net, Engine), String> {
    step("exchange", "doing", String::new());
    let net_cfg = NetConfig {
        my_id: me,
        sample_rate: settings.sample_rate,
        packet_frames: settings.packet_size,
        format: sample_format(settings),
        punch_timeout: NetConfig::DEFAULT_PUNCH_TIMEOUT,
        upnp: settings.upnp,
        peer_timeout: NetConfig::DEFAULT_PEER_TIMEOUT,
    };
    let net = Net::start(gathered, net_cfg).map_err(|e| format!("네트워크를 열 수 없어요 ({e})"))?;
    step("exchange", "done", format!("주소 {candidates_n}개 · 멤버 {members}명"));
    let engine = start_engine(&net, settings)?;
    Ok((net, engine))
}

/// Our private IPv4 network as "a.b.c" (the /24), for the nearby-rooms list.
pub fn lan_prefix() -> Option<String> {
    let ip = gachi_net::local_ip_toward("1.1.1.1:53".parse().ok()?)?;
    let std::net::IpAddr::V4(v4) = ip else { return None };
    let [a, b, c, _] = v4.octets();
    v4.is_private().then(|| format!("{a}.{b}.{c}"))
}

#[cfg(test)]
mod tests {

    #[test]
    fn only_addresses_of_ones_own_are_banned() {
        let key = |s: &str| super::ban_key(s.parse().unwrap()).map(|k| k.to_string());
        assert_eq!(key("203.0.113.7"), Some("203.0.113.7".into()));
        // every home has a 192.168.0.x, and a carrier's shared range is not one person's
        assert_eq!(key("192.168.0.5"), None);
        assert_eq!(key("10.1.2.3"), None);
        assert_eq!(key("100.72.1.9"), None);
        // IPv6: the whole /64 (a device changes the rest by itself)
        assert_eq!(key("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), Some("2001:db8:1:2::".into()));
        assert_eq!(key("fe80::1"), None);
    }

    #[test]
    fn losing_our_own_connection_is_not_everyone_leaving() {
        use std::time::{Duration, Instant};
        let t = Instant::now();
        let s = Duration::from_secs;
        // two others, silent in the same second: it is us (even if our internet looked fine)
        assert!(super::is_cut_off(2, &[(t, false), (t + s(1), false)]));
        // one of two silent: that one has a problem
        assert!(!super::is_cut_off(2, &[(t, false)]));
        // two who dropped out 20 s apart: they left one by one
        assert!(!super::is_cut_off(2, &[(t, false), (t + s(20), false)]));
        // one other, silent while our internet answered: they are gone
        assert!(!super::is_cut_off(1, &[(t, false)]));
        // one other, silent while nothing answered us: it is us
        assert!(super::is_cut_off(1, &[(t, true)]));
        assert!(!super::is_cut_off(0, &[]));
    }

    #[test]
    fn copies_that_fill_the_link_are_recognised() {
        // 0.5% before; 0.4% or 1.5% with copies is the same link, 6% is a full one
        assert!(!super::copies_hurt(0.5, 0.4));
        assert!(!super::copies_hurt(0.5, 1.5));
        assert!(super::copies_hurt(0.5, 6.0));
        // a link that was bad already: only clearly more than double
        assert!(!super::copies_hurt(4.0, 7.0));
        assert!(super::copies_hurt(4.0, 9.0));
    }

    #[test]
    fn only_a_host_left_alone_closes_the_code_on_leaving() {
        assert!(super::leaving_closes_code(true, 0), "empty room: gone at once");
        assert!(!super::leaving_closes_code(true, 2), "others stay: the next host takes the code over");
        assert!(!super::leaving_closes_code(false, 0), "not the host: not ours to close");
    }

    #[tokio::test]
    async fn a_cancelled_join_stops_waiting_at_once() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let cancelled = { let f = flag.clone(); move || f.load(Ordering::SeqCst) };
        // a wait like "the host has not answered yet" (15 s)
        let started = Instant::now();
        let cancel = { let f = flag.clone(); tokio::spawn(async move { tokio::time::sleep(Duration::from_millis(200)).await; f.store(true, Ordering::SeqCst) }) };
        let r = super::unless_cancelled(tokio::time::sleep(Duration::from_secs(15)), &cancelled).await;
        cancel.await.unwrap();
        assert_eq!(r, Err("cancelled".to_string()));
        assert!(started.elapsed() < Duration::from_millis(600), "gave up in {:?}", started.elapsed());
        // not cancelled: the result comes through
        let flag = AtomicBool::new(false);
        let r = super::unless_cancelled(async { 7 }, &|| flag.load(Ordering::SeqCst)).await;
        assert_eq!(r, Ok(7));
    }

    #[test]
    fn the_target_counts_in_the_senders_packets() {
        let ms = |pf: f32| pf / 48.0; // packet length at 48 kHz
        // the same network and output buffer: about the same time, whatever packets come
        let small = auto_target_ms(2.0, 0.1, ms(16.0), 1.33);
        let big = auto_target_ms(2.0, 0.1, ms(64.0), 1.33);
        assert!(big > small, "bigger packets need more room ({small} vs {big})");
        assert!((big - small - MARGIN_PACKETS * (ms(64.0) - ms(16.0))).abs() < 1e-4, "1.6 packets' worth more");
        // counted in each sender's own packets, it comes back to the same time (at most a
        // quarter packet more, never less)
        for (t, pf) in [(small, 16.0), (big, 64.0)] {
            let back = to_packets(t, ms(pf)) * ms(pf);
            assert!(back >= t && back - t < ms(pf) / 4.0 + 1e-4, "{t} ms in {pf}-frame packets → {back} ms");
        }
        // never less than one output callback plus a packet; a bad link may go to 40 ms
        assert!(auto_target_ms(0.0, 0.0, ms(32.0), 2.67) >= 2.67 + ms(32.0));
        assert_eq!(auto_target_ms(500.0, 0.0, ms(32.0), 1.33), MAX_AUTO_MS);
        // quarter packets, rounded up
        assert_eq!(to_packets(1.0, 0.5), 2.0);
        assert_eq!(to_packets(1.1, 0.5), 2.25);
        assert_eq!(to_packets(1000.0, 0.333), MAX_TARGET_PACKETS);
    }

    use super::*;

    #[test]
    fn the_hosts_room_state_is_kept_within_what_a_room_can_be() {
        let ours = RoomState { code: "K7MQ2X".into(), name: "합주".into(), open: true, max_peers: 4, password: None, host_key: None, next_id: 3, banned: vec![] };
        let wild = RoomState {
            code: "<script>".into(),
            name: format!("  {}\u{202E}\n", "가".repeat(500)),
            open: false,
            max_peers: 255,
            password: Some("p".repeat(5000)),
            host_key: Some("not a key".into()),
            next_id: 9,
            banned: (0..200u8).map(|n| std::net::IpAddr::from([203, 0, 113, n])).collect(),
        };
        let got = clean_room_state(wild, &ours);
        assert_eq!(got.code, "K7MQ2X", "a code that is none: ours stays");
        assert_eq!(got.name, "가".repeat(ROOM_NAME_MAX));
        assert_eq!(got.max_peers, gachi_proto::signal::ROOM_MAX);
        assert_eq!((got.password, got.host_key), (None, None));
        assert_eq!(got.banned.len(), MAX_BANNED);
        assert_eq!(got.banned.last(), Some(&std::net::IpAddr::from([203, 0, 113, 199])), "the latest are kept");
        assert!(!got.open);
        // an ordinary one passes as it is
        let fine = RoomState { code: "ABCDEF".into(), host_key: Some("0f".repeat(16)), password: Some("pw".into()), max_peers: 16, ..ours.clone() };
        assert_eq!(clean_room_state(fine.clone(), &ours), fine);
        assert_eq!(clip_room_name("   "), "새 합주실");
    }

    fn entry(now: Instant) -> PeerEntry {
        PeerEntry {
            info: PeerInfo {
                seats: 0,
                id: 2,
                profile: Profile { name: "B".into(), instrument: "기타".into(), color: None },
                is_host: false,
                candidates: vec![],
                token: 0,
                key: None,
                aes: false,
                audio: None,
            },
            handle: None,
            chats: (now, 0),
            ui: PeerUi::default(),
            route: None,
            added: Instant::now(),
            lost: false,
            lost_since: None,
            lost_offline: false,
            auto_ms: START_MS,
            raised_at: now,
            lowered_at: now,
            settled: true,
            restarts_seen: 0,
            copies_since: None,
            copies_before: 0.0,
            copies_worse: 0,
            copies_blocked: None,
            clean_since: None,
        }
    }

    #[test]
    fn auto_target_rises_at_once_and_comes_down_in_a_few_steps() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut p = entry(t0);
        hold_auto(&mut p, 32.0, at(1));
        assert_eq!(p.auto_ms, 32.0, "up at once");
        // the measurement wobbles below: nothing changes within the hold time
        for s in 2..6 {
            hold_auto(&mut p, 6.0, at(s));
        }
        assert_eq!(p.auto_ms, 32.0);
        // then halfway down each second, the last bit at once
        let mut seen = vec![];
        for s in 6..14 {
            hold_auto(&mut p, 6.0, at(s));
            seen.push(p.auto_ms);
        }
        assert_eq!(seen, vec![19.0, 12.5, 9.25, 7.625, 6.8125, 6.40625, 6.0, 6.0]);
        // needing more again restarts the hold
        hold_auto(&mut p, 10.0, at(14));
        hold_auto(&mut p, 6.0, at(17));
        assert_eq!(p.auto_ms, 10.0);
    }
}
