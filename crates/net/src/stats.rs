use std::time::Instant;

/// Network stats for one peer, updated by the receive thread.
#[derive(Clone, Debug, Default)]
pub struct PeerNetStats {
    pub rtt_ms: Option<f32>,
    /// percent of packets missing over the last window (after late copies filled gaps)
    pub loss_pct: f32,
    /// percent that went missing on the way at first (before late copies filled them):
    /// what the sender needs to know to decide on sending copies
    pub raw_loss_pct: f32,
    /// smoothed interarrival jitter (RFC 3550 style), ms
    pub jitter_ms: f32,
    /// how late the latest packets arrive compared to the earliest ones over the last
    /// few seconds, ms. A sender whose sound card hands over 10 ms at a time (WASAPI shared
    /// mode) sends bursts: its first packet of a burst is ~10 ms "late". The jitter buffer
    /// must hold that much, which the averaged `jitter_ms` badly underestimates.
    pub spread_ms: f32,
    /// the lateness worth covering: what the last 30 s showed, apart from bad moments that
    /// would cost more delay to cover than they are worth ([`buffer_level`] at
    /// [`WORTH_BALANCED`]). What the jitter buffer is sized for; a moment beyond it costs one
    /// short concealed gap instead of a bigger delay all the time. Lower than `spread_ms`,
    /// which one bad moment pushes up for seconds.
    pub spread_p_ms: f32,
    /// the worst lateness of each of the last seconds (up to 30, oldest first; stops left
    /// out): what [`buffer_level`] works from, for a caller with its own `worth`
    pub late_seconds: [f32; SLOTS],
    pub late_seconds_n: u8,
    /// enough of this stream (2 s) has been measured for `spread_p_ms` to mean something
    pub settled: bool,
    /// times the sender restarted its stream (new audio settings); measurements start over
    pub restarts: u32,
    /// the sender's packet length (ms; 0 until its first packet). The jitter buffer counts
    /// in the sender's packets, which need not be the size we send.
    pub packet_ms: f32,
    /// what the sender's packets carry, as its last one said: bits per sample, channels,
    /// frames per packet (0 until its first packet)
    pub bits: u8,
    pub channels: u8,
    pub packet_frames: u16,
    pub received: u64,
    pub lost: u64,
    /// the peer's clock minus ours (µs), from the fastest ping round trips
    pub clock_offset_us: Option<i64>,
    /// times the stream stopped altogether for a moment (see `OUTAGE_GAP_MS`): counted
    /// apart from the loss, which they would swamp (one 0.7 s stop read as 60% loss and
    /// sent every packet twice for a minute, which cannot help a stop)
    pub outages: u32,
    /// the latest stop: when, and how long (ms)
    pub last_outage: Option<(Instant, f32)>,
}

impl PeerNetStats {
    /// The measured seconds (see `late_seconds`).
    pub fn seconds(&self) -> &[f32] {
        &self.late_seconds[..(self.late_seconds_n as usize).min(SLOTS)]
    }
}

/// Tracks sequence gaps and arrival jitter for one peer's audio stream.
pub(crate) struct StreamTracker {
    packet_ms: f64,
    sample_rate: f64,
    highest: Option<u32>,
    window_recv: u32,
    window_lost: u32,
    /// gaps filled later (by a late packet or its copy) in this window
    window_filled: u32,
    last_transit: Option<f64>,
    /// earliest transit seen (slowly forgotten, so clock drift and route changes pass)
    min_transit: Option<f64>,
    /// worst lateness in the current and previous window
    spread_now: f64,
    spread_prev: f64,
    window_start: f64,
    /// worst lateness per second over the last 30 s (a ring, oldest first from `ring_pos`)
    ring: [f32; SLOTS],
    ring_pos: usize,
    ring_filled: usize,
    slot_start: f64,
    slot_max: f64,
    /// the least late packet of the current second (see `REBASE_MS`)
    slot_min: f64,
    /// `everyday()` as of the last closed second
    everyday: Option<f32>,
    /// the sequence numbers of the latest stop (a late packet inside it filled nothing that
    /// was counted)
    outage_seqs: Option<(u32, u32)>,
    t0: Instant,
}

