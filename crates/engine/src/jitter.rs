//! Per-peer jitter buffer. Lives on the output audio thread: fixed capacity, no allocation.
//!
//! - Reorders packets by sequence number.
//! - Waits until `target` packets are buffered before playing (priming).
//! - **Concealment**: a missing packet, or a buffer that ran dry, is filled by continuing the
//!   sound: the pitch period of the last ~20 ms is found (normalized autocorrelation) and
//!   repeated, at full level for 10 ms, fading out by 60 ms. When real sound comes back it
//!   is crossfaded in over 2 ms. (It used to repeat the last 32-frame packet at half volume,
//!   a 1.5 kHz buzz, and fall silent when the buffer ran dry.)
//! - **No clicks**: wherever the sound does not continue on its own (a concealed packet, the
//!   real one after it, a skipped packet, starting after silence) the jump between the last
//!   sample played and the new one is spread over `SMOOTH` samples (~0.3 ms), and running
//!   dry fades out over the same time instead of cutting to zero. No added delay.
//! - **Time-scaling**: when the buffer holds more than it should (after a stall everything
//!   sent meanwhile lands at once; the target was cut) the extra is not dropped but played
//!   a little faster: one pitch period at a time is folded into the next (overlap-add), up
//!   to a quarter of the stretch, until the delay is back. When the target is raised a lot,
//!   periods are doubled the same way instead of stopping to refill. Only what is beyond
//!   60 ms is dropped outright.
//! - Runs dry → underrun → re-prime at half the target. What the sender sent during the
//!   stall then lands at once: playback starts from the newest `target` packets of it, so
//!   the delay does not stay up by the stall's length.
//! - Target changes: small ones are left to the drift control; a big raise re-primes (one
//!   short gap), a big cut drops the extra packets at once (one smoothed splice) instead
//!   of draining them at 1 ms per second.
//! - **Clock drift**: the sender's sound card never runs at exactly our rate (typically
//!   ±20–200 ppm), so the buffer slowly fills or drains. A slow controller watches the
//!   average fill and reads slightly faster or slower (linear interpolation, at most
//!   ±1000 ppm ≈ ±1.7 cents), so the buffer stays at its target without skips or dropouts.
//! - **Sample-rate conversion**: a peer running at another rate (e.g. 44.1 kHz into our
//!   48 kHz) is read at `base_ratio` = their rate / ours, on top of the drift correction.
//!   Reads between samples use 4-point Catmull-Rom interpolation (much cleaner than linear).

use crate::frame::Frame;

/// Slots (packets). Targets go up to half of this: 128 packets ≈ 43 ms even with the
/// smallest (16-frame) packets, enough for a bad Wi-Fi link (~25 ms) with room to spare.
/// (A slot holds one frame of up to 1 KB: 256 KB per peer.)
const CAP: usize = 256;
/// Max read-rate correction (1000 ppm).
const MAX_CORRECTION: f64 = 0.001;
/// PI controller on the fill error (packets, beyond the dead band):
/// proportional part reacts, integral part learns the steady clock offset.
/// Critically damped (no overshoot) for any packet size: with n packets/s the loop is
/// e'' = -n²·KI·e - n·KP·e', ζ = KP / (2·√KI) = 1. The old KP 0.0002 / KI 1e-7 (ζ 0.3)
/// swung the correction between +600 and -300 ppm for a 60 ppm clock difference, the
/// delay with it (±1 ms, ~25 s period): see `examples/jitter_live.rs`.
const KP: f64 = 0.0004;
const KI: f64 = 0.000_000_04;
/// Fill error (packets) we tolerate before correcting: the averaged fill wobbles a little
/// with callback/packet timing.
const DEAD_BAND: f64 = 0.5;
/// Only integrate the fill error when the average is this close to target (packets). Wide
/// enough for the offset the proportional part alone leaves at a 300 ppm clock difference
/// (1.5 packets beyond the dead band), so the integral can take it over.
const INTEGRATE_WITHIN: f64 = 3.0;
/// A target change this big (frames: 3 ms at 48 kHz, whatever the packet size) is not
/// left to the drift control, which moves 1 ms per second: a raise re-primes (one short
/// gap), a cut drops the extra packets at once (one smoothed splice).
const BIG_CHANGE_FRAMES: u32 = 144;
/// Packet length assumed before the first packet tells.
const DEFAULT_PACKET_FRAMES: u32 = 32;
/// Samples a jump in the waveform is spread over (0.33 ms at 48 kHz).
const SMOOTH: u32 = 16;

/// Concealment: source frames of history kept (≈ 43 ms at 48 kHz).
const HIST: usize = 2048;
/// Pitch periods searched (frames): 1.2 kHz down to 60 Hz.
const PITCH_MIN: usize = 40;
const PITCH_MAX: usize = 800;
/// Frames compared when looking for the period.
const PITCH_WIN: usize = 256;
/// Best normalized correlation under this: the sound has no period (noise).
const NOT_PERIODIC: f32 = 0.3;
/// Made-up sound: full level this long, then fading, silent from `PLC_END` (frames:
/// 10 ms and 60 ms at 48 kHz).
const PLC_FULL: u32 = 480;
const PLC_END: u32 = 2880;
/// Crossfade from made-up sound back into the real one (frames, 2 ms).
const XFADE: u32 = 96;

/// Time-scaling works on this many frames at once (20 ms), joined from the packets that
/// follow; `SEG_MAX` is the room for them plus a doubled period.
const SCALE_FRAMES: usize = 960;
const SEG_MAX: usize = 2304;
/// Frames compared when looking for the period to fold or double.
const SCALE_WIN: usize = 128;
/// Most the buffer owes in frames to play faster (or slower) for; beyond it, packets are
/// dropped as before (60 ms at 48 kHz: over a second of catching up).
const MAX_DEBT: u32 = 2880;

/// EMA weight per played packet (time constant ≈ 500 packets ≈ 0.3 s at 32 frames).
const FILL_ALPHA: f64 = 0.002;

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitterStats {
    pub depth: u32,
    pub target: u32,
    pub underruns: u32,
    pub concealed: u32,
    pub late: u32,
    pub skipped: u32,
    /// pitch periods folded or doubled to change the delay without a gap
    pub scaled: u32,
    /// current read-rate correction in ppm (+ = reading faster: the sender's clock is fast)
    pub drift_ppm: i32,
}

/// The stereo frames being played: one packet, or several joined while time-scaling.
struct Seg {
    frames: usize,
    data: Box<[f32]>,
}

impl Seg {
    fn new() -> Self {
        Self { frames: 0, data: vec![0.0; SEG_MAX * 2].into_boxed_slice() }
    }

    fn load(&mut self, f: &Frame) {
        self.frames = 0;
        self.append(f);
    }

    /// Adds a packet (mono doubled) if it fits.
    fn append(&mut self, f: &Frame) -> bool {
        let n = f.frames as usize;
        if self.frames + n > SEG_MAX {
            return false;
        }
        let ch = f.channels.max(1) as usize;
        for k in 0..n {
            let (b, o) = (k * ch, (self.frames + k) * 2);
            self.data[o] = f.data[b];
            self.data[o + 1] = f.data[b + (ch - 1).min(1)];
        }
        self.frames += n;
        true
    }
}

