//! Reliable, ordered room messages over the (lossy) UDP link to one peer.
//!
//! Each message gets a sequence number and is resent until the peer acknowledges it.
//! The receiver acknowledges every copy, hands messages over strictly in order and drops
//! duplicates. Messages are small and rare (chat, metronome, profile), so a simple
//! "resend everything unacknowledged every 250 ms" is plenty.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

const RESEND_AFTER: Duration = Duration::from_millis(250);
const OUTBOX_MAX: usize = 256;
/// how far ahead of the next expected message we buffer
const EARLY_WINDOW: u32 = 256;

struct Out {
    seq: u32,
    data: Vec<u8>,
    last_sent: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct ControlChannel {
    next_out: u32,
    outbox: VecDeque<Out>,
    next_in: u32,
    early: BTreeMap<u32, Vec<u8>>,
}

impl ControlChannel {
    /// Queues a message. False if too many are still unacknowledged.
    pub fn queue(&mut self, data: Vec<u8>) -> bool {
        if self.outbox.len() >= OUTBOX_MAX {
            return false;
        }
        let seq = self.next_out;
        self.next_out = self.next_out.wrapping_add(1);
        self.outbox.push_back(Out {
            seq,
            data,
            last_sent: None,
        });
        true
    }

    /// Nothing waits to be sent or acknowledged (the usual state).
    pub fn idle(&self) -> bool {
        self.outbox.is_empty()
    }

    /// Calls `send(seq, data)` for every message due now (new or waiting too long for an ack).
    /// None is ever given up on: the receiver hands messages over strictly in order, so one
    /// that never came would hold back every later one for good (it used to be dropped after
    /// 10 s: a peer silent for longer and then back got no chat, metronome or room state
    /// from us again). The caller does not call this while the peer is silent; a peer that
    /// is gone takes its channel with it.
    pub fn send_due(&mut self, now: Instant, mut send: impl FnMut(u32, &[u8])) {
        for o in self.outbox.iter_mut() {
            if o.last_sent.is_none_or(|t| now - t >= RESEND_AFTER) {
                o.last_sent = Some(now);
                send(o.seq, &o.data);
            }
        }
    }

    pub fn on_ack(&mut self, seq: u32) {
        self.outbox.retain(|o| o.seq != seq);
    }

    /// A message arrived. Returns the messages that are now deliverable, in order
    /// (empty for duplicates or while an earlier one is still missing).
    pub fn on_data(&mut self, seq: u32, payload: &[u8]) -> Vec<Vec<u8>> {
        let ahead = seq.wrapping_sub(self.next_in);
        if ahead >= EARLY_WINDOW {
            return Vec::new(); // already delivered (duplicate) or absurdly far ahead
        }
        self.early.entry(seq).or_insert_with(|| payload.to_vec());
        let mut out = Vec::new();
        while let Some(m) = self.early.remove(&self.next_in) {
            out.push(m);
            self.next_in = self.next_in.wrapping_add(1);
        }
        out
    }

    #[cfg(test)]
    pub fn pending(&self) -> usize {
        self.outbox.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_once_despite_reordering_and_duplicates() {
        let mut rx = ControlChannel::default();
        assert!(rx.on_data(1, b"b").is_empty(), "waits for 0");
        assert!(rx.on_data(2, b"c").is_empty());
        assert_eq!(
            rx.on_data(0, b"a"),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert!(rx.on_data(1, b"b").is_empty(), "duplicate");
        assert_eq!(rx.on_data(3, b"d"), vec![b"d".to_vec()]);
    }

    #[test]
    fn resends_until_acked_and_never_leaves_a_gap() {
        let (mut tx, mut rx) = (ControlChannel::default(), ControlChannel::default());
        tx.queue(b"x".to_vec());
        tx.queue(b"y".to_vec());
        let t0 = Instant::now();
        let mut sent = Vec::new();
        tx.send_due(t0, |s, _| sent.push(s));
        assert_eq!(sent, vec![0, 1]);
        sent.clear();
        tx.send_due(t0 + Duration::from_millis(100), |s, _| sent.push(s));
        assert!(sent.is_empty(), "not yet");
        tx.on_ack(0);
        tx.send_due(t0 + Duration::from_millis(300), |s, _| sent.push(s));
        assert_eq!(sent, vec![1], "only the unacked one");
        // a minute with no acknowledgement (the peer was silent): still there, and once it
        // gets through, what was queued after it is delivered too
        let mut t = t0;
        for _ in 0..240 {
            t += RESEND_AFTER;
            tx.send_due(t, |_, _| {});
        }
        assert_eq!(tx.pending(), 1);
        assert!(rx.on_data(0, b"x").len() == 1);
        tx.queue(b"z".to_vec());
        let mut got = Vec::new();
        tx.send_due(t + RESEND_AFTER, |s, d| got.extend(rx.on_data(s, d)));
        assert_eq!(got, vec![b"y".to_vec(), b"z".to_vec()]);
    }
}