const SLOT_MS: f64 = 1000.0;
/// Packets missing in a row for this long (ms) are a stop of the stream (a link dropping
/// for a moment), not loss: copies sent 2 packets later cannot fill it, and the loss figure
/// is about the scattered kind they can.
const OUTAGE_GAP_MS: f64 = 40.0;
/// A second whose packets came this late (ms) was a stop too: no jitter buffer covers it
/// (they hold at most 40 ms), so it does not size the buffer either.
const OUTAGE_LATE_MS: f64 = 200.0;
/// A whole second in which even the earliest packet was this late (ms) is no jitter: the
/// stream's timing moved. The sender paused and went on with the next sequence number (a DAW
/// stopped calling the plugin for a moment, or the app skipped a backlog), or the route got
/// longer. Lateness is measured from there on, at once; the slow leak of the minimum would
/// take minutes (after a 0.3 s pause in DAW mode the buffer sat at its 40 ms limit for 3 min).
const REBASE_MS: f64 = 3.0;
/// 30 s of seconds
pub const SLOTS: usize = 30;
/// What one bad moment per 30 s is worth in delay (ms), for [`buffer_level`]: the buffer
/// grows to cover a kind of lateness only if that costs less delay, all the time, than the
/// moments it removes are worth. A moment is a run of neighbouring late seconds (a hiccup
/// comes as a few stalls within a second or two, and counts once); each one left costs a
/// short concealed gap. Fitted with `crates/engine/examples/policy_sim.rs`: at 4 a link
/// with stalls every few seconds is covered about as before, one that stalls now and then
/// is not, and stalls no buffer could cover no longer make it bigger (Wi-Fi with 40-120 ms
/// stalls: 95% of the time under 18 ms instead of 43, as many gaps). 2 is faster and
/// leaves more gaps to concealment, 8 steadier and slower.
pub const WORTH_BALANCED: f32 = 4.0;
pub const WORTH_FAST: f32 = 2.0;
pub const WORTH_STEADY: f32 = 8.0;
/// Lateness beyond this (ms) no jitter buffer covers (the automatic target stops there):
/// it costs its gap whatever the level, so it does not pull the level up.
pub const MAX_COVER_MS: f32 = 40.0;
/// A bad moment lasts at most this many seconds; a longer run counts as several.
const MOMENT_SECONDS: usize = 3;
/// This many late seconds in a row, up to now, is no hiccup: the link got worse.
const WORSE_AFTER_SECONDS: usize = 4;
/// A history shorter than this (seconds) is weighed as if it were this long (a single
/// hiccup in the first seconds is not "one every two seconds").
const WEIGH_AT_LEAST: usize = 10;

/// Lateness is the max over 2–4 s (two windows), so one burst pattern keeps it up.
const SPREAD_WINDOW_MS: f64 = 2000.0;
/// The earliest transit drifts up this much per second (sound-card clocks differ by
/// ≤ 0.2 ms/s; route changes are rarer), so an old minimum does not stick forever.
const MIN_TRANSIT_LEAK_PER_MS: f64 = 0.001;
/// Until this many seconds were seen, use the plain worst case.
const MIN_SLOTS: usize = 2;

impl StreamTracker {
    /// `sample_rate` = the sender's rate (packet durations are frames / their rate).
    pub fn new(sample_rate: u32) -> Self {
        Self {
            packet_ms: 0.0,
            sample_rate: sample_rate as f64,
            highest: None,
            window_recv: 0,
            window_lost: 0,
            window_filled: 0,
            last_transit: None,
            min_transit: None,
            spread_now: 0.0,
            spread_prev: 0.0,
            window_start: 0.0,
            ring: [0.0; SLOTS],
            ring_pos: 0,
            ring_filled: 0,
            slot_start: 0.0,
            slot_max: 0.0,
            slot_min: f64::MAX,
            everyday: None,
            outage_seqs: None,
            t0: Instant::now(),
        }
    }