pub struct JitterBuffer {
    slots: Box<[Option<Frame>]>,
    next: Option<u32>,
    newest: u32,
    /// the target rounded up: priming and skip-ahead count whole packets
    target: u32,
    /// the exact target (fraction of a packet allowed): what the drift control steers to
    target_f: f64,
    primed: bool,
    /// packets to collect before playing (the target, or half of it after a dropout)
    prime_at: u32,
    cur: Seg,
    /// fractional read position inside `cur`, in frames
    cur_pos: f64,
    cur_valid: bool,
    last: Frame,
    stats: JitterStats,
    drift: bool,
    fill_avg: Option<f64>,
    integral: f64,
    ratio: f64,
    /// source rate / our rate (1.0 when both run at the same rate)
    base_ratio: f64,
    /// the last frames of the previous segment (left neighbours for interpolation)
    tail: [(f32, f32); TAIL],
    sinc: &'static [f32],
    /// the last sample we played (l, r)
    out_last: (f32, f32),
    /// the next packet does not continue the sound: smooth the jump into it
    seam: bool,
    /// offset being faded out (l, r) and samples left
    smooth: (f32, f32),
    smooth_left: u32,
    /// the packet playing now was made up (concealment)
    concealing: bool,
    /// smoothing on (always, except in tests of the packet logic with step signals)
    declick: bool,
    /// packets to drop at the next packet boundary (the target was cut a lot)
    trim: u32,
    /// priming again after running dry (not the first start)
    refill: bool,
    /// concealment: the last `HIST` source frames played (ring), write index, frames in it
    hist: Box<[(f32, f32)]>,
    hist_w: usize,
    hist_n: usize,
    /// the period being repeated (frames), a copy of it, position in it, frames made so far
    plc_tmpl: Box<[(f32, f32)]>,
    plc_period: usize,
    plc_pos: usize,
    plc_t: u32,
    /// a made-up stream is running (and can be continued or crossfaded out of)
    plc_on: bool,
    /// frames of real sound still to crossfade in from the made-up one
    xfade_left: u32,
    /// scratch for the pitch search (mono)
    pitch_buf: Box<[f32]>,
    /// pitch-based concealment on (else the old half-volume repeat and silence when dry)
    plc: bool,
    /// frames held beyond the target, to be played faster for (see `time_scale`), and
    /// frames short of a raised target, to be played slower for. Never both.
    remove_debt: u32,
    insert_debt: u32,
    /// time-scaling on (else the extra is dropped and a raise stops to refill, as before)
    scale: bool,
}

impl JitterBuffer {
    pub fn new(target: u32) -> Self {
        let mut jb = Self {
            slots: vec![None; CAP].into_boxed_slice(),
            next: None,
            newest: 0,
            target: 1,
            target_f: 1.0,
            primed: false,
            prime_at: 1,
            cur: Seg::new(),
            cur_pos: 0.0,
            cur_valid: false,
            last: Frame::EMPTY,
            stats: JitterStats::default(),
            drift: true,
            fill_avg: None,
            integral: 0.0,
            ratio: 1.0,
            base_ratio: 1.0,
            tail: [(0.0, 0.0); TAIL],
            sinc: sinc_table(),
            out_last: (0.0, 0.0),
            seam: false,
            smooth: (0.0, 0.0),
            smooth_left: 0,
            concealing: false,
            declick: true,
            trim: 0,
            refill: false,
            hist: vec![(0.0, 0.0); HIST].into_boxed_slice(),
            hist_w: 0,
            hist_n: 0,
            plc_tmpl: vec![(0.0, 0.0); PITCH_MAX].into_boxed_slice(),
            plc_period: PITCH_MIN,
            plc_pos: 0,
            plc_t: 0,
            plc_on: false,
            xfade_left: 0,
            pitch_buf: vec![0.0; PITCH_WIN + PITCH_MAX].into_boxed_slice(),
            plc: true,
            remove_debt: 0,
            insert_debt: 0,
            scale: true,
        };
        jb.set_target(target);
        jb
    }

    pub fn set_target(&mut self, packets: u32) {
        self.set_target_f(packets as f64);
    }

    /// The target in packets, fractions allowed (e.g. 4.4): the average fill is steered
    /// there, so the delay can sit between two whole packets.
    pub fn set_target_f(&mut self, packets: f64) {
        let packets = if packets.is_finite() { packets.clamp(1.0, (CAP / 2) as f64) } else { 1.0 };
        let new = packets.ceil() as u32;
        let old_f = self.target_f;
        self.target_f = packets;
        if new == self.target && (packets - old_f).abs() < 0.5 {
            return; // a small nudge: the drift control follows by itself
        }
        let pf = if self.last.frames > 0 { self.last.frames as u32 } else { DEFAULT_PACKET_FRAMES };
        let big = BIG_CHANGE_FRAMES.div_ceil(pf);
        let grow = new.saturating_sub(self.target);
        // what the buffer holds beyond the new target, on average (whole packets)
        let excess = self.fill_avg.map_or(0.0, |a| a - packets).floor().max(0.0) as u32;
        self.target = new;
        self.prime_at = new;
        // the fill average now measures against another target: start it over (the learned
        // clock drift in `integral` stays)
        self.fill_avg = None;
        self.trim = 0;
        // a much bigger target means the network got worse: wait once for the buffer to
        // fill (one short gap) instead of crawling there at -1000 ppm for seconds while
        // still running dry
        if grow >= big && self.primed {
            if self.scaling() && self.hist_n >= PITCH_MAX {
                // play slower until the buffer has grown by that much
                self.owe(-((grow * pf) as i64));
            } else {
                self.primed = false;
            }
        }
        // much smaller (the network is better again, or the first measurement is in): drop
        // the extra delay at once rather than over many seconds
        if excess >= big && self.primed {
            self.trim = excess;
        }
    }

    /// Pitch-based concealment (on by default); off = the old half-volume repeat of the last
    /// packet, and silence when the buffer runs dry. For comparisons.
    pub fn set_concealment(&mut self, pitch: bool) {
        self.plc = pitch;
    }

    /// Time-scaling (on by default); off = packets over the target are dropped and a big
    /// raise stops to refill. For comparisons.
    pub fn set_time_scaling(&mut self, on: bool) {
        self.scale = on;
    }

    /// Turn drift correction off for streams on our own clock (local monitoring).
    pub fn set_drift_correction(&mut self, on: bool) {
        self.drift = on;
        if !on {
            self.ratio = 1.0;
            self.integral = 0.0;
            self.fill_avg = None;
        }
    }

    /// Sender's sample rate / ours. Clamped to 0.25..=4.
    pub fn set_base_ratio(&mut self, r: f64) {
        if r.is_finite() {
            self.base_ratio = r.clamp(0.25, 4.0);
        }
    }

    pub fn stats(&self) -> JitterStats {
        let drift_ppm = ((self.ratio - 1.0) * 1e6).round() as i32;
        JitterStats { depth: self.buffered(), target: self.target, drift_ppm, ..self.stats }
    }

    fn idx(seq: u32) -> usize {
        seq as usize % CAP
    }

    /// Sequence positions between the play head and the newest packet (some may be missing).
    fn buffered(&self) -> u32 {
        match self.next {
            Some(n) => {
                let d = self.newest.wrapping_sub(n) as i32;
                if d < 0 { 0 } else { d as u32 + 1 }
            }
            None => 0,
        }
    }

