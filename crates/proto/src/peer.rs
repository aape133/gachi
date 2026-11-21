//! Room messages sent peer to peer (JSON inside encrypted `Control` packets), so a room keeps
//! working without the connection server: chat, profiles, status, metronome.
//!
//! The host is not elected by messages: it is always the member with the lowest id (the
//! earliest to join), so everyone agrees on their own.

use crate::signal::{AudioInfo, Profile};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PeerMsg {
    Chat {
        text: String,
    },
    Profile {
        profile: Profile,
    },
    Status {
        audio: AudioInfo,
    },
    /// From the host: the room metronome. Beat 0 is at `start_us` on the **host's** clock;
    /// each peer converts with its measured clock offset to the host.
    /// `by` = who asked for it (for "… turned the metronome on").
    /// `count_in`: bars counted in from `start_us` before bar 1 (versions that do not know
    /// it click plain beats there: the same beats, in step).
    Metronome {
        on: bool,
        bpm: f32,
        beats: u8,
        start_us: u64,
        by: crate::PeerId,
        #[serde(default)]
        count_in: u8,
    },
    /// To the host: please turn the metronome on/off or change it.
    MetronomeRequest {
        on: bool,
        bpm: f32,
        beats: u8,
        #[serde(default)]
        count_in: u8,
    },
    /// From the host: everything a member needs to take over as host (code, whether it is
    /// open, size, password, the key to reclaim the code on the connection server).
    RoomState(RoomState),
    /// From the host: a newcomer was admitted; connect to it.
    Introduce {
        peer: crate::signal::PeerInfo,
    },
    /// My addresses changed (e.g. the router opened a port for us).
    Candidates {
        addrs: Vec<std::net::SocketAddr>,
    },
    /// How your audio reaches me: percent of your packets that went missing on the way
    /// (before late copies filled gaps). The sender decides from it whether to send copies.
    LinkReport {
        raw_loss: f32,
    },
    /// From the host: this member is put out of the room. Everyone drops it (so it is out
    /// even if its own app ignores this), and it leaves.
    Kick {
        id: crate::PeerId,
    },
    /// From the host: this member is gone without having said so (it never got through to
    /// the host, and has just come back in as a newcomer from the same PC). Everyone drops
    /// it. Older versions do not know this message and drop it later on their own.
    Gone {
        id: crate::PeerId,
    },
}

/// The room as the host runs it. Shared only inside the (encrypted) room.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoomState {
    pub code: String,
    pub name: String,
    /// the code is turned on: newcomers can join
    pub open: bool,
    pub max_peers: u8,
    pub password: Option<String>,
    /// proves to the connection server that we may reopen / take over `code`
    pub host_key: Option<String>,
    /// the next id to hand out (ids are never reused within a room)
    pub next_id: crate::PeerId,
    /// public addresses of people put out of the room (IPv6: their /64): the host lets
    /// nobody in from there. Part of the room's state so that the next host keeps to it.
    #[serde(default)]
    pub banned: Vec<std::net::IpAddr>,
    /// How long the room has been open (ms), as the host sends it: every member shows the
    /// room's time, not its own since joining, and the next host goes on from it. An age
    /// rather than a time of day: the PCs' clocks need not agree. Older versions do not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
}

impl PeerMsg {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("PeerMsg serializes")
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        serde_json::from_slice(b).ok()
    }
}

/// Text typed by people (names, instruments, room names, chat) as it may be shown: without
/// control characters, and without the invisible ones that reorder or hide text (direction
/// overrides and isolates, zero-width space and joiners used alone to fake a name). What is
/// left is plain text; the UI escapes it before it goes into the page.
pub fn clean_text(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
        })
        .collect()
}

/// Chat text that fits one control packet (UTF-8 bytes, cut at a character boundary).
pub fn clip_chat(text: &str) -> String {
    let text = clean_text(text);
    let text = text.trim();
    let mut end = text.len().min(900);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::MAX_CONTROL_PAYLOAD;

    #[test]
    fn typed_text_loses_what_cannot_be_seen() {
        // a name made to read backwards, a line break, a zero-width space
        assert_eq!(clean_text("ab\u{202E}dc\n\u{200B}!"), "abdc!");
        // ordinary text, Korean and emoji stay as they are
        assert_eq!(clean_text("민준 🎸 <b>hi</b>"), "민준 🎸 <b>hi</b>");
        assert_eq!(clip_chat("  안녕\u{0007}하세요  "), "안녕하세요");
    }

    #[test]
    fn longest_chat_fits_one_packet() {
        let long = "가".repeat(1000);
        let msg = PeerMsg::Chat {
            text: clip_chat(&long),
        };
        assert!(msg.to_bytes().len() <= MAX_CONTROL_PAYLOAD);
        assert_eq!(PeerMsg::from_bytes(&msg.to_bytes()), Some(msg));
    }

    /// The count-in is new: a metronome message from a version without it still reads (no
    /// count-in), and ours reads there (the field is one it does not know and skips).
    #[test]
    fn a_metronome_message_without_a_count_in_still_reads() {
        let old = br#"{"t":"metronome","on":true,"bpm":120.0,"beats":4,"start_us":5,"by":1}"#;
        assert_eq!(
            PeerMsg::from_bytes(old),
            Some(PeerMsg::Metronome {
                on: true,
                bpm: 120.0,
                beats: 4,
                start_us: 5,
                by: 1,
                count_in: 0
            })
        );
        let ours = PeerMsg::MetronomeRequest {
            on: true,
            bpm: 90.0,
            beats: 3,
            count_in: 2,
        };
        assert_eq!(PeerMsg::from_bytes(&ours.to_bytes()), Some(ours));
    }
}