    /// Closes the current second into the ring. A second with a stop in it stays out.
    fn close_slot(&mut self) {
        if self.slot_min != f64::MAX && self.slot_min > REBASE_MS {
            if let Some(m) = self.min_transit.as_mut() {
                *m += self.slot_min;
            }
        }
        self.slot_min = f64::MAX;
        if self.slot_max > OUTAGE_LATE_MS {
            self.slot_max = 0.0;
            return;
        }
        self.ring[self.ring_pos] = self.slot_max as f32;
        self.ring_pos = (self.ring_pos + 1) % SLOTS;
        self.ring_filled = (self.ring_filled + 1).min(SLOTS);
        self.slot_max = 0.0;
        self.everyday = buffer_level(&self.seconds(), WORTH_BALANCED);
    }

    /// The measured seconds, oldest first.
    fn seconds(&self) -> Vec<f32> {
        let start = (self.ring_pos + SLOTS - self.ring_filled) % SLOTS;
        (0..self.ring_filled).map(|k| self.ring[(start + k) % SLOTS]).collect()
    }

    fn reset_timing(&mut self) {
        self.last_transit = None;
        self.min_transit = None;
        self.spread_now = 0.0;
        self.spread_prev = 0.0;
        self.ring_pos = 0;
        self.ring_filled = 0;
        self.slot_max = 0.0;
        self.slot_min = f64::MAX;
        self.everyday = None;
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        if sample_rate > 0 && sample_rate as f64 != self.sample_rate {
            self.sample_rate = sample_rate as f64;
            self.reset_timing();
        }
    }

    pub fn on_packet(&mut self, seq: u32, frames: u16, stats: &mut PeerNetStats) {
        let packet_ms = frames as f64 / self.sample_rate * 1000.0;
        if packet_ms != self.packet_ms {
            self.packet_ms = packet_ms;
            self.reset_timing(); // the sender changed packet size: start over
        }
        stats.packet_ms = self.packet_ms as f32;
        stats.received += 1;
        self.window_recv += 1;
        match self.highest {
            None => self.highest = Some(seq),
            Some(h) => {
                let d = seq.wrapping_sub(h) as i32;
                if d > 0 {
                    let gap = (d - 1) as u32;
                    if gap as f64 * packet_ms >= OUTAGE_GAP_MS {
                        // the stream stopped for a moment: a stop, not loss
                        stats.outages += 1;
                        stats.last_outage = Some((Instant::now(), (gap as f64 * packet_ms) as f32));
                        self.outage_seqs = Some((h.wrapping_add(1), seq.wrapping_sub(1)));
                    } else if gap < 1000 {
                        self.window_lost += gap;
                        stats.lost += gap as u64;
                    }
                    self.highest = Some(seq);
                } else if d < -1000 {
                    // the sender restarted its stream (new audio settings): numbering
                    // starts over, and so do the timing measurements
                    self.highest = Some(seq);
                    self.reset_timing();
                    stats.restarts += 1;
                } else if self.outage_seqs.is_some_and(|(a, b)| seq.wrapping_sub(a) <= b.wrapping_sub(a)) {
                    // a late packet from inside a stop: nothing was counted for it
                } else if self.window_lost > 0 {
                    // a late packet filled a gap we already counted
                    self.window_lost -= 1;
                    self.window_filled += 1;
                    stats.lost = stats.lost.saturating_sub(1);
                }
            }
        }

        // transit = arrival - send time (send time reconstructed from seq); only differences matter
        let arrival = self.t0.elapsed().as_secs_f64() * 1000.0;
        self.on_arrival(seq, arrival, stats);
    }