    fn reset_to(&mut self, seq: u32) {
        self.slots.iter_mut().for_each(|s| *s = None);
        self.next = Some(seq);
        self.newest = seq;
        self.primed = false;
        self.prime_at = self.target;
        self.refill = false;
        self.trim = 0;
        self.fill_avg = None;
        self.integral = 0.0;
        self.ratio = 1.0;
        self.remove_debt = 0;
        self.insert_debt = 0;
        // a new stream: nothing of the old one to continue
        self.hist_n = 0;
        self.plc_on = false;
        self.xfade_left = 0;
    }

    /// Called once per played packet: steer the read rate so the average fill sits at target.
    fn update_drift(&mut self, buffered: u32) {
        if !self.drift {
            return;
        }
        // (a debt too small to fold a period for is left to this control: under a ms)
        if self.remove_debt < PITCH_MIN as u32 {
            self.remove_debt = 0;
        }
        if self.insert_debt < PITCH_MIN as u32 {
            self.insert_debt = 0;
        }
        let pf = self.packet_frames() as f64;
        let fill = buffered as f64 - self.remove_debt as f64 / pf + self.insert_debt as f64 / pf;
        // start from the target so the priming burst does not kick the controller
        let avg = self.fill_avg.unwrap_or(self.target_f);
        let avg = avg + (fill - avg) * FILL_ALPHA;
        self.fill_avg = Some(avg);
        let err = avg - self.target_f;
        let beyond = if err > DEAD_BAND {
            err - DEAD_BAND
        } else if err < -DEAD_BAND {
            err + DEAD_BAND
        } else {
            0.0
        };
        // the integral learns the clock difference only near the target; far from it (a
        // burst, a target change) it would wind up and push the pitch for seconds
        // nor while the correction is already at its limit (it would only wind up)
        let saturated = (beyond * KP + self.integral).abs() >= MAX_CORRECTION;
        if err.abs() < INTEGRATE_WITHIN && !saturated {
            self.integral = (self.integral + beyond * KI).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        }
        self.ratio = 1.0 + (beyond * KP + self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
    }

    /// Frame `k` (l, r) of the packet after `cur`, if it is already here (for interpolation).
    /// Frame `k` of what follows the segment being played (in the packets waiting), if it
    /// is here.
    fn peek_next(&self, mut k: usize) -> Option<(f32, f32)> {
        let mut seq = self.next?;
        loop {
            let f = self.slots[Self::idx(seq)].as_ref()?;
            if f.seq != seq || f.frames == 0 {
                return None;
            }
            if k < f.frames as usize {
                let ch = f.channels as usize;
                return Some(if ch == 1 { (f.data[k], f.data[k]) } else { (f.data[k * 2], f.data[k * 2 + 1]) });
            }
            k -= f.frames as usize;
            seq = seq.wrapping_add(1);
        }
    }

    pub fn push(&mut self, f: Frame) {
        let Some(next) = self.next else {
            self.reset_to(f.seq);
            self.slots[Self::idx(f.seq)] = Some(f);
            return;
        };
        let d = f.seq.wrapping_sub(next) as i32;
        if d < -(CAP as i32) {
            // far behind anything we could still play: the sender restarted its stream
            // (changed its audio settings → sequence numbers start over). Follow it instead
            // of dropping everything it sends from now on as "late".
            self.reset_to(f.seq);
            self.slots[Self::idx(f.seq)] = Some(f);
            return;
        }
        if d < 0 {
            self.stats.late += 1;
            return;
        }
        if d as usize >= CAP {
            // sender restarted or a very long gap: start over from here
            self.reset_to(f.seq);
        } else if (f.seq.wrapping_sub(self.newest) as i32) > 0 {
            self.newest = f.seq;
        }
        self.slots[Self::idx(f.seq)] = Some(f);
    }

    /// Fills `out` with interleaved stereo. Mono packets are duplicated to both channels.
    pub fn read_stereo(&mut self, out: &mut [f32]) {
        let mut o = 0;
        while o + 1 < out.len() {
            if self.cur_valid {
                let ch = 2usize;
                let frames = self.cur.frames;
                let step = self.ratio * self.base_ratio;
                while o + 1 < out.len() {
                    let i = self.cur_pos as usize;
                    if i >= frames {
                        break;
                    }
                    let frac = (self.cur_pos - i as f64) as f32;
                    let (l, r) = {
                        let at = |k: usize| {
                            let b = k * ch;
                            (self.cur.data[b], self.cur.data[b + (ch - 1).min(1)])
                        };
                        if frac == 0.0 {
                            at(i)
                        } else {
                            // the frames around the read position, weighted by a windowed sinc
                            let first = i as isize - (HALF as isize - 1);
                            let mut taps = [(0f32, 0f32); TAPS];
                            if first >= 0 && i + HALF < frames {
                                let d = &self.cur.data[first as usize * 2..(first as usize + TAPS) * 2];
                                for (t, f) in taps.iter_mut().zip(d.as_chunks::<2>().0) {
                                    *t = (f[0], f[1]);
                                }
                            } else {
                                // neighbours may sit in the previous segment or the next packets
                                for (j, t) in taps.iter_mut().enumerate() {
                                    let k = first + j as isize;
                                    *t = if k < 0 {
                                        self.tail[(TAIL as isize + k).max(0) as usize]
                                    } else if (k as usize) < frames {
                                        at(k as usize)
                                    } else {
                                        self.peek_next(k as usize - frames).unwrap_or_else(|| at(frames - 1))
                                    };
                                }
                            }
                            interpolate(self.sinc, &taps, frac)
                        }
                    };
                    let (mut l, mut r) = (l, r);
                    if self.seam && self.declick {
                        // spread the jump from what we played last into this packet
                        self.seam = false;
                        self.smooth = (self.out_last.0 - l, self.out_last.1 - r);
                        self.smooth_left = SMOOTH;
                    }
                    if self.smooth_left > 0 {
                        let w = self.smooth_left as f32 / (SMOOTH + 1) as f32;
                        l += self.smooth.0 * w;
                        r += self.smooth.1 * w;
                        self.smooth_left -= 1;
                    }
                    out[o] = l;
                    out[o + 1] = r;
                    self.out_last = (l, r);
                    o += 2;
                    self.cur_pos += step;
                }
                if self.cur_pos as usize >= frames {
                    // (a segment shorter than the tail: the older frames stay, moved up)
                    let keep = frames.min(TAIL);
                    self.tail.copy_within(keep.., 0);
                    for (t, f) in self.tail[TAIL - keep..].iter_mut().zip(self.cur.data[(frames - keep) * 2..frames * 2].as_chunks::<2>().0) {
                        *t = (f[0], f[1]);
                    }
                    self.cur_pos -= frames as f64; // carry the fraction into the next packet
                    self.cur_valid = false;
                }
                continue;
            }
            if !self.advance() {
                // ran dry: keep the sound going (for up to 60 ms) while the buffer refills
                if self.declick && self.plc && self.plc_can_continue() {
                    if !self.plc_on {
                        self.plc_start();
                        self.seam = true;
                    }
                    let frames = if self.last.frames > 0 { self.last.frames as usize } else { DEFAULT_PACKET_FRAMES as usize };
                    self.plc_frame(frames);
                    self.concealing = true;
                    self.cur_valid = true;
                    continue;
                }
                // nothing to play: fade out from where the sound was, then silence
                let (l0, r0) = if self.declick { self.out_last } else { (0.0, 0.0) };
                let rest = &mut out[o..];
                for (k, f) in rest.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let w = (SMOOTH as usize).saturating_sub(k + 1) as f32 / SMOOTH as f32;
                    *f = [l0 * w, r0 * w];
                }
                if let Some(&[l, r]) = rest.as_chunks::<2>().0.last() {
                    self.out_last = (l, r);
                }
                self.smooth_left = 0;
                // whatever plays next starts from here
                self.seam = true;
                return;
            }
        }
    }

