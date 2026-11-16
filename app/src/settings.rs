use gachi_engine::{EngineConfig, HostChoice, InputChannels};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The deployed connection server (Cloudflare Worker, see `worker/`). For local testing
/// against `wrangler dev`, run the app with `GACHI_SERVER=127.0.0.1:8787`.
pub const DEFAULT_SERVER: &str = "gachi.aape.workers.dev";

/// Same shape as the UI's settings object (camelCase).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// ASIO driver name, "" = the first ASIO driver

    pub device: String,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub input_channel: String,
    pub quality: String,
    pub packet_size: u16,
    pub jitter_mode: String,
    pub jitter_packets: u32,
    pub name: String,
    pub instrument: String,
    /// my colour: an index into `gachi_proto::signal::COLOR_HUES` (picked at random on the
    /// first run, changed by clicking one's own avatar)
    #[serde(default)]
    pub color: Option<u8>,
    /// connection server: `host[:port]`, `https://…` or `ws://…` (see signal::Endpoint)
    pub server: String,
    /// metronome click volume, 0–100 (only you hear the click)
    #[serde(default = "default_metro_volume")]
    pub metronome_volume: f32,
    /// ask the router (UPnP) to forward our UDP port while in a room
    #[serde(default = "yes")]
    pub upnp: bool,
    /// "system", "light" or "dark"
    #[serde(default = "default_theme")]
    pub theme: String,
    /// show our open room to people on the same network (their home screen lists it)
    #[serde(default = "yes")]
    pub show_nearby: bool,
    /// "device": gachi opens the ASIO interface itself; "daw": the DAW owns it and the gachi
    /// plugin hands the sound over (DAW mode)
    #[serde(default = "default_connection")]
    pub connection: String,
    /// what the automatic jitter buffer favours: "fast" (less delay, more short concealed
    /// gaps on a link that stalls), "balanced", "steady" (the other way round)
    #[serde(default = "default_jitter_style")]
    pub jitter_style: String,
    /// the window draws with the graphics card (WebView2's GPU acceleration). Off if the
    /// screen flickers or breaks up; takes effect at the next start. Encryption's AES in
    /// hardware is the CPU's and unaffected.
    #[serde(default = "yes")]
    pub gpu: bool,
    /// the log file in full: connection figures every 2 s and the developer's detail
    /// (else every 30 s, and events only). For looking into a problem; off unless asked.
    #[serde(default)]
    pub verbose_log: bool,
    /// at startup, ask GitHub once whether a newer version is out (nothing is downloaded;
    /// the home screen only says so)
    #[serde(default = "yes")]
    pub check_updates: bool,
    /// recordings: "wav" (uncompressed) or "flac" (lossless, smaller)
    #[serde(default = "default_rec_format")]
    pub rec_format: String,
    /// WAV: 16, 24 or 32 (float); FLAC: 16 or 24
    #[serde(default = "default_rec_bits")]
    pub rec_bits: u32,
    /// also one file of everyone together, as heard here
    #[serde(default = "yes")]
    pub rec_mix: bool,
}

fn default_rec_format() -> String {
    "wav".into()
}

fn default_rec_bits() -> u32 {
    16
}

/// The log is written in full (see `Settings::verbose_log`; read by the log filter and by
/// the session's periodic figures).
pub static VERBOSE_LOG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn default_jitter_style() -> String {
    "balanced".into()
}

fn default_connection() -> String {
    "device".into()
}

fn default_theme() -> String {
    // (until the theme button is first used: whatever Windows is set to)
    "system".into()
}

fn yes() -> bool {
    true
}

fn default_metro_volume() -> f32 {
    60.0
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device: String::new(),
            sample_rate: 48_000,
            buffer_size: 64,
            input_channel: "1-2".into(),
            // 16 bit unless picked otherwise: two thirds of the upload, which is what most
            // connections are short of; 24 bit is for those who want it
            quality: "pcm16".into(),
            // automatic: as long as the device's block (see gachi_engine::auto_packet_frames)
            packet_size: 0,
            jitter_mode: "auto".into(),
            jitter_packets: 3,
            name: "나".into(),
            instrument: "기타".into(),
            color: None,
            // the connection server (Cloudflare Worker); GACHI_SERVER overrides it
            server: std::env::var("GACHI_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.into()),
            metronome_volume: default_metro_volume(),
            upnp: true,
            theme: default_theme(),
            show_nearby: true,
            connection: default_connection(),
            jitter_style: default_jitter_style(),
            gpu: true,
            verbose_log: false,
            check_updates: true,
            rec_format: default_rec_format(),
            rec_bits: default_rec_bits(),
            rec_mix: true,
        }
    }
}

/// Keys that are not saved: the connection server always comes from the build (or
/// GACHI_SERVER), so an update can move everyone to a new server.
const NOT_SAVED: &[&str] = &["server"];

