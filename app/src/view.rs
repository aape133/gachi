//! JSON shapes the web UI expects (see ui/backend.js).

use serde::{Deserialize, Serialize};

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Breakdown {
    pub in_buf: f32,
    pub net: f32,
    pub jitter: f32,
    pub out_buf: f32,
}

impl Breakdown {
    pub fn total(&self) -> f32 {
        self.in_buf + self.net + self.jitter + self.out_buf
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct ConnView {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub ipv6: bool,
    /// "wired" | "wifi" | "unknown"
    pub link: &'static str,
    /// audio to/from this peer is end-to-end encrypted
    pub encrypted: bool,
    /// with what: "AES-256-GCM" when both CPUs have AES in hardware, else "ChaCha20-Poly1305"
    pub cipher: &'static str,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub instrument: String,
    pub hue: u32,
    pub is_host: bool,
    pub connected: bool,
    pub conn: ConnView,
    pub loss: f32,
    /// the stream stopped for a moment within the last 30 s: how long (ms), else 0
    pub outage_ms: f32,
    pub jitter_ms: f32,
    /// jitter buffer in packets (fractions allowed)
    pub packets: f32,
    pub auto_packets: f32,
    pub jitter_auto: bool,
    pub volume: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub breakdown: Breakdown,
    pub total: f32,
    pub rtt: f32,
    /// round trip and the stream's timing measured: until then `total` is an estimate
    pub measured: bool,
    /// the jitter buffer this peer would need on a wired link (ms), for the Wi-Fi tip
    pub jitter_if_wired: f32,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RoomView {
    pub code: String,
    pub name: String,
    pub max_peers: u8,
    /// the code is on: people with it can join
    pub open: bool,
    pub is_host: bool,
    pub started_at: u64,
    pub peers: Vec<PeerView>,
    /// https link to share (opens the app through the server's invite page)
    pub invite_url: String,
    /// 12 digits from every member's key; the same on every screen unless a key was swapped
    pub safety_code: Option<String>,
}

/// Mixer state for one peer, in UI units.
#[derive(Clone, Debug)]
pub struct PeerUi {
    pub volume_db: f32,
    /// -100 ..= 100
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub jitter_auto: bool,
    pub jitter_packets: u32,
}

impl Default for PeerUi {
    fn default() -> Self {
        Self { volume_db: 0.0, pan: 0.0, mute: false, solo: false, jitter_auto: true, jitter_packets: 3 }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PeerPatch {
    pub volume: Option<f32>,
    pub pan: Option<f32>,
    pub mute: Option<bool>,
    pub solo: Option<bool>,
    pub jitter_auto: Option<bool>,
    pub jitter_packets: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct SelfUi {
    pub gain_db: f32,
    pub muted: bool,
    pub monitor: bool,
    pub master_db: f32,
    /// DAW mode: how loud the plugin passes your own sound through
    pub monitor_db: f32,
    /// DAW mode: the plugin passes your own sound through at all
    pub own: bool,
}

impl Default for SelfUi {
    fn default() -> Self {
        Self { gain_db: 0.0, muted: false, monitor: false, master_db: 0.0, monitor_db: 0.0, own: true }
    }
}

#[derive(Deserialize, Default, Debug)]
pub struct SelfPatch {
    pub gain: Option<f32>,
    pub muted: Option<bool>,
    pub monitor: Option<bool>,
    #[serde(rename = "monitorDb")]
    pub monitor_db: Option<f32>,
    pub own: Option<bool>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CreateOpts {
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_max")]
    pub max_peers: u8,
    #[serde(default)]
    pub password: String,
}

fn default_max() -> u8 {
    4
}