    /// Keeps a packet that is about to play (source frames) for concealment.
    fn remember(&mut self, f: &Frame) {
        let ch = f.channels.max(1) as usize;
        for k in 0..f.frames as usize {
            let b = k * ch;
            self.hist[self.hist_w] = (f.data[b], f.data[b + (ch - 1).min(1)]);
            self.hist_w = (self.hist_w + 1) % HIST;
        }
        self.hist_n = (self.hist_n + f.frames as usize).min(HIST);
    }

    /// History frame `back` frames before the end (1 = the newest).
    fn hist_at(&self, back: usize) -> (f32, f32) {
        self.hist[(self.hist_w + HIST - back) % HIST]
    }

    /// Enough real sound to continue, or a made-up stream that has not faded out yet.
    fn plc_can_continue(&self) -> bool {
        if self.plc_on {
            self.plc_t < PLC_END
        } else {
            self.hist_n >= PITCH_MIN * 2
        }
    }

    /// The period (frames) the last stretch of sound repeats with: the lag with the highest
    /// normalized correlation, searched on every 4th lag and frame, then refined at full rate.
    fn pitch(&mut self) -> usize {
        let max_lag = PITCH_MAX.min(self.hist_n.saturating_sub(PITCH_WIN / 4));
        if max_lag <= PITCH_MIN {
            return (self.hist_n / 2).max(1);
        }
        let win = PITCH_WIN.min(self.hist_n - max_lag);
        let n = win + max_lag;
        // mono, oldest first
        for i in 0..n {
            let (l, r) = self.hist_at(n - i);
            self.pitch_buf[i] = 0.5 * (l + r);
        }
        let x = &self.pitch_buf[..n];
        let corr = |lag: usize, step: usize| -> f32 {
            let (mut xy, mut xx, mut yy) = (0f32, 0f32, 0f32);
            let mut i = n - win;
            while i < n {
                let (a, b) = (x[i], x[i - lag]);
                xy += a * b;
                xx += a * a;
                yy += b * b;
                i += step;
            }
            if xx * yy <= 1e-12 { 0.0 } else { xy / (xx * yy).sqrt() }
        };
        let mut best = (PITCH_MIN, f32::MIN);
        let mut lag = PITCH_MIN;
        while lag <= max_lag {
            let c = corr(lag, 4);
            if c > best.1 {
                best = (lag, c);
            }
            lag += 4;
        }
        let (lo, hi) = (best.0.saturating_sub(4).max(PITCH_MIN), (best.0 + 4).min(max_lag));
        let mut fine = (lo, f32::MIN);
        for lag in lo..=hi {
            let c = corr(lag, 1);
            if c > fine.1 {
                fine = (lag, c);
            }
        }
        // no period to speak of (noise, cymbals): repeat the longest stretch, which still
        // sounds like noise, rather than a short one, which buzzes at its repeat rate
        if fine.1 < NOT_PERIODIC { max_lag } else { fine.0 }
    }

    /// Starts a made-up stream from the end of the history.
    fn plc_start(&mut self) {
        let p = self.pitch().clamp(1, PITCH_MAX);
        for k in 0..p {
            self.plc_tmpl[k] = self.hist_at(p - k);
        }
        self.plc_period = p;
        self.plc_pos = 0;
        self.plc_t = 0;
        self.plc_on = true;
    }

    /// The next made-up frame: the period repeated, full level for `PLC_FULL`, then fading
    /// to silence at `PLC_END`.
    fn plc_next(&mut self) -> (f32, f32) {
        let g = if self.plc_t < PLC_FULL {
            1.0
        } else if self.plc_t < PLC_END {
            1.0 - (self.plc_t - PLC_FULL) as f32 / (PLC_END - PLC_FULL) as f32
        } else {
            0.0
        };
        let (l, r) = self.plc_tmpl[self.plc_pos];
        self.plc_pos = (self.plc_pos + 1) % self.plc_period;
        self.plc_t = self.plc_t.saturating_add(1);
        (l * g, r * g)
    }

    /// Fills `cur` with `frames` made-up frames (stereo). They go into the history too: it
    /// holds what was played, without holes, so the next concealment continues the sound
    /// as heard.
    fn plc_frame(&mut self, frames: usize) {
        let frames = frames.clamp(1, crate::frame::MAX_FRAME_SAMPLES / 2);
        let mut f = Frame { seq: 0, channels: 2, frames: frames as u16, ..Frame::EMPTY };
        for k in 0..frames {
            let (l, r) = self.plc_next();
            f.data[k * 2] = l;
            f.data[k * 2 + 1] = r;
        }
        self.remember(&f);
        self.cur.load(&f);
    }

    /// Time-scaling is on (with the concealment; the packet-logic tests run without).
    fn scaling(&self) -> bool {
        self.declick && self.plc && self.scale
    }

    fn packet_frames(&self) -> u32 {
        if self.last.frames > 0 { self.last.frames as u32 } else { DEFAULT_PACKET_FRAMES }
    }

    /// Books `frames` to play faster for (positive: the buffer holds that much too much) or
    /// slower for (negative). The two cancel. Returns the frames beyond `MAX_DEBT`, which
    /// the caller drops outright.
    fn owe(&mut self, frames: i64) -> u32 {
        let net = self.remove_debt as i64 - self.insert_debt as i64 + frames;
        let over = (net - MAX_DEBT as i64).max(0);
        let net = net.clamp(-(MAX_DEBT as i64), MAX_DEBT as i64);
        self.remove_debt = net.max(0) as u32;
        self.insert_debt = (-net).max(0) as u32;
        over as u32
    }

    /// Books `packets` too many: as a debt to play faster for, dropping only what is beyond
    /// `MAX_DEBT`. Returns the packets to drop now.
    fn owe_packets(&mut self, packets: u32) -> u32 {
        if !self.scaling() {
            return packets;
        }
        let pf = self.packet_frames();
        self.owe((packets * pf) as i64).div_ceil(pf)
    }

