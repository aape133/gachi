//! gachi audio engine.
//!
//! Threads and data flow (no locks or allocations on the two audio callbacks):
//!
//! ```text
//!  input callback ──Frame──▶ PacketSink (encodes + sends UDP right away, no thread hop)
//!        │
//!        └──Frame──▶ monitor ring ─┐
//!                                  ▼
//!  network thread ──Frame──▶ per-peer ring ──▶ JitterBuffer ──▶ mixer ──▶ output callback
//! ```
//!
//! Control (add/remove peer) reaches the output callback through a command ring;
//! removed peers come back through a garbage ring so they are freed off the audio thread.

pub mod device;
pub mod engine;
pub mod extras;
pub mod frame;
mod host_thread;
pub mod jitter;
pub mod feedback;
pub mod limiter;
pub mod load;
pub mod meter;
pub mod params;

pub use device::{list_devices, open_asio_control_panel, DeviceInfo, HostChoice, ASIO_AVAILABLE};
pub use engine::{auto_packet_frames, DriverCheck, Engine, EngineConfig, EngineError, InputChannels, PacketSink, PeerHandle, StreamInfo};
pub use extras::{safe_name, MetronomeParams, RecordingResult, SELF_TRACK};
pub use frame::Frame;
pub use jitter::{JitterBuffer, JitterStats};
pub use load::{CallbackLoad, LoadWindow};
pub use meter::{ChannelMeters, Meter};
pub use params::{db_to_gain, PeerParams, SelfParams};

/// Tests only: counts what the allocator is asked for inside [`alloc_guard::count`], on
/// the calling thread. The audio callbacks must come out at zero: an allocation can take a
/// lock inside the allocator and wait behind another thread.
#[cfg(test)]
pub(crate) mod alloc_guard {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static WATCHING: Cell<bool> = const { Cell::new(false) };
        static CALLS: Cell<u64> = const { Cell::new(0) };
    }

    pub struct Counting;

    fn note() {
        let _ = WATCHING.try_with(|w| {
            if w.get() {
                let _ = CALLS.try_with(|c| c.set(c.get() + 1));
            }
        });
    }

    // SAFETY: every call goes to the system allocator unchanged; only a thread-local
    // counter is touched on the way
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note();
            System.alloc(layout)
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            note();
            System.dealloc(ptr, layout)
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note();
            System.realloc(ptr, layout, new_size)
        }
    }

    /// Runs `f` and returns how many times it allocated, freed or resized memory.
    pub fn count(f: impl FnOnce()) -> u64 {
        CALLS.with(|c| c.set(0));
        WATCHING.with(|w| w.set(true));
        f();
        WATCHING.with(|w| w.set(false));
        CALLS.with(|c| c.get())
    }
}

#[cfg(test)]
#[global_allocator]
static ALLOC: alloc_guard::Counting = alloc_guard::Counting;