    fn on_arrival(&mut self, seq: u32, arrival: f64, stats: &mut PeerNetStats) {
        let transit = arrival - seq as f64 * self.packet_ms;
        // lateness against the earliest transit (which leaks upward over time)
        let min = self.min_transit.map_or(transit, |m| transit.min(m));
        self.min_transit = Some(min);
        let late = transit - min;
        if arrival - self.window_start >= SPREAD_WINDOW_MS {
            // new window; leak the minimum by the time passed
            if let Some(m) = self.min_transit.as_mut() {
                *m += (arrival - self.window_start) * MIN_TRANSIT_LEAK_PER_MS;
            }
            self.spread_prev = self.spread_now;
            self.spread_now = 0.0;
            self.window_start = arrival;
        }
        self.spread_now = self.spread_now.max(late);
        stats.spread_ms = self.spread_now.max(self.spread_prev) as f32;
        if arrival - self.slot_start >= SLOT_MS {
            if self.slot_start > 0.0 {
                self.close_slot();
                let seconds = self.seconds();
                stats.late_seconds[..seconds.len()].copy_from_slice(&seconds);
                stats.late_seconds_n = seconds.len() as u8;
            }
            self.slot_start = arrival;
        }
        if late > OUTAGE_LATE_MS && self.slot_max <= OUTAGE_LATE_MS {
            // the stream came back after a stop (held somewhere, then delivered at once)
            stats.outages += 1;
            stats.last_outage = Some((Instant::now(), late as f32));
        }
        self.slot_max = self.slot_max.max(late);
        self.slot_min = self.slot_min.min(late);
        stats.spread_p_ms = self.everyday.unwrap_or(stats.spread_ms);
        stats.settled = self.ring_filled >= MIN_SLOTS;
        if let Some(prev) = self.last_transit {
            let d = (transit - prev).abs();
            stats.jitter_ms += ((d - stats.jitter_ms as f64) / 16.0) as f32;
        }
        self.last_transit = Some(transit);

        let total = self.window_recv + self.window_lost;
        if total >= 500 {
            stats.loss_pct = self.window_lost as f32 * 100.0 / total as f32;
            stats.raw_loss_pct = (self.window_lost + self.window_filled) as f32 * 100.0 / total as f32;
            self.window_recv = 0;
            self.window_lost = 0;
            self.window_filled = 0;
        }
    }
}