    /// Pays some of a debt on the segment about to play: joins the packets that follow (they
    /// are here already; taking them early changes no timing), finds the period the sound
    /// repeats with, and folds two periods into one (faster) or plays one twice (slower),
    /// crossfading over the whole period. At most a quarter of the stretch per call.
    fn time_scale(&mut self) {
        let removing = self.remove_debt >= PITCH_MIN as u32;
        if !removing && self.insert_debt < PITCH_MIN as u32 {
            return;
        }
        while self.cur.frames < if removing { SCALE_FRAMES } else { PITCH_MAX } {
            let Some(next) = self.next else { break };
            let Some(f) = self.slots[Self::idx(next)] else { break };
            if !self.cur.append(&f) {
                break;
            }
            self.slots[Self::idx(next)] = None;
            self.next = Some(next.wrapping_add(1));
            self.remember(&f);
            self.last = f;
        }
        let len = self.cur.frames;
        if !removing {
            // slower: little is buffered (that is why), so the period comes from what was
            // just played. H is the period before this segment, C the segment's start:
            // insert a period that starts as C would and ends as H did, then C follows
            let lag = self.pitch().min(self.insert_debt as usize);
            if lag < PITCH_MIN || lag > len || len + lag > SEG_MAX || self.hist_n < len + lag {
                return;
            }
            let d = &mut self.cur.data;
            d.copy_within(0..2 * len, 2 * lag);
            for i in 0..lag {
                let w = (i + 1) as f32 / (lag + 1) as f32;
                // (the history holds this segment too: the period before it is further back)
                let h = self.hist[(self.hist_w + HIST - (len + lag - i)) % HIST];
                d[2 * i] = d[2 * (lag + i)] * (1.0 - w) + h.0 * w;
                d[2 * i + 1] = d[2 * (lag + i) + 1] * (1.0 - w) + h.1 * w;
            }
            self.cur.frames += lag;
            self.insert_debt -= lag as u32;
            self.stats.scaled += 1;
            return;
        }
        let win = SCALE_WIN.min(len / 4);
        let max_lag = (len / 4).min(self.remove_debt as usize).min(PITCH_MAX);
        if win < 32 || max_lag < PITCH_MIN {
            return;
        }
        let d = &mut self.cur.data;
        // the lag at which the start of the segment best matches itself
        let corr = |d: &[f32], lag: usize| -> f32 {
            let (mut xy, mut xx, mut yy) = (0f32, 0f32, 0f32);
            for k in 0..win {
                let a = d[2 * k] + d[2 * k + 1];
                let b = d[2 * (k + lag)] + d[2 * (k + lag) + 1];
                xy += a * b;
                xx += a * a;
                yy += b * b;
            }
            if xx * yy <= 1e-12 { 0.0 } else { xy / (xx * yy).sqrt() }
        };
        let mut best = (max_lag, f32::MIN);
        let mut lag = PITCH_MIN;
        while lag <= max_lag {
            let c = corr(d, lag);
            if c > best.1 {
                best = (lag, c);
            }
            lag += 2;
        }
        for lag in [best.0.saturating_sub(1).max(PITCH_MIN), (best.0 + 1).min(max_lag)] {
            let c = corr(d, lag);
            if c > best.1 {
                best = (lag, c);
            }
        }
        let lag = best.0;
        // A B → one period that starts as A and ends as B
        for i in 0..lag {
            let w = (i + 1) as f32 / (lag + 1) as f32;
            for c in 0..2 {
                d[2 * i + c] = d[2 * i + c] * (1.0 - w) + d[2 * (i + lag) + c] * w;
            }
        }
        d.copy_within(4 * lag..2 * len, 2 * lag);
        self.cur.frames -= lag;
        self.remove_debt -= lag as u32;
        self.stats.scaled += 1;
    }

    /// Blends the start of a real packet with the made-up stream it takes over from.
    fn crossfade_in(&mut self, f: &mut Frame) {
        let ch = f.channels.max(1) as usize;
        for k in 0..f.frames as usize {
            if self.xfade_left == 0 {
                break;
            }
            let w = (XFADE - self.xfade_left + 1) as f32 / (XFADE + 1) as f32;
            let (sl, sr) = self.plc_next();
            let b = k * ch;
            f.data[b] = f.data[b] * w + sl * (1.0 - w);
            if ch > 1 {
                f.data[b + 1] = f.data[b + 1] * w + sr * (1.0 - w);
            }
            self.xfade_left -= 1;
        }
        if self.xfade_left == 0 {
            self.plc_on = false;
        }
    }

    fn advance(&mut self) -> bool {
        loop {
            let Some(mut next) = self.next else { return false };
            let mut buffered = self.buffered();
            if !self.primed {
                if buffered < self.prime_at {
                    return false;
                }
                self.primed = true;
                self.prime_at = self.target;
                // after a stall, all that was sent meanwhile lands at once: start from the
                // newest `target` packets. Playing all of it held the delay up by the
                // stall's length (2× the target after skip-ahead), draining for ~10 s
                if std::mem::take(&mut self.refill) && buffered > self.target {
                    // (what can be caught up with by playing faster is kept)
                    let drop = self.owe_packets(buffered - self.target);
                    for _ in 0..drop {
                        self.slots[Self::idx(next)] = None;
                        next = next.wrapping_add(1);
                    }
                    self.next = Some(next);
                    self.stats.skipped += drop;
                    buffered -= drop;
                }
                // the fill average from before the gap means nothing now
                self.fill_avg = None;
            }
            if buffered == 0 {
                self.primed = false;
                // a late burst, not a broken stream: start again at half the buffer (a short
                // gap); the drift control slowly refills to the target
                self.prime_at = (self.target / 2).max(1);
                self.refill = true;
                self.stats.underruns += 1;
                self.cur_pos = 0.0;
                self.remove_debt = 0;
                self.insert_debt = 0;
                return false;
            }
            if self.trim > 0 {
                // the target was cut: drop the oldest packets over it (never into the
                // target: a burst pattern may have the buffer low just now)
                let over = self.trim.min(buffered.saturating_sub(self.target));
                let drop = self.owe_packets(over);
                for _ in 0..drop {
                    self.slots[Self::idx(next)] = None;
                    next = next.wrapping_add(1);
                }
                self.next = Some(next);
                self.stats.skipped += drop;
                buffered -= drop;
                self.trim = 0;
                if drop > 0 {
                    self.seam = true;
                    self.fill_avg = None;
                }
            }
            self.update_drift(buffered);
            self.next = Some(next.wrapping_add(1));
            let slot = self.slots[Self::idx(next)].take();
            // far over the target (counting what is already being caught up with)
            let held = buffered.saturating_sub(self.remove_debt / self.packet_frames());
            if held > self.target * 2 + 2 && self.owe_packets(held - self.target) > 0 {
                self.stats.skipped += 1;
                self.seam = true;
                continue; // drop this packet to reduce latency
            }
            match slot {
                Some(mut f) => {
                    if self.declick && self.plc && self.concealing && self.plc_on {
                        // back to real sound: crossfade out of the made-up one
                        self.xfade_left = XFADE;
                    } else {
                        self.seam |= self.concealing;
                    }
                    self.concealing = false;
                    if self.xfade_left > 0 {
                        self.crossfade_in(&mut f);
                    }
                    if self.declick && self.plc {
                        self.remember(&f);
                    }
                    self.cur.load(&f);
                    self.last = f;
                    if self.scaling() && self.xfade_left == 0 {
                        self.time_scale();
                    }
                }
                None if self.last.frames == 0 => {
                    // nothing played yet to stand in for it (the stream starts with a gap)
                    self.stats.concealed += 1;
                    continue;
                }
                None if self.declick && self.plc && self.plc_can_continue() => {
                    self.stats.concealed += 1;
                    if !(self.concealing && self.plc_on) {
                        // the made-up sound starts here (a run of losses continues it)
                        self.plc_start();
                        self.seam = true;
                    }
                    self.concealing = true;
                    self.plc_frame(self.last.frames as usize);
                }
                None => {
                    self.stats.concealed += 1;
                    self.seam = true;
                    self.concealing = true;
                    self.last.data.iter_mut().for_each(|s| *s *= 0.5);
                    let half = self.last;
                    self.cur.load(&half);
                }
            }
            self.cur_valid = true;
            return true;
        }
    }
}

