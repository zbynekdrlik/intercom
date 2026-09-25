//! The Janus leg's own 20 ms clock (issue 1345, 25.9.2026: the phone voice sounds robotic).
//!
//! The hub mixes in 256-frame (5.33 ms) blocks. The old Janus sender emitted a 20 ms packet whenever
//! 960 frames had piled up, i.e. every 3.75 blocks. Packets therefore left on a 16/21 ms beat
//! (measured on strih-lx: sd 2.29 ms, min 15.3, max 22.4 ms). The Janus audiobridge mixes each
//! participant every 20 ms from a small buffer, so that beat made it conceal.
//!
//! Now the block loop only feeds a [`PacedRing`], and a sender with its own monotonic
//! [`PaceSchedule`] pops exactly one [`FRAME_48K`] frame per [`TICK`]:
//!
//! - a short underflow is bridged with a whole silent frame, then the ring re-primes to its target
//!   (never a partial zero-splice);
//! - a fill that drifts (a missed mix tick, a rate offset) is pulled back by a gentle 1 ms/s
//!   servo, long before either edge case below;
//! - an overflow trims the oldest samples back to the target;
//! - one packet per tick keeps the RTP timestamps contiguous by construction.
//!
//! [`hub_block_period`] is the block loop's exact period, shared by `main.rs` and the tests.
//!
//! [`IntervalStats`] measures the real send spacing for the `/api/state` janus facet. [`rx_gap`] is
//! the receive-side loss plan the Opus decoder uses (FEC for the frame right before a packet, PLC
//! for earlier ones).
//!
//! Pure and std-only, so it verifies with a rustc `--test` replica under Tier-0 (issue 557).

use std::collections::VecDeque;
use std::time::Duration;

/// One 20 ms frame of 48 kHz mono audio: the unit every Janus packet carries.
pub const FRAME_48K: usize = 960;

/// The sender's tick: one packet per 20 ms.
pub const TICK: Duration = Duration::from_millis(20);

/// The fill the ring waits for before it hands out audio (at start and after an underflow): two
/// frames. Just before a pop the fill then sits around the target, so a mix block that is late by
/// up to ~5 ms, or a sender tick that catches up a short stall, still finds a whole frame.
pub const RING_TARGET_FRAMES: usize = 2 * FRAME_48K;

/// Above this fill the oldest samples are trimmed back to [`RING_TARGET_FRAMES`] (100 ms). It is
/// only reached when the sender stalled for a long time.
pub const RING_CAP_FRAMES: usize = 5 * FRAME_48K;

/// A sender that falls further behind its grid than this skips to a fresh grid instead of firing
/// the missed packets back to back.
pub const MAX_LAG: Duration = Duration::from_millis(100);

/// The longest sequence gap the receiver conceals (FEC + PLC). A longer gap is a restart.
pub const MAX_CONCEAL_FRAMES: u16 = 5;

/// The largest backward sequence step still treated as a late (reordered) packet (RFC 3550 A.1
/// `MAX_MISORDER`). A bigger backward jump is a new sender or a stray packet: a restart, so one
/// bad packet can never lock the real stream out.
pub const MAX_MISORDER: u16 = 100;

/// The fill servo acts only after the pre-pop fill has stayed past a threshold for this many
/// consecutive ticks (1 s): the normal 256-sample ripple of the block feed never triggers it.
pub const SERVO_TICKS: u32 = 50;

/// What one servo action moves: 1 ms of audio, dropped or repeated inside a frame.
pub const SERVO_STEP_FRAMES: usize = 48;

/// The hub block loop's period, exact to the nanosecond. Whole microseconds are NOT exact:
/// 256 frames at 48 kHz is 5333.33 µs, and the old `5333 µs` made the loop run 62.5 ppm fast, so
/// every egress gained ~3 samples/s against a 48 kHz consumer. Zero for a zero rate.
pub fn hub_block_period(block_frames: usize, sample_rate: u32) -> Duration {
    if sample_rate == 0 {
        return Duration::ZERO;
    }
    let nanos = block_frames as u128 * 1_000_000_000 / u128::from(sample_rate);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// What [`PacedRing::pop_frame`] handed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopKind {
    /// A whole frame of real audio.
    Audio,
    /// Silence while the ring fills up to its target (start-up, or after an underflow).
    Priming,
    /// Silence because the ring ran dry mid-stream (counted in [`PacedRing::underflows`]).
    Underflow,
}

