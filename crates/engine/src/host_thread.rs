//! One thread for everything ASIO.
//!
//! An ASIO driver is a COM object that belongs to the thread that loaded it: list the
//! devices on one thread and open them on another, and the second thread cannot see the
//! device ("no input device"). So every ASIO call (listing, opening, closing streams) runs
//! here, and the streams also live here until their engine is dropped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// A job running this long means the driver stopped answering (opening a device takes a
/// second or two at most).
const STUCK_AFTER: Duration = Duration::from_secs(5);

/// When the running job started (ms since `epoch()`, +1), 0 while the thread is idle.
static BUSY_SINCE: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static T0: OnceLock<Instant> = OnceLock::new();
    *T0.get_or_init(Instant::now)
}

/// The ASIO thread has been inside one driver call for too long: the driver hangs.
/// Nothing new is sent to it then (it would wait forever behind the stuck call).
pub(crate) fn stuck() -> bool {
    let since = BUSY_SINCE.load(Relaxed);
    since != 0 && epoch().elapsed().as_millis() as u64 + 1 - since > STUCK_AFTER.as_millis() as u64
}

/// Streams kept alive on the ASIO thread (cpal streams cannot move between threads).
#[derive(Default)]
pub(crate) struct Kept {
    streams: HashMap<u64, Vec<cpal::Stream>>,
    next: u64,
}

impl Kept {
    pub fn keep(&mut self, streams: Vec<cpal::Stream>) -> u64 {
        self.next += 1;
        self.streams.insert(self.next, streams);
        self.next
    }

    pub fn release(&mut self, id: u64) {
        self.streams.remove(&id);
    }
}

type Job = Box<dyn FnOnce(&mut Kept) + Send>;

fn sender() -> &'static mpsc::Sender<Job> {
    static TX: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("gachi-asio".into())
            .spawn(move || {
                let mut kept = Kept::default();
                while let Ok(job) = rx.recv() {
                    BUSY_SINCE.store(epoch().elapsed().as_millis() as u64 + 1, Relaxed);
                    // a panicking driver must not take the thread (and every later call) down
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut kept)));
                    BUSY_SINCE.store(0, Relaxed);
                }
            })
            .expect("spawn the ASIO thread");
        tx
    })
}

/// Runs `f` on the ASIO thread and waits for its result, at most `limit`. `None` when the
/// driver hangs (then `f` is not even queued) or did not answer in time (then `f` still
/// runs whenever the driver comes back).
pub(crate) fn run<R: Send + 'static>(limit: Duration, f: impl FnOnce(&mut Kept) -> R + Send + 'static) -> Option<R> {
    if stuck() {
        return None;
    }
    let (rtx, rrx) = mpsc::sync_channel(1);
    sender()
        .send(Box::new(move |k| {
            let _ = rtx.send(f(k));
        }))
        .ok()?;
    match rrx.recv_timeout(limit) {
        Ok(r) => Some(r),
        Err(_) => {
            tracing::warn!(?limit, "ASIO driver did not answer in time");
            None
        }
    }
}

/// How long the slow ASIO calls (listing, opening, closing) may take.
pub(crate) const SLOW: Duration = Duration::from_secs(8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hanging_driver_call_does_not_block_the_caller() {
        let (go_tx, go_rx) = mpsc::channel::<()>();
        // a "driver call" that hangs until we let it go
        let t = Instant::now();
        assert_eq!(run(Duration::from_millis(100), move |_| go_rx.recv().ok()), None);
        assert!(t.elapsed() < Duration::from_secs(1));
        // not stuck yet (a slow open is normal), and still busy
        assert!(!stuck());
        go_tx.send(()).unwrap();
        // once it returns, the thread answers again
        assert_eq!(run(Duration::from_secs(5), |_| 7), Some(7));
        assert!(!stuck());
    }
}
