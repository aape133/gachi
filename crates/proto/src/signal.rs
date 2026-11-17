//! Messages on the connection server (Cloudflare Worker) WebSocket. JSON, tagged by `"t"`.
//!
//! The Worker never sees audio, chat or member lists. It only:
//! - keeps a room code reachable while the **host** holds it open (`/ws/host`), and
//! - passes a newcomer's join request to the host and the host's answer back
//!   (`/ws/join/{code}`). The newcomer disconnects as soon as it has its answer.
//!
//! The host checks the password, assigns the newcomer an id, sends it the member list, and
//! introduces it to everyone peer to peer (see `crate::peer`).
//!
//! The request and the answer are **sealed** between newcomer and host (see
//! `gachi_net::joinseal`): the server passes on only ciphertext. With a room password, the
//! password goes into the key itself and is never sent, so only someone who knows it can
//! read the request or forge the answer.

use crate::PeerId;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub instrument: String,
    /// which of the preset colours ([`COLOR_HUES`]) this person picked: everyone in the
    /// room shows them in it. `None` from older versions (shown in a colour made from the
    /// peer id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<u8>,
}

/// The preset colours people pick from, one for each of a full room, as hues: violet, blue,
/// sky, teal, green, lime, yellow, orange, red, pink, purple, indigo; then four soft ones
/// (brown, olive, slate, mauve), given as the hue plus [`COLOR_SOFT`]: sixteen plain hues
/// are too close together to tell apart, a soft colour is told from the plain one of its
/// hue at a glance. The UI gives each its lightness per theme, so they read on a dark and
/// on a light background alike. The first ten are as they always were (a version that
/// knows only those shows the others in a colour made from the peer id).
pub const COLOR_HUES: [u16; 16] = [255, 215, 190, 165, 135, 90, 48, 25, 355, 320, 288, 236, COLOR_SOFT + 28, COLOR_SOFT + 78, COLOR_SOFT + 212, COLOR_SOFT + 305];

/// Added to a hue in [`COLOR_HUES`]: the colour is a soft (less saturated) one.
pub const COLOR_SOFT: u16 = 1000;

/// The most people a room holds.
pub const ROOM_MAX: u8 = 16;

/// The room size the versions up to 0.1.2 can take part in (they mix 8 others at most, and
/// say nothing about it): what a missing `seats` means.
pub const ROOM_MAX_BEFORE: u8 = 8;

/// The biggest room a member can be in, from the `seats` it gave (0: an older version).
pub fn seats_of(seats: u8) -> u8 {
    if seats == 0 {
        ROOM_MAX_BEFORE
    } else {
        seats
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PeerInfo {
    pub id: PeerId,
    pub profile: Profile,
    #[serde(default)]
    pub is_host: bool,
    /// UDP addresses to try (LAN, public via STUN, IPv6, UPnP).
    pub candidates: Vec<SocketAddr>,
    /// Random value the peer puts in its punch packets so we only accept that peer.
    pub token: u32,
    /// The peer's X25519 public key for this session (audio is encrypted end to end).
    #[serde(default)]
    pub key: Option<[u8; 32]>,
    /// The peer's CPU has AES in hardware (audio to it uses AES-256-GCM if ours does too).
    #[serde(default)]
    pub aes: bool,
    /// The peer's audio setup, once it has reported it.
    #[serde(default)]
    pub audio: Option<AudioInfo>,
    /// The biggest room this peer's version can be in (see [`seats_of`]).
    #[serde(default)]
    pub seats: u8,
}

/// What each peer tells the others about its own setup, so latency breakdowns
/// show real numbers instead of guesses.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct AudioInfo {
    /// input buffer latency on that peer's machine (ms)
    pub input_ms: f32,
    pub sample_rate: u32,
    pub link: Link,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Wired,
    Wifi,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoomInfo {
    pub code: String,
    pub name: String,
    pub max_peers: u8,
    pub has_password: bool,
}

/// A newcomer's request. Travels sealed ([`SealedJoin`]); only the host reads it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JoinRequest {
    pub profile: Profile,
    pub candidates: Vec<SocketAddr>,
    pub token: u32,
    pub key: [u8; 32],
    /// AES in hardware (see [`PeerInfo::aes`])
    #[serde(default)]
    pub aes: bool,
    /// The biggest room the newcomer's version can be in (see [`seats_of`])
    #[serde(default)]
    pub seats: u8,
}

/// A [`JoinRequest`] sealed to the host's join key, as the server passes it on.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedJoin {
    /// the newcomer's one-time public key (hex)
    pub eph: String,
    /// the newcomer mixed a room password into the key
    pub pw: bool,
    /// the request, encrypted (hex)
    pub sealed: String,
}

/// The host's [`Welcome`], sealed for the one newcomer who asked (hex).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedWelcome {
    pub sealed: String,
}

/// The host's answer to an accepted newcomer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Welcome {
    /// the newcomer's id
    pub you: PeerId,
    pub room: RoomInfo,
    /// everyone already in the room, the host included
    pub peers: Vec<PeerInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Admission {
    Accept { welcome: SealedWelcome },
    Reject { kind: ErrorKind, message: String },
}

/// Host → Worker: how the room shows up for people on the same network (same public
/// address, same private /24). The Worker never lists it anywhere else.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Lobby {
    /// room name
    pub name: String,
    /// host's nickname
    pub host: String,
    /// private network prefix, e.g. "192.168.0" (rooms on other subnets are not shown)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan: Option<String>,
    /// what devices behind the host's router have in common (`gachi_net::router_key`, 32
    /// hex digits): the room is shown only to those who bring the same value. Without it
    /// the room is not listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gw: Option<String>,
    /// joining needs a password
    pub locked: bool,
}