/// What the fill servo decided before a pop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServoAction {
    None,
    /// The fill sat high for a second: drop 1 ms of the oldest audio.
    Drop,
    /// The fill sat low for a second: take 1 ms less and repeat the frame's last millisecond.
    Repeat,
}

/// The ring between the block loop (push, 256 frames every 5.33 ms) and the paced sender (pop,
/// 960 frames every 20 ms). Mono 48 kHz samples.
///
/// The two sides run on the same monotonic clock, but a missed mix tick loses a block and any
/// rounding in the block period is a slow rate offset. So a gentle fill servo keeps the pre-pop
/// fill inside a band: after [`SERVO_TICKS`] pops in a row above `target + FRAME/2` it drops
/// [`SERVO_STEP_FRAMES`]; after as many below `target - FRAME/4` it repeats 1 ms inside the frame.
/// The 60 ms trim and the silent underflow frame stay as the last resort.
#[derive(Debug, Clone)]
pub struct PacedRing {
    buf: VecDeque<i16>,
    target: usize,
    cap: usize,
    primed: bool,
    underflows: u64,
    trims: u64,
    high_ticks: u32,
    low_ticks: u32,
    servo_drops: u64,
    servo_repeats: u64,
}

impl PacedRing {
    /// A ring that primes to `target` samples and trims back to it above `cap`.
    pub fn new(target: usize, cap: usize) -> Self {
        let target = target.max(FRAME_48K);
        PacedRing {
            buf: VecDeque::with_capacity(cap.max(target) + FRAME_48K),
            target,
            cap: cap.max(target + FRAME_48K),
            primed: false,
            underflows: 0,
            trims: 0,
            high_ticks: 0,
            low_ticks: 0,
            servo_drops: 0,
            servo_repeats: 0,
        }
    }

    /// Track the pre-pop fill against the servo band and decide this pop's correction.
    fn servo(&mut self, fill: usize) -> ServoAction {
        if fill > self.target + FRAME_48K / 2 {
            self.high_ticks += 1;
            self.low_ticks = 0;
        } else if fill < self.target - FRAME_48K / 4 {
            self.low_ticks += 1;
            self.high_ticks = 0;
        } else {
            self.high_ticks = 0;
            self.low_ticks = 0;
        }
        if self.high_ticks >= SERVO_TICKS {
            self.high_ticks = 0;
            ServoAction::Drop
        } else if self.low_ticks >= SERVO_TICKS {
            self.low_ticks = 0;
            ServoAction::Repeat
        } else {
            ServoAction::None
        }
    }

    /// Append mixed samples. Above the cap, drop the OLDEST samples back to the target.
    pub fn push(&mut self, mono: &[i16]) {
        self.buf.extend(mono.iter().copied());
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.target;
            self.buf.drain(..excess);
            self.trims += 1;
        }
    }

    /// Take exactly one [`FRAME_48K`] frame. Silence while priming or on an underflow; an underflow
    /// keeps any partial samples and re-primes to the target.
    pub fn pop_frame(&mut self) -> (Vec<i16>, PopKind) {
        if !self.primed {
            if self.buf.len() < self.target {
                return (vec![0; FRAME_48K], PopKind::Priming);
            }
            self.primed = true;
        }
        if self.buf.len() >= FRAME_48K {
            match self.servo(self.buf.len()) {
                ServoAction::None => {}
                ServoAction::Drop => {
                    // Above the high threshold, so a whole frame is still left after the drop.
                    self.buf.drain(..SERVO_STEP_FRAMES);
                    self.servo_drops += 1;
                }
                ServoAction::Repeat => {
                    let mut frame: Vec<i16> =
                        self.buf.drain(..FRAME_48K - SERVO_STEP_FRAMES).collect();
                    let tail_start = frame.len() - SERVO_STEP_FRAMES;
                    frame.extend_from_within(tail_start..);
                    self.servo_repeats += 1;
                    return (frame, PopKind::Audio);
                }
            }
            return (self.buf.drain(..FRAME_48K).collect(), PopKind::Audio);
        }
        self.primed = false;
        self.high_ticks = 0;
        self.low_ticks = 0;
        self.underflows += 1;
        (vec![0; FRAME_48K], PopKind::Underflow)
    }

    /// Empty the ring and re-prime.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.primed = false;
        self.high_ticks = 0;
        self.low_ticks = 0;
    }

    /// 1 ms drops the servo made because the fill sat high.
    pub fn servo_drops(&self) -> u64 {
        self.servo_drops
    }

    /// 1 ms repeats the servo made because the fill sat low.
    pub fn servo_repeats(&self) -> u64 {
        self.servo_repeats
    }

    /// Samples currently buffered.
    pub fn fill(&self) -> usize {
        self.buf.len()
    }

    /// Mid-stream underflows bridged with silence.
    pub fn underflows(&self) -> u64 {
        self.underflows
    }

    /// Overflow trims back to the target.
    pub fn trims(&self) -> u64 {
        self.trims
    }
}