impl Settings {
    /// Saved settings on top of the defaults (unknown or broken entries are ignored).
    pub fn load(path: &Path) -> Settings {
        let mut s = Settings::default();
        if let Ok(text) = std::fs::read_to_string(path) {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(mut v) => {
                    if let Some(obj) = v.as_object_mut() {
                        NOT_SAVED.iter().for_each(|k| {
                            obj.remove(*k);
                        });
                    }
                    if let Err(e) = s.apply_patch(&v) {
                        tracing::warn!("settings file ignored: {e}");
                    }
                }
                Err(e) => tracing::warn!("settings file unreadable: {e}"),
            }
        }
        // only uncompressed PCM is left (anything else, like an old "opus", becomes the default)
        if s.quality != "pcm16" && s.quality != "pcm24" {
            s.quality = Settings::default().quality;
        }
        if s.connection != "daw" {
            s.connection = default_connection();
        }
        // a colour of one's own from the first run on (kept: written back at once)
        let n = gachi_proto::signal::COLOR_HUES.len();
        if s.color.is_none_or(|c| c as usize >= n) {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
            s.color = Some((nanos as usize % n) as u8);
            let _ = s.save(path);
        }
        s
    }

    /// How we show to the room.
    pub fn profile(&self) -> gachi_proto::signal::Profile {
        gachi_proto::signal::Profile { name: self.name.clone(), instrument: self.instrument.clone(), color: self.color }
    }

    /// Writes the settings (atomically: a crash mid-write cannot leave a broken file).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut v = serde_json::to_value(self).map_err(std::io::Error::other)?;
        if let Some(obj) = v.as_object_mut() {
            NOT_SAVED.iter().for_each(|k| {
                obj.remove(*k);
            });
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&v).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// Merges known keys from a partial JSON object (what the UI sends).
    pub fn apply_patch(&mut self, patch: &serde_json::Value) -> Result<(), String> {
        let mut v = serde_json::to_value(&*self).map_err(|e| e.to_string())?;
        if let (Some(obj), Some(p)) = (v.as_object_mut(), patch.as_object()) {
            for (k, val) in p {
                // the connection server comes from the build only: the UI cannot point the
                // app at another one (nor could a script that got into the page)
                if obj.contains_key(k) && !NOT_SAVED.contains(&k.as_str()) {
                    obj.insert(k.clone(), val.clone());
                }
            }
        }
        *self = serde_json::from_value(v).map_err(|e| format!("잘못된 설정 값: {e}"))?;
        Ok(())
    }

    /// How recordings are made (see `gachi_engine::RecFormat`).
    pub fn rec_format(&self) -> gachi_engine::RecFormat {
        gachi_engine::RecFormat::from_settings(&self.rec_format, self.rec_bits)
    }

    /// DAW mode: the DAW owns the interface, the gachi plugin hands the sound over.
    pub fn daw_mode(&self) -> bool {
        self.connection == "daw"
    }

    pub fn engine_config(&self) -> EngineConfig {
        let device = (!self.device.is_empty()).then(|| self.device.clone());
        EngineConfig {
            host: if self.daw_mode() { HostChoice::Daw } else { HostChoice::Asio },
            input_device: device.clone(),
            output_device: device,
            sample_rate: self.sample_rate,
            buffer_size: self.buffer_size,
            packet_frames: self.packet_size,
            input: parse_input(&self.input_channel),
        }
    }
}

/// Input routing from the UI's value (1-based channel numbers):
/// `"3"` mono, `"3-4"` stereo pair, `"1+5-6"` channel 1 (mic, centered) plus pair 5/6.
pub fn parse_input(s: &str) -> InputChannels {
    let ch = |t: &str| t.trim().parse::<usize>().ok().filter(|&n| n >= 1).map(|n| n - 1);
    let pair = |t: &str| {
        let (a, b) = t.split_once('-')?;
        Some((ch(a)?, ch(b)?))
    };
    let parsed = match s.split_once('+') {
        Some((m, p)) => ch(m).zip(pair(p)).map(|(mono, pair)| InputChannels::Mix { mono, pair }),
        None if s.contains('-') => pair(s).map(|(a, b)| InputChannels::Stereo(a, b)),
        None => ch(s).map(InputChannels::Mono),
    };
    parsed.unwrap_or(InputChannels::Stereo(0, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_without_the_server() {
        let dir = std::env::temp_dir().join(format!("gachi-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        let s = Settings {
            name: "민지".into(),
            theme: "light".into(),
            server: "somewhere.else:1".into(),
            ..Settings::default()
        };
        s.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("somewhere.else"), "server is not saved");
        let back = Settings::load(&path);
        assert_eq!((back.name.as_str(), back.theme.as_str()), ("민지", "light"));
        assert_eq!(back.server, Settings::default().server);
        std::fs::write(&path, "{ broken").unwrap();
        assert_eq!(Settings::load(&path).name, Settings::default().name, "broken file → defaults");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_connection_mode_picks_the_engine_host() {
        let mut s = Settings::default();
        assert_eq!(s.engine_config().host, HostChoice::Asio);
        s.apply_patch(&serde_json::json!({ "connection": "daw" })).unwrap();
        assert!(s.daw_mode());
        assert_eq!(s.engine_config().host, HostChoice::Daw);
    }

    #[test]
    fn the_ui_cannot_change_the_server() {
        let mut s = Settings::default();
        s.apply_patch(&serde_json::json!({ "server": "evil.example:1", "name": "민지" })).unwrap();
        assert_eq!(s.server, Settings::default().server);
        assert_eq!(s.name, "민지", "other keys still apply");
    }

    #[test]
    fn parses_input_routing() {
        assert_eq!(parse_input("2"), InputChannels::Mono(1));
        assert_eq!(parse_input("1-2"), InputChannels::Stereo(0, 1));
        assert_eq!(parse_input("5-6"), InputChannels::Stereo(4, 5));
        assert_eq!(parse_input("1+5-6"), InputChannels::Mix { mono: 0, pair: (4, 5) });
        assert_eq!(parse_input("0"), InputChannels::Stereo(0, 1), "1-based");
        assert_eq!(parse_input("x"), InputChannels::Stereo(0, 1));
    }

    #[test]
    fn patch_merges_known_keys_only() {
        let mut s = Settings::default();
        s.apply_patch(&serde_json::json!({ "bufferSize": 32, "name": "지민", "bogus": 1 })).unwrap();
        assert_eq!((s.buffer_size, s.name.as_str()), (32, "지민"));
        assert!(s.apply_patch(&serde_json::json!({ "bufferSize": "big" })).is_err());
    }
}