/// The lateness (ms) worth covering with the jitter buffer, from the worst lateness of each
/// of the last seconds (oldest first). Covering a level costs that much delay, all the
/// time; every bad moment left beyond it (a run of later seconds) costs `worth_ms`, counted
/// per 30 s. The cheapest level wins. `None` until `MIN_SLOTS` seconds are in.
///
/// It used to be "the lowest level at most 3 moments in 30 s go beyond", which sized the
/// buffer for the fourth moment whatever it cost: a link with a 100 ms stall now and then
/// sat at the 40 ms limit, which covers none of them.
pub fn buffer_level(seconds: &[f32], worth_ms: f32) -> Option<f32> {
    if seconds.len() < MIN_SLOTS {
        return None;
    }
    // bad moments above `level`: runs of seconds later than it, one per started
    // `MOMENT_SECONDS` (a longer run is not a hiccup but a bad stretch, and counts more)
    let moments = |level: f32| {
        let mut moments = 0;
        let mut run = 0usize;
        for &s in seconds.iter().chain([0.0].iter()) {
            if s > level {
                run += 1;
            } else if run > 0 {
                moments += run.div_ceil(MOMENT_SECONDS);
                run = 0;
            }
        }
        moments
    };
    let per_30s = SLOTS as f32 / seconds.len().max(WEIGH_AT_LEAST) as f32;
    let cost = |level: f32| level + worth_ms * moments(level) as f32 * per_30s;
    // the levels that make a difference: each second's own lateness (covering exactly it),
    // as far as a buffer can go
    // (what every second shows is no moment but the link's floor: always covered)
    let floor = seconds.iter().fold(f32::MAX, |m, &s| m.min(s)).min(MAX_COVER_MS);
    let mut best = (floor, cost(floor));
    for &l in seconds.iter().filter(|&&l| l <= MAX_COVER_MS) {
        let c = cost(l);
        if c < best.1 || (c == best.1 && l < best.0) {
            best = (l, c);
        }
    }
    // the link just got worse and stays so: follow it now, not once enough moments add up
    let recent = seconds.iter().rev().take(WORSE_AFTER_SECONDS);
    let lately = if seconds.len() >= WORSE_AFTER_SECONDS { recent.fold(f32::MAX, |m, &s| m.min(s)).min(MAX_COVER_MS) } else { 0.0 };
    Some(best.0.max(lately))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds packets of 32 frames at 48 kHz (0.667 ms each), `per_burst` at a time.
    fn spread_for(per_burst: u32, seconds: f64) -> f32 {
        let mut t = StreamTracker::new(48_000);
        t.packet_ms = 32.0 / 48.0;
        let mut st = PeerNetStats::default();
        let period = per_burst as f64 * t.packet_ms;
        let mut seq = 0u32;
        let mut now = 0.0;
        while now < seconds * 1000.0 {
            for _ in 0..per_burst {
                // the whole burst arrives together, 1 ms after the last packet was captured
                t.on_arrival(seq, now + period + 1.0, &mut st);
                seq += 1;
            }
            now += period;
        }
        st.spread_ms
    }

    #[test]
    fn a_restarted_stream_does_not_look_hugely_late() {
        let mut t = StreamTracker::new(48_000);
        let mut st = PeerNetStats::default();
        for seq in 50_000..50_100u32 {
            t.on_packet(seq, 32, &mut st);
        }
        for seq in 0..100u32 {
            t.on_packet(seq, 32, &mut st);
        }
        assert!(st.spread_ms < 50.0, "{}", st.spread_ms);
        assert_eq!(st.lost, 0);
    }

    /// The stream stops for 0.7 s (a link dropping for a moment): a stop, not 60% loss,
    /// while a few scattered missing packets still count as loss.
    #[test]
    fn a_stop_is_not_loss() {
        let mut t = StreamTracker::new(48_000);
        let mut st = PeerNetStats::default();
        for seq in 0..600u32 {
            t.on_packet(seq, 32, &mut st);
        }
        for seq in 1650..2400u32 {
            t.on_packet(seq, 32, &mut st);
        }
        assert_eq!((st.lost, st.outages), (0, 1), "{st:?}");
        assert!(st.last_outage.is_some_and(|(_, ms)| (ms - 700.0).abs() < 1.0), "{st:?}");
        // a late packet from inside the stop fills nothing that was counted
        t.on_packet(1600, 32, &mut st);
        assert_eq!(st.lost, 0);
        // single missing packets are loss as before
        for seq in (2400..2600u32).filter(|s| s % 50 != 0) {
            t.on_packet(seq, 32, &mut st);
        }
        assert_eq!(st.lost, 4, "{st:?}");
    }

    /// One second in which the stream came 0.8 s late (held, then delivered at once) does
    /// not size the jitter buffer for the next 30 s.
    #[test]
    fn a_stop_does_not_size_the_buffer() {
        let mut t = StreamTracker::new(48_000);
        t.packet_ms = 32.0 / 48.0;
        let mut st = PeerNetStats::default();
        let pm = t.packet_ms;
        let mut seq = 0u32;
        let mut now = 0.0;
        while now < 20_000.0 {
            // seconds 8..9: everything arrives 800 ms late
            let late = if (8000.0..9000.0).contains(&now) { 800.0 } else { 0.0 };
            t.on_arrival(seq, now + pm + 0.2 + late, &mut st);
            seq += 1;
            now += pm;
        }
        assert!(st.outages >= 1, "{st:?}");
        assert!(st.spread_p_ms < 5.0, "everyday lateness {}", st.spread_p_ms);
    }

    /// What the level is: the cheapest of "cover it" (that much delay, always) and "leave
    /// it" (`worth` per moment in 30 s).
    #[test]
    fn the_buffer_covers_what_is_worth_covering() {
        let mut quiet = vec![1.5f32; 30];
        // stalls no buffer covers (100 ms), five in 30 s: the old rule went to the limit
        for k in [3, 9, 15, 21, 27] {
            quiet[k] = 100.0;
        }
        assert_eq!(buffer_level(&quiet, WORTH_BALANCED), Some(1.5));
        // a 12 ms stall every 3 s: ten moments, worth covering at any leaning
        let mut bursty = vec![1.5f32; 30];
        for k in (0..30).step_by(3) {
            bursty[k] = 12.0;
        }
        assert_eq!(buffer_level(&bursty, WORTH_FAST), Some(12.0));
        // two 12 ms stalls in 30 s: not worth 10 ms of delay all the time, unless steadiness
        // is what counts
        let mut rare = vec![1.5f32; 30];
        rare[5] = 12.0;
        rare[20] = 12.0;
        assert_eq!(buffer_level(&rare, WORTH_BALANCED), Some(1.5));
        assert_eq!(buffer_level(&rare, WORTH_STEADY), Some(12.0));
        // the first seconds of a stream err on the steady side: a hiccup then is covered
        // (it may be how the link is) until the history shows it was a one-off
        assert_eq!(buffer_level(&[1.5, 12.0, 1.5], WORTH_BALANCED), Some(12.0));
        // and what every second shows is the floor, never left uncovered
        assert_eq!(buffer_level(&[1.5, 1.5, 1.5], WORTH_FAST), Some(1.5));
    }

    #[test]
    fn steady_packets_have_no_spread() {
        assert!(spread_for(1, 5.0) < 0.05, "{}", spread_for(1, 5.0));
    }

    #[test]
    fn one_rare_hiccup_does_not_size_the_buffer() {
        // 10 ms bursts, plus one 25 ms stall every 10 s (twice in 20 s → within RARE_ALLOWED)
        let mut t = StreamTracker::new(48_000);
        t.packet_ms = 32.0 / 48.0;
        let mut st = PeerNetStats::default();
        let (mut seq, mut now) = (0u32, 0.0f64);
        while now < 30_000.0 {
            let stall = if (now as u64 / 10).is_multiple_of(1000) && now > 1000.0 { 15.0 } else { 0.0 };
            for _ in 0..15 {
                t.on_arrival(seq, now + 10.0 + 1.0 + stall, &mut st);
                seq += 1;
            }
            now += 10.0;
        }
        assert!(st.spread_p_ms < 10.5, "p {}", st.spread_p_ms);
    }

    /// A stream like the logs from real tests: 32-frame packets sent two per 64-frame
    /// callback, everyday jitter (exponential, mean `jitter` ms), plus stalls: every packet
    /// sent during a stall of `len` ms arrives at its end. Returns `spread_p_ms` once per
    /// second over `secs`.
    fn trace(secs: f64, jitter: f64, stalls: &[(f64, f64)], seed: u64) -> Vec<f32> {
        let mut t = StreamTracker::new(48_000);
        t.packet_ms = 32.0 / 48.0;
        let mut st = PeerNetStats::default();
        let mut rng = seed;
        let mut rand = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut arrivals: Vec<(f64, u32)> = Vec::new();
        let (mut seq, mut now) = (0u32, 0.0f64);
        while now < secs * 1000.0 {
            for _ in 0..2 {
                let mut at = now + 1.333 + 0.3 - jitter * (1.0 - rand()).ln();
                if let Some(&(s, len)) = stalls.iter().find(|&&(s, len)| at >= s * 1000.0 && at < s * 1000.0 + len) {
                    at = s * 1000.0 + len + 0.1 * rand();
                }
                arrivals.push((at, seq));
                seq += 1;
            }
            now += 1.333;
        }
        arrivals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut out = Vec::new();
        let mut next_sample = 1000.0;
        for (at, s) in arrivals {
            while at >= next_sample {
                out.push(st.spread_p_ms);
                next_sample += 1000.0;
            }
            t.on_arrival(s, at, &mut st);
        }
        out
    }

    /// One bad moment as the logs show it: not a single stall but a few within ~1.5 s
    /// (Wi-Fi power save or scanning, another program waking up), each `len` ms.
    fn cluster(at: f64, len: f64) -> [(f64, f64); 4] {
        [(at, len), (at + 0.3, len * 0.6), (at + 0.8, len), (at + 1.4, len * 0.5)]
    }

    /// Seconds the measured spread spent above `limit`, and its peak.
    fn time_above(p: &[f32], limit: f32) -> (usize, f32) {
        (p.iter().filter(|&&v| v > limit).count(), p.iter().fold(0f32, |m, &v| m.max(v)))
    }

    #[test]
    fn wifi_like_link_with_a_stall_now_and_then() {
        // everyday ~5 ms of lateness, and a 40 ms stall about once a minute (what Wi-Fi 6
        // 5 GHz showed between two PCs in one room)
        let stalls: Vec<(f64, f64)> = [cluster(25.0, 40.0), cluster(80.0, 35.0)].concat();
        let p = trace(120.0, 0.7, &stalls, 7);
        let (secs, peak) = time_above(&p, 12.0);
        println!("wifi: {secs} s above 12 ms (peak {peak:.1}), everyday {:.1}", p[20]);
        assert!(secs <= 2, "sized for the rare stalls for {secs} s (peak {peak:.1})");
    }

    #[test]
    fn a_busy_receiver_now_and_then() {
        // wired, but another program makes us late by ~15 ms every ~12 s (Spotify)
        let stalls: Vec<(f64, f64)> = (1..10).flat_map(|k| cluster(k as f64 * 12.0 + 0.4 * k as f64, 15.0)).collect();
        let p = trace(120.0, 0.1, &stalls, 11);
        let (secs, peak) = time_above(&p, 6.0);
        println!("busy receiver: {secs} s above 6 ms (peak {peak:.1}), everyday {:.1}", p[8]);
        assert!(secs <= 2, "sized for the rare stalls for {secs} s (peak {peak:.1})");
    }

    #[test]
    fn stalls_that_keep_coming_size_the_buffer() {
        // a 20 ms stall every 2 s is the link's normal state: the buffer must cover it
        let stalls: Vec<(f64, f64)> = (2..60).map(|k| (k as f64 * 2.0 + 0.5, 20.0)).collect();
        let p = trace(120.0, 0.1, &stalls, 13);
        let late = &p[30..];
        let covered = late.iter().filter(|&&v| v >= 18.0).count();
        println!("frequent stalls: covered {covered} of {} s", late.len());
        assert!(covered >= late.len() * 9 / 10, "only {covered} of {} s", late.len());
    }

    #[test]
    fn a_link_that_gets_worse_is_followed_within_seconds() {
        // wired, then from 40 s on a stall every 0.5 s (the link went bad and stays so)
        let stalls: Vec<(f64, f64)> = (0..160).map(|k| (40.0 + k as f64 * 0.5, 12.0)).collect();
        let p = trace(120.0, 0.1, &stalls, 17);
        assert!(p[35] < 3.0, "before: {}", p[35]);
        let caught = p[40..].iter().position(|&v| v >= 10.0).expect("never followed");
        println!("worse link: followed after {caught} s");
        assert!(caught <= 6, "took {caught} s");
    }

    #[test]
    fn a_sender_that_paused_and_went_on_is_not_late_forever() {
        // the DAW stops calling the plugin for 324 ms; the sender then goes on with the next
        // sequence number, so every packet after looks 324 ms late
        let mut t = StreamTracker::new(48_000);
        t.packet_ms = 32.0 / 48.0;
        let mut st = PeerNetStats::default();
        let (mut seq, mut now, mut shift) = (0u32, 0.0f64, 0.0f64);
        let mut seen = Vec::new();
        while now < 40_000.0 {
            if (10_000.0..10_000.0 + t.packet_ms).contains(&now) {
                shift += 324.0;
            }
            t.on_arrival(seq, now + 1.0 + shift, &mut st);
            seq += 1;
            now += t.packet_ms;
            if (now as u64).is_multiple_of(1000) && seen.len() < (now / 1000.0) as usize {
                seen.push((st.spread_ms, st.spread_p_ms));
            }
        }
        println!("after a pause: {:?}", &seen[8..16]);
        // a few seconds of a big spread right after (one bad moment), then normal again
        let late: Vec<_> = seen[14..].iter().filter(|(s, p)| *s > 3.0 || *p > 3.0).collect();
        assert!(late.is_empty(), "still late after the pause: {late:?}");
        assert!(seen[12].1 < 3.0, "the buffer is never sized for the pause: {:?}", seen[12]);
    }

    #[test]
    fn wasapi_bursts_show_their_length() {
        // 15 packets = 10 ms per burst: the first one waited ~9.3 ms for the rest
        let s = spread_for(15, 5.0);
        assert!((9.0..9.7).contains(&s), "{s}");
    }
}