/// Worker → app: a room open on the same network.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NearbyRoom {
    pub code: String,
    pub name: String,
    pub host: String,
    pub locked: bool,
    pub members: u8,
    pub max_peers: u8,
}

/// Client → Worker.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToWorker {
    /// Host (on `/ws/host`): open a new code (`code: None`) or reopen / take over one.
    /// Taking over a code that is held (or in its grace period) needs its `host_key`.
    /// `join_key`: the host's public key newcomers seal their request to (hex). `locked`:
    /// the room has a password (newcomers then ask for it before sending anything).
    HostOpen {
        code: Option<String>,
        host_key: Option<String>,
        max_peers: u8,
        members: u8,
        join_key: String,
        locked: bool,
    },
    /// Host: room size changed.
    Capacity { max_peers: u8, members: u8 },
    /// Host: the answer to a `JoinRequest`.
    Admit {
        req: u32,
        #[serde(flatten)]
        admission: Admission,
    },
    /// Host: turn the code off. The Worker forgets the room.
    Close,
    /// Host: show the room to people on the same network (`None`: stop showing it).
    Lobby { lobby: Option<Lobby> },
    /// Host: a member was put out. The code's `host_key` is shared with every member (so the
    /// next host can take the code over): give the code a new one, which the room is told
    /// without the member put out. Answered with [`FromWorker::Rekeyed`].
    Rekey,
    /// Newcomer (on `/ws/join/{code}`), after the server's [`FromWorker::Host`].
    Join {
        #[serde(flatten)]
        join: SealedJoin,
    },
}

/// Worker → client.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromWorker {
    /// To the host: the code is open. Keep `host_key` (share it in the room) to reopen or
    /// take over this code later.
    Opened { code: String, host_key: String },
    /// To the host, after [`ToWorker::Rekey`]: the code's new `host_key` (the old one no
    /// longer takes the code over).
    Rekeyed { host_key: String },
    /// To the host: someone wants in.
    JoinRequest {
        req: u32,
        #[serde(flatten)]
        join: SealedJoin,
    },
    /// To the newcomer, right after it connects: the host's join key (hex) and whether the
    /// room has a password.
    Host { join_key: String, locked: bool },
    /// To the newcomer: accepted.
    Welcome {
        #[serde(flatten)]
        welcome: SealedWelcome,
    },
    Error { kind: ErrorKind, message: String },
    /// On `/ws/nearby`: open rooms on the caller's network.
    Nearby { rooms: Vec<NearbyRoom> },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// no such code, or its code is turned off
    NotFound,
    Full,
    /// the room has a password and none was given
    PasswordRequired,
    WrongPassword,
    RateLimited,
    /// another room holds this code (when reopening)
    CodeTaken,
    /// the host did not answer in time
    Timeout,
    BadRequest,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shapes_match_the_worker() {
        let m = ToWorker::Capacity { max_peers: 8, members: 3 };
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"t":"capacity","max_peers":8,"members":3}"#);

        let reject = ToWorker::Admit {
            req: 7,
            admission: Admission::Reject { kind: ErrorKind::WrongPassword, message: "x".into() },
        };
        assert_eq!(
            serde_json::to_string(&reject).unwrap(),
            r#"{"t":"admit","req":7,"result":"reject","kind":"wrong_password","message":"x"}"#
        );

        let join = SealedJoin { eph: "ab".into(), pw: true, sealed: "cd".into() };
        let s = serde_json::to_string(&ToWorker::Join { join: join.clone() }).unwrap();
        assert_eq!(s, r#"{"t":"join","eph":"ab","pw":true,"sealed":"cd"}"#);
        let back: FromWorker = serde_json::from_str(&s.replacen(r#""t":"join""#, r#""t":"join_request","req":3"#, 1)).unwrap();
        assert_eq!(back, FromWorker::JoinRequest { req: 3, join });

        let accept = ToWorker::Admit { req: 1, admission: Admission::Accept { welcome: SealedWelcome { sealed: "ee".into() } } };
        assert_eq!(serde_json::to_string(&accept).unwrap(), r#"{"t":"admit","req":1,"result":"accept","welcome":{"sealed":"ee"}}"#);
        let w: FromWorker = serde_json::from_str(r#"{"t":"welcome","sealed":"ee"}"#).unwrap();
        assert_eq!(w, FromWorker::Welcome { welcome: SealedWelcome { sealed: "ee".into() } });
        let h: FromWorker = serde_json::from_str(r#"{"t":"host","join_key":"00","locked":true}"#).unwrap();
        assert_eq!(h, FromWorker::Host { join_key: "00".into(), locked: true });
    }

    /// A new host key on the server's terms: `{"t":"rekey"}` out, `rekeyed` back.
    #[test]
    fn a_new_host_key_is_asked_for_and_told() {
        assert_eq!(serde_json::to_string(&ToWorker::Rekey).unwrap(), r#"{"t":"rekey"}"#);
        let m: FromWorker = serde_json::from_str(r#"{"t":"rekeyed","host_key":"0123456789abcdef0123456789abcdef"}"#).unwrap();
        assert_eq!(m, FromWorker::Rekeyed { host_key: "0123456789abcdef0123456789abcdef".into() });
    }
}