/// Frames weighed for one resampled frame (half before the read position, half after).
/// 4 (a cubic) lost 0.5 dB at 10 kHz and 2.5 dB at 15 kHz when the position fell between
/// two samples, and nothing when on one: with the clocks a few ppm apart the position
/// slides, so the highs wobbled once a second or so. See `examples/quality.rs`.
const TAPS: usize = 24;
const HALF: usize = TAPS / 2;
const TAIL: usize = HALF - 1;
/// Positions between two samples the weights are tabulated for (and blended between).
const PHASES: usize = 256;
const KAISER_BETA: f64 = 8.0;

/// The weights: row `p` (of `PHASES + 1`) for a read position `p / PHASES` past a sample.
fn sinc_table() -> &'static [f32] {
    static TABLE: std::sync::OnceLock<Box<[f32]>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        // modified Bessel function I0 (for the Kaiser window)
        let i0 = |x: f64| {
            let (mut sum, mut term, mut k) = (1.0f64, 1.0f64, 1.0f64);
            while term > 1e-12 * sum {
                term *= (x / (2.0 * k)).powi(2);
                sum += term;
                k += 1.0;
            }
            sum
        };
        let mut t = vec![0f32; (PHASES + 1) * TAPS];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let mut row = [0f64; TAPS];
            for (j, w) in row.iter_mut().enumerate() {
                let d = j as f64 - (HALF as f64 - 1.0) - frac;
                let x = (d / HALF as f64).clamp(-1.0, 1.0);
                let window = i0(KAISER_BETA * (1.0 - x * x).sqrt()) / i0(KAISER_BETA);
                let sinc = if d.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d) };
                *w = sinc * window;
            }
            // a steady level stays what it is
            let sum: f64 = row.iter().sum();
            for (j, w) in row.iter().enumerate() {
                t[p * TAPS + j] = (w / sum) as f32;
            }
        }
        t.into_boxed_slice()
    })
}

