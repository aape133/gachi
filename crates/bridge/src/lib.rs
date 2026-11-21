//! The same-PC audio path between the gachi DAW plugin and the gachi app ("DAW mode").
//!
//! The DAW owns the audio interface; the plugin hands each block it processes to the app
//! (`up`: the player's sound, after the DAW's effects) and takes back what the app mixed
//! (`down`: the other players). Both run in one shared memory region:
//!
//! ```text
//! [Header: magic, version, owner, format, heartbeats, ring indices]
//! [up ring:   RING_FRAMES stereo f32 frames]   plugin → app
//! [down ring: RING_FRAMES stereo f32 frames]   app → plugin
//! ```
//!
//! The plugin side runs on the DAW's audio thread: it never blocks, never allocates, and
//! only copies and signals an event. The app side waits on that event, so it wakes as
//! soon as a block is in (a few microseconds on an idle system).
//!
//! One plugin instance at a time owns the bridge (`owner`); another one that finds it taken
//! gets [`Error::Busy`] and stays passive. A crashed owner's claim lapses once its heartbeat
//! is older than [`STALE_MS`].
//!
//! Windows only (the app is ASIO-only, so Windows-only as well).

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{pro_audio, AppEnd, PluginEnd};

/// Layout version: the app and the plugin must agree (checked on open).
/// 2: `waiting_ms` (another plugin instance is waiting for the bridge).
pub const VERSION: u32 = 2;
/// "GCHB"
pub const MAGIC: u32 = u32::from_le_bytes(*b"GCHB");
/// Interleaved stereo.
pub const CHANNELS: usize = 2;
/// Frames per ring (power of two): 8192 ≈ 170 ms at 48 kHz, far more than ever sits in it.
pub const RING_FRAMES: usize = 8192;
/// An owner whose heartbeat is older than this has gone (crashed, or the DAW stopped
/// processing it): another plugin instance may take over.
pub const STALE_MS: u64 = 1000;
/// The shared memory and event names (session-local), unless a test picks its own.
pub const DEFAULT_NAME: &str = "gachi-bridge-v1";

/// The bridge name both sides use: `DEFAULT_NAME`, or `GACHI_BRIDGE_NAME` when set (tests
/// and examples run beside a real gachi without touching its bridge).
pub fn name() -> String {
    std::env::var("GACHI_BRIDGE_NAME")
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| DEFAULT_NAME.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The app is not running (or not in DAW mode): no bridge to open.
    NotRunning,
    /// Another plugin instance owns the bridge.
    Busy,
    /// The other side was built with another layout version.
    Version { theirs: u32, ours: u32 },
    /// The region does not look like a gachi bridge.
    BadMagic,
    /// A system call failed (code from GetLastError).
    Os(u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotRunning => f.write_str(&gachi_lang::tr!("daw.gachi_app_not")),
            Error::Busy => f.write_str(&gachi_lang::tr!("daw.gachi_plugin_another")),
            Error::Version { theirs, ours } => {
                f.write_str(&gachi_lang::tr!("daw.gachi_app_plugin", theirs, ours))
            }
            Error::BadMagic => f.write_str(&gachi_lang::tr!("daw.gachi_link_area")),
            Error::Os(code) => f.write_str(&gachi_lang::tr!("daw.system_error_code", code)),
        }
    }
}

impl std::error::Error for Error {}

/// Single-producer single-consumer ring arithmetic on monotonic frame counters (they never
/// wrap in practice: 2^64 frames is millions of years). Kept apart from the shared memory so
/// it can be tested on its own.
pub(crate) mod ring {
    use super::RING_FRAMES;

    /// Frames the reader can take, or `None` when the counters cannot be right (the other
    /// side was replaced or wrote garbage): the reader then starts over at `write`.
    pub fn readable(write: u64, read: u64) -> Option<usize> {
        let n = write.checked_sub(read)?;
        (n <= RING_FRAMES as u64).then_some(n as usize)
    }

    /// Frames the writer may add without overtaking the reader (`None`: counters broken).
    pub fn writable(write: u64, read: u64) -> Option<usize> {
        readable(write, read).map(|n| RING_FRAMES - n)
    }

    /// Where frame number `at` lives in the ring, and how many frames fit before the end.
    pub fn slot(at: u64, frames: usize) -> (usize, usize) {
        let start = (at % RING_FRAMES as u64) as usize;
        (start, frames.min(RING_FRAMES - start))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn counters_that_make_no_sense_are_refused() {
            assert_eq!(readable(10, 4), Some(6));
            assert_eq!(readable(4, 10), None, "reader ahead of writer");
            assert_eq!(
                readable(RING_FRAMES as u64 + 1, 0),
                None,
                "more than the ring holds"
            );
            assert_eq!(writable(0, 0), Some(RING_FRAMES));
            assert_eq!(writable(RING_FRAMES as u64, 0), Some(0));
        }

        #[test]
        fn a_copy_splits_at_the_end_of_the_ring() {
            let end = RING_FRAMES as u64 - 3;
            assert_eq!(slot(end, 10), (RING_FRAMES - 3, 3));
            assert_eq!(slot(end + 3, 7), (0, 7));
        }
    }
}