/// The sender's deadlines: exact multiples of [`TICK`] from its start, so the grid never drifts
/// however late a wake-up is. Times are durations since the sender's own monotonic origin.
#[derive(Debug, Clone)]
pub struct PaceSchedule {
    next: Duration,
    resyncs: u64,
}

impl Default for PaceSchedule {
    fn default() -> Self {
        Self::new()
    }
}

impl PaceSchedule {
    /// The first packet is due one tick after the origin.
    pub fn new() -> Self {
        PaceSchedule {
            next: TICK,
            resyncs: 0,
        }
    }

    /// How long to sleep before the next packet is due (zero = send now).
    pub fn wait(&self, now: Duration) -> Duration {
        self.next.saturating_sub(now)
    }

    /// A packet was sent at `now`: move to the next deadline. A short stall is caught up on the
    /// same grid; a stall longer than [`MAX_LAG`] starts a fresh grid one tick after `now`.
    pub fn advance(&mut self, now: Duration) {
        self.next += TICK;
        if now > self.next + MAX_LAG {
            self.next = now + TICK;
            self.resyncs += 1;
        }
    }

    /// How many times a long stall restarted the grid.
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }
}

/// The spacing of the last `window` sends: the pacing proof on `/api/state`.
#[derive(Debug, Clone)]
pub struct IntervalStats {
    last: Option<Duration>,
    intervals_ms: VecDeque<f64>,
    window: usize,
}

impl IntervalStats {
    /// Keep the last `window` intervals (250 = 5 s at 20 ms).
    pub fn new(window: usize) -> Self {
        let window = window.max(1);
        IntervalStats {
            last: None,
            intervals_ms: VecDeque::with_capacity(window),
            window,
        }
    }

    /// A packet left at `at` (monotonic time since the sender's origin).
    pub fn record(&mut self, at: Duration) {
        if let Some(prev) = self.last {
            if self.intervals_ms.len() == self.window {
                self.intervals_ms.pop_front();
            }
            self.intervals_ms
                .push_back(at.saturating_sub(prev).as_secs_f64() * 1000.0);
        }
        self.last = Some(at);
    }

    /// Forget the last timestamp and the window (a new session: the join gap is not an interval).
    pub fn reset(&mut self) {
        self.last = None;
        self.intervals_ms.clear();
    }

    /// Population standard deviation of the intervals in ms (0 with fewer than two).
    pub fn sd_ms(&self) -> f64 {
        let n = self.intervals_ms.len();
        if n < 2 {
            return 0.0;
        }
        let mean = self.intervals_ms.iter().sum::<f64>() / n as f64;
        let var = self
            .intervals_ms
            .iter()
            .map(|x| (x - mean) * (x - mean))
            .sum::<f64>()
            / n as f64;
        var.sqrt()
    }

    /// The longest interval in the window in ms (0 when empty).
    pub fn max_ms(&self) -> f64 {
        self.intervals_ms.iter().copied().fold(0.0, f64::max)
    }
}

/// How a received RTP sequence number relates to the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxGap {
    /// The first packet of a session.
    First,
    /// The next packet in order.
    InOrder,
    /// `n` packets are missing just before this one (1..=[`MAX_CONCEAL_FRAMES`]): conceal them.
    Lost(u16),
    /// A gap too long to conceal, or a backward jump beyond [`MAX_MISORDER`]: decode this packet
    /// as a fresh start.
    Resync,
    /// A duplicate or a late, reordered packet (at most [`MAX_MISORDER`] back): drop it.
    Stale,
}

/// Classify `seq` against the last accepted sequence number (16-bit wrap aware).
pub fn rx_gap(last: Option<u16>, seq: u16) -> RxGap {
    let Some(last) = last else {
        return RxGap::First;
    };
    match seq.wrapping_sub(last) {
        0 => RxGap::Stale,
        1 => RxGap::InOrder,
        d if d - 1 <= MAX_CONCEAL_FRAMES => RxGap::Lost(d - 1),
        d if d >= 0u16.wrapping_sub(MAX_MISORDER) => RxGap::Stale,
        _ => RxGap::Resync,
    }
}