/// The frame at `frac` in [0, 1) past `taps[HALF - 1]`.
#[inline]
fn interpolate(table: &[f32], taps: &[(f32, f32); TAPS], frac: f32) -> (f32, f32) {
    let pos = frac * PHASES as f32;
    let p = (pos as usize).min(PHASES - 1);
    let w = pos - p as f32;
    let (a, b) = table[p * TAPS..(p + 2) * TAPS].split_at(TAPS);
    let (mut l, mut r) = (0f32, 0f32);
    for j in 0..TAPS {
        let c = a[j] + (b[j] - a[j]) * w;
        l += taps[j].0 * c;
        r += taps[j].1 * c;
    }
    (l, r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32, value: f32) -> Frame {
        let mut f = Frame { seq, channels: 2, frames: 4, ..Frame::EMPTY };
        f.data[..8].fill(value);
        f
    }

    /// Smoothing off: these tests check the packet logic with step signals.
    fn raw(mut jb: JitterBuffer) -> JitterBuffer {
        jb.declick = false;
        jb
    }

    /// A 440 Hz tone in `pf`-frame mono packets (amplitude 0.8), starting at packet `seq`.
    fn tone(seq: u32, pf: usize) -> Frame {
        let mut f = Frame { seq, channels: 1, frames: pf as u16, ..Frame::EMPTY };
        for k in 0..pf {
            let n = seq as usize * pf + k;
            f.data[k] = 0.8 * (std::f32::consts::TAU * 440.0 * n as f32 / 48_000.0).sin();
        }
        f
    }

    /// The biggest step between neighbouring samples (left channel). A 440 Hz tone at 0.8
    /// moves at most ~0.046 per sample; a click is a step far beyond that.
    fn biggest_step(out: &[f32]) -> f32 {
        let l: Vec<f32> = out.iter().step_by(2).copied().collect();
        l.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn a_lost_packet_does_not_click() {
        for declick in [false, true] {
            let mut jb = JitterBuffer::new(2);
            jb.declick = declick;
            jb.set_drift_correction(false);
            let mut out = Vec::new();
            for seq in 0..40u32 {
                if seq != 17 && seq != 25 && seq != 26 {
                    jb.push(tone(seq, 32)); // three lost (one alone, two in a row)
                }
                let mut buf = [0f32; 64];
                jb.read_stereo(&mut buf);
                out.extend_from_slice(&buf);
            }
            // three gone: concealed, or (the two in a row ran it dry) skipped on refilling
            let st = jb.stats();
            assert_eq!(st.concealed + st.skipped, 3, "{st:?}");
            let step = biggest_step(&out);
            println!("declick {declick}: biggest step {step:.3}");
            if declick {
                assert!(step < 0.12, "click: step {step:.3}");
            } else {
                assert!(step > 0.2, "the test signal should click without smoothing ({step:.3})");
            }
        }
    }

    /// A lost packet in a steady tone is filled with the tone itself (pitch repetition),
    /// not a buzz or a dip.
    #[test]
    fn a_lost_packet_is_filled_with_the_tone() {
        for (pitch, min_snr) in [(false, 0.0), (true, 25.0)] {
            let mut jb = JitterBuffer::new(2);
            jb.set_drift_correction(false);
            jb.set_concealment(pitch);
            let mut out: Vec<f32> = Vec::new();
            for seq in 0..80u32 {
                if seq != 50 {
                    jb.push(tone(seq, 32));
                }
                let mut buf = [0f32; 64];
                jb.read_stereo(&mut buf);
                out.extend(buf.iter().step_by(2));
            }
            // the delay is constant (nothing ran dry): find it from a packet before the loss
            let want = |n: usize| 0.8 * (std::f32::consts::TAU * 440.0 * n as f32 / 48_000.0).sin();
            let d = (0..400).min_by_key(|&d| ((0..64).map(|k| (out[1000 + k] - want(1000 + k - d)).abs()).sum::<f32>() * 1e6) as u64).unwrap();
            let (mut sig, mut err) = (0f32, 0f32);
            for n in 50 * 32..51 * 32 {
                sig += want(n).powi(2);
                err += (out[n + d] - want(n)).powi(2);
            }
            let snr = 10.0 * (sig / err.max(1e-12)).log10();
            println!("pitch {pitch}: SNR {snr:.1} dB over the lost packet");
            if pitch {
                assert!(snr > min_snr, "SNR {snr:.1} dB");
            }
        }
    }

    /// The buffer runs dry for 10 ms in the middle of a tone: the tone goes on (no gap),
    /// and the real sound comes back without a click.
    #[test]
    fn running_dry_keeps_the_sound_going() {
        let mut jb = JitterBuffer::new(2);
        jb.set_drift_correction(false);
        let mut out: Vec<f32> = Vec::new();
        let mut held = Vec::new();
        for seq in 0..60u32 {
            // packets 30..45 (10 ms) arrive late, all at once
            if (30..45).contains(&seq) {
                held.push(tone(seq, 32));
            } else {
                jb.push(tone(seq, 32));
            }
            if seq == 45 {
                held.drain(..).for_each(|f| jb.push(f));
            }
            let mut buf = [0f32; 64];
            jb.read_stereo(&mut buf);
            out.extend(buf.iter().step_by(2));
        }
        assert!(jb.stats().underruns >= 1, "{:?}", jb.stats());
        // no 1 ms stretch near silent once the tone has started
        let quiet = out[400..].chunks(48).filter(|w| w.iter().map(|x| x * x).sum::<f32>() / 48.0 < 0.01).count();
        assert_eq!(quiet, 0, "the tone stopped for {quiet} ms");
        let step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        assert!(step < 0.12, "click: step {step:.3}");
    }

    #[test]
    fn running_dry_fades_out_and_back_in() {
        let mut jb = JitterBuffer::new(1);
        jb.set_drift_correction(false);
        let mut out = Vec::new();
        for seq in 0..30u32 {
            // the sender stalls for 6 packets in the middle of a loud tone
            if !(12..18).contains(&seq) {
                jb.push(tone(seq, 32));
            }
            let mut buf = [0f32; 64];
            jb.read_stereo(&mut buf);
            out.extend_from_slice(&buf);
        }
        assert!(jb.stats().underruns >= 1);
        let step = biggest_step(&out);
        assert!(step < 0.12, "click: step {step:.3}");
    }

    fn read(jb: &mut JitterBuffer, frames: usize) -> Vec<f32> {
        let mut out = vec![9.0; frames * 2];
        jb.read_stereo(&mut out);
        out.iter().step_by(2).copied().collect()
    }

    #[test]
    fn waits_for_target_then_plays_in_order() {
        let mut jb = raw(JitterBuffer::new(2));
        jb.push(frame(10, 1.0));
        assert_eq!(read(&mut jb, 4), vec![0.0; 4], "not primed yet");
        jb.push(frame(11, 2.0));
        assert_eq!(read(&mut jb, 8), [vec![1.0; 4], vec![2.0; 4]].concat());
    }

    /// A WASAPI sender delivers 15 packets every 10 ms; we read 64 frames at a time.
    /// Returns (underruns, skipped) over ~10 s.
    fn bursty(target: u32) -> (u32, u32) {
        let mut jb = JitterBuffer::new(target);
        let mut seq = 0u32;
        let mut out = vec![0.0f32; 64 * 2];
        let packet = |seq: u32| {
            let mut f = Frame { seq, channels: 2, frames: 32, ..Frame::EMPTY };
            f.data[..64].fill(0.5);
            f
        };
        // time in frames: a burst of 480 frames every 480, reads of 64 every 64
        let (mut next_burst, mut next_read) = (0u64, 0u64);
        while next_read < 48_000 * 10 {
            if next_burst <= next_read {
                for _ in 0..15 {
                    jb.push(packet(seq));
                    seq += 1;
                }
                next_burst += 480;
            } else {
                jb.read_stereo(&mut out);
                next_read += 64;
            }
        }
        let st = jb.stats();
        (st.underruns, st.skipped)
    }

    #[test]
    fn bridges_wasapi_bursts_with_a_burst_sized_target() {
        // what the averaged jitter suggested (~5): each burst overfills past 2×target+2,
        // packets get dropped, and the buffer runs dry before the next burst — every 10 ms
        let (under, _) = bursty(5);
        assert!(under >= 400, "{under}");
        // what auto now picks for a 9.3 ms spread: ceil((9.33 + 0.5) / 0.667) + 1 = 16
        assert_eq!(bursty(16), (0, 0));
    }

    /// Steady sender (one 32-frame packet per 32 frames read); the target moves up and down.
    #[test]
    fn target_changes_settle_without_sticking_the_pitch_correction() {
        let mut jb = JitterBuffer::new(4);
        let mut out = vec![0.0f32; 32 * 2];
        let mut seq = 0u32;
        let mut run = |jb: &mut JitterBuffer, packets: u32| {
            for _ in 0..packets {
                let mut f = Frame { seq, channels: 2, frames: 32, ..Frame::EMPTY };
                f.data[..64].fill(0.5);
                jb.push(f);
                seq += 1;
                jb.read_stereo(&mut out);
            }
        };
        run(&mut jb, 1500 * 5); // 5 s at target 4
        jb.set_target(12); // much bigger: one short refill, no long -1000 ppm crawl
        run(&mut jb, 1500 * 10);
        let up = jb.stats();
        assert!(up.drift_ppm.abs() < 300, "after raising: {up:?}");
        assert!((up.depth as i64 - 12).abs() <= 2, "after raising: {up:?}");
        jb.set_target(8); // coming down: the drift control removes the extra packets
        run(&mut jb, 1500 * 20);
        let down = jb.stats();
        assert!(down.drift_ppm.abs() < 300, "after lowering: {down:?}");
        assert!((down.depth as i64 - 8).abs() <= 2, "after lowering: {down:?}");
        assert!(down.underruns <= 1, "{down:?}");
    }

    #[test]
    fn follows_a_sender_that_restarted_its_stream() {
        let mut jb = raw(JitterBuffer::new(2));
        for seq in 5000..5010 {
            jb.push(frame(seq, 1.0));
        }
        assert_eq!(read(&mut jb, 8), vec![1.0; 8]);
        // the sender changed its audio settings: numbering starts again at 0
        for seq in 0..4 {
            jb.push(frame(seq, 2.0));
        }
        assert_eq!(read(&mut jb, 8), vec![2.0; 8], "plays the new stream");
        assert_eq!(jb.stats().late, 0);
    }

    #[test]
    fn reorders() {
        let mut jb = JitterBuffer::new(3);
        jb.push(frame(0, 1.0));
        jb.push(frame(2, 3.0));
        jb.push(frame(1, 2.0));
        assert_eq!(read(&mut jb, 12), [vec![1.0; 4], vec![2.0; 4], vec![3.0; 4]].concat());
    }

    #[test]
    fn conceals_loss_with_decaying_repeat() {
        let mut jb = raw(JitterBuffer::new(1));
        jb.push(frame(0, 1.0));
        jb.push(frame(2, 3.0)); // 1 is lost
        assert_eq!(read(&mut jb, 12), [vec![1.0; 4], vec![0.5; 4], vec![3.0; 4]].concat());
        assert_eq!(jb.stats().concealed, 1);
    }

    #[test]
    fn drops_late_packets_and_counts_underruns() {
        let mut jb = raw(JitterBuffer::new(1));
        jb.push(frame(5, 1.0));
        read(&mut jb, 4);
        jb.push(frame(4, 9.0)); // already played past it
        assert_eq!(jb.stats().late, 1);
        assert_eq!(read(&mut jb, 4), vec![0.0; 4]);
        assert_eq!(jb.stats().underruns, 1);
    }

    #[test]
    fn skips_when_far_over_target() {
        let mut jb = raw(JitterBuffer::new(1));
        for s in 0..8 {
            jb.push(frame(s, s as f32));
        }
        read(&mut jb, 4);
        assert!(jb.stats().skipped > 0);
        assert!(jb.stats().depth <= 4);
    }

    /// Far over the target with real sound: nothing is dropped, the extra is played faster
    /// (periods folded) until the delay is back, without a click.
    #[test]
    fn catches_up_by_playing_faster() {
        let mut jb = JitterBuffer::new(3);
        jb.set_drift_correction(false);
        let mut out: Vec<f32> = Vec::new();
        let mut buf = [0f32; 128]; // 64 frames a read, two packets
        let mut seq = 0u32;
        let mut feed = |jb: &mut JitterBuffer, n: u32| {
            for _ in 0..n {
                jb.push(tone(seq, 32));
                seq += 1;
            }
        };
        for _ in 0..40 {
            feed(&mut jb, 2);
            jb.read_stereo(&mut buf);
            out.extend(buf.iter().step_by(2));
        }
        feed(&mut jb, 30); // a stall's worth lands at once (20 ms)
        for _ in 0..300 {
            feed(&mut jb, 2);
            jb.read_stereo(&mut buf);
            out.extend(buf.iter().step_by(2));
        }
        let st = jb.stats();
        assert_eq!(st.skipped, 0, "{st:?}");
        assert!(st.scaled >= 2, "{st:?}");
        assert!(st.depth <= 3 * 2 + 2, "the delay came back: {st:?}");
        let step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        assert!(step < 0.12, "click: step {step:.3}");
    }

    #[test]
    fn converts_44k1_to_48k_cleanly() {
        // 1 kHz sine recorded at 44.1 kHz, played back at 48 kHz
        let (src_sr, dst_sr, f) = (44_100.0f64, 48_000.0f64, 1000.0f64);
        let mut jb = JitterBuffer::new(20); // all 40 packets fit without skip-ahead
        jb.set_drift_correction(false);
        jb.set_base_ratio(src_sr / dst_sr);
        let pf = 32usize;
        let mut n = 0usize;
        let mut push = |jb: &mut JitterBuffer, seq: u32| {
            let mut fr = Frame { seq, channels: 1, frames: pf as u16, ..Frame::EMPTY };
            for k in 0..pf {
                fr.data[k] = (std::f64::consts::TAU * f * (n + k) as f64 / src_sr).sin() as f32;
            }
            n += pf;
            jb.push(fr);
        };
        for s in 0..40 {
            push(&mut jb, s);
        }
        let mut out = vec![0.0; 2 * 1000];
        jb.read_stereo(&mut out);
        // compare with the ideal 1 kHz sine at 48 kHz (same phase origin)
        let mut err = 0.0f64;
        let mut sig = 0.0f64;
        for (k, s) in out.iter().step_by(2).enumerate() {
            let ideal = (std::f64::consts::TAU * f * k as f64 / dst_sr).sin();
            err += (*s as f64 - ideal).powi(2);
            sig += ideal * ideal;
        }
        let snr_db = 10.0 * (sig / err).log10();
        assert!(snr_db > 60.0, "SNR {snr_db:.1} dB");
    }

    /// The clocks a little apart (the read position slides between samples): a high tone
    /// comes out as it went in, at a steady level.
    #[test]
    fn a_high_tone_survives_the_drift_correction() {
        let (sr, f, ratio) = (48_000.0f64, 12_000.0f64, 1.0002f64);
        let mut jb = JitterBuffer::new(4);
        jb.set_drift_correction(false);
        jb.set_base_ratio(ratio);
        let (mut n, mut seq) = (0usize, 0u32);
        let mut out: Vec<f64> = Vec::new();
        let mut buf = [0f32; 128];
        while out.len() < 48_000 {
            while (n as f64) < (out.len() + 64) as f64 * ratio + 192.0 {
                let mut fr = Frame { seq, channels: 1, frames: 32, ..Frame::EMPTY };
                for k in 0..32 {
                    fr.data[k] = (0.5 * (std::f64::consts::TAU * f * (n + k) as f64 / sr).sin()) as f32;
                }
                n += 32;
                seq += 1;
                jb.push(fr);
            }
            jb.read_stereo(&mut buf);
            out.extend(buf.iter().step_by(2).map(|&s| s as f64));
        }
        // level over 10 ms stretches (at 12 kHz: 4 samples a cycle, so sum of squares)
        let levels: Vec<f64> = out[4800..].as_chunks::<480>().0.iter().map(|c| (c.iter().map(|v| v * v).sum::<f64>() / 240.0).sqrt()).collect();
        let (lo, hi) = levels.iter().fold((f64::MAX, 0f64), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        assert!((lo - 0.5).abs() < 0.005 && (hi - 0.5).abs() < 0.005, "level {lo:.4}..{hi:.4}");
    }

    #[test]
    fn handles_sequence_wraparound() {
        let mut jb = JitterBuffer::new(2);
        jb.push(frame(u32::MAX, 1.0));
        jb.push(frame(0, 2.0));
        assert_eq!(read(&mut jb, 8), [vec![1.0; 4], vec![2.0; 4]].concat());
    }

    /// The sender's clock runs `ppm` fast (negative = slow). Plays `secs` of audio with
    /// 32-frame packets and 64-frame output callbacks.
    fn simulate_drift(ppm: f64, secs: f64, correct: bool) -> JitterStats {
        let (sr, pf, block) = (48_000.0, 32usize, 64usize);
        let mut jb = JitterBuffer::new(3);
        jb.set_drift_correction(correct);
        let mut out = vec![0.0; block * 2];
        let (mut sent, mut t_send, mut t_play) = (0u32, 0.0f64, 0.0f64);
        let packet_s = pf as f64 / sr / (1.0 + ppm * 1e-6);
        while t_play < secs {
            while t_send <= t_play {
                let mut f = Frame { seq: sent, channels: 1, frames: pf as u16, ..Frame::EMPTY };
                f.data[..pf].fill(0.25);
                jb.push(f);
                sent += 1;
                t_send += packet_s;
            }
            jb.read_stereo(&mut out);
            t_play += block as f64 / sr;
        }
        jb.stats()
    }

    #[test]
    fn drift_correction_holds_the_buffer_at_target() {
        for ppm in [300.0, -300.0] {
            let st = simulate_drift(ppm, 120.0, true);
            assert_eq!(st.skipped, 0, "{ppm} ppm: {st:?}");
            assert!(st.underruns <= 1, "{ppm} ppm: {st:?}");
            assert!((1..=6).contains(&st.depth), "{ppm} ppm: {st:?}");
            assert_eq!(st.drift_ppm.signum(), (ppm as i32).signum(), "{ppm} ppm: {st:?}");
        }
    }

    #[test]
    fn small_or_no_drift_stays_stable() {
        for ppm in [0.0, 50.0, -50.0] {
            let st = simulate_drift(ppm, 300.0, true);
            assert_eq!(st.skipped, 0, "{ppm} ppm: {st:?}");
            assert!(st.underruns <= 1, "{ppm} ppm: {st:?}");
            assert!(st.drift_ppm.abs() <= 300, "{ppm} ppm: {st:?}");
        }
        // without drift the controller only nudges the fill into its dead band
        assert!(simulate_drift(0.0, 60.0, true).drift_ppm.abs() <= 20, "barely any correction without drift");
    }

    #[test]
    fn without_correction_drift_causes_glitches() {
        // sanity check for the test above: the same scenario does go wrong uncorrected
        let fast = simulate_drift(300.0, 120.0, false);
        let slow = simulate_drift(-300.0, 120.0, false);
        assert!(fast.skipped + fast.scaled > 0, "{fast:?}");
        assert!(slow.underruns > 1, "{slow:?}");
    }

    #[test]
    fn interpolation_is_smooth_across_packets() {
        // a ramp split into packets must come out monotonic while resampling
        let mut jb = JitterBuffer::new(12); // high target: no skip-ahead in this test
        jb.ratio = 1.0007;
        jb.drift = false;
        for p in 0..20u32 {
            let mut f = Frame { seq: p, channels: 1, frames: 32, ..Frame::EMPTY };
            for i in 0..32 {
                f.data[i] = (p * 32 + i as u32) as f32;
            }
            jb.push(f);
        }
        let mut out = vec![0.0; 2 * 500];
        jb.read_stereo(&mut out);
        let l: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!(l.windows(2).all(|w| w[1] > w[0] && w[1] - w[0] < 1.01), "not smooth");
    }
}
