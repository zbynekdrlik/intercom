//! The Janus leg's own 20 ms clock (issue 1345, 25.9.2026: the phone voice sounds robotic).
//!
//! The hub mixes in 256-frame (5.33 ms) blocks. The old Janus sender emitted a 20 ms packet whenever
//! 960 frames had piled up, i.e. every 3.75 blocks. Packets therefore left on a 16/21 ms beat
//! (measured on strih-snv: sd 2.29 ms, min 15.3, max 22.4 ms). The Janus audiobridge mixes each
//! participant every 20 ms from a small buffer, so that beat made it conceal.
//!
//! Now the block loop only feeds a [`PacedRing`], and a sender with its own monotonic
//! [`PaceSchedule`] pops exactly one [`FRAME_48K`] frame per [`TICK`]:
//!
//! - the ring is a resampler leg (issue 1461 lane 2): the same frame-count controller as a VBAN
//!   leg ([`crate::vban_jitter::NetworkFill`] over [`crate::asrc`]) with a fixed target, so a fill
//!   that drifts (a lost mix tick, a rate offset between the block loop's grid and the sender's)
//!   is followed by the ratio, never by a 1 ms splice;
//! - a short underflow is bridged with a whole silent frame, then the ring re-primes to its target
//!   (never a partial zero-splice);
//! - an overflow above the cap is one counted resync back to the target;
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
use std::time::{Duration, Instant};

use crate::asrc::{self, ReadPoint};
use crate::vban_jitter::{NetworkFill, NetworkFillStats, PopPlan};

/// One 20 ms frame of 48 kHz mono audio: the unit every Janus packet carries.
pub const FRAME_48K: usize = 960;

/// The sender's tick: one packet per 20 ms.
pub const TICK: Duration = Duration::from_millis(20);

/// The fill the ring waits for before it hands out audio (at start and after an underflow): two
/// frames. Just before a pop the fill then sits around the target, so a mix block that is late by
/// up to ~5 ms, or a sender tick that catches up a short stall, still finds a whole frame.
pub const RING_TARGET_FRAMES: usize = 2 * FRAME_48K;

/// Above this fill the ring resyncs back to [`RING_TARGET_FRAMES`] (100 ms). It is only reached
/// when the sender stalled for a long time.
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

/// The ring between the block loop (push, 256 frames every 5.33 ms) and the paced sender (pop,
/// 960 frames every 20 ms). Mono 48 kHz samples.
///
/// The two sides run on the same monotonic clock, but a lost mix tick (since issue 1401 only the
/// part of a late wake beyond the four ticks the hub-mix thread runs late) loses a block and any
/// rounding in the block period is a slow rate offset. Since issue 1461 the ring crosses that the
/// way every hub input crosses its sender's clock: a [`NetworkFill`] resampler leg at a fixed
/// target ([`RING_TARGET_FRAMES`]). Every push is a point of its rate fit, timed by the moment the
/// block left the mix ([`PacedRing::push_at`], stamped before the ring's lock); every pop renders
/// exactly one frame from a fractional read point. The silent underflow frame and the resync above
/// the cap stay as the last resort, both counted.
#[derive(Debug, Clone)]
pub struct PacedRing {
    buf: VecDeque<i16>,
    fill: NetworkFill,
    /// The pushes' time origin (the first push).
    origin: Option<Instant>,
    underflows: u64,
    trims: u64,
}

impl PacedRing {
    /// A ring that primes to `target` samples and resyncs back to it above `cap`.
    pub fn new(target: usize, cap: usize) -> Self {
        let target = target.max(FRAME_48K);
        let cap = cap.max(target + FRAME_48K);
        PacedRing {
            buf: VecDeque::with_capacity(cap + FRAME_48K),
            fill: NetworkFill::new(target, cap),
            origin: None,
            underflows: 0,
            trims: 0,
        }
    }

    /// Append mixed samples that left the mix at `at` (taken before the ring's lock): one point of
    /// the ring's rate fit. Above the cap, drop the OLDEST samples back to the target (one counted
    /// resync).
    pub fn push_at(&mut self, mono: &[i16], at: Instant) {
        let origin = *self.origin.get_or_insert(at);
        self.fill.note_arrival(
            mono.len(),
            at.saturating_duration_since(origin).as_secs_f64(),
        );
        self.buf.extend(mono.iter().copied());
        if let Some(keep) = self.fill.overrun_keep(self.buf.len()) {
            let excess = self.buf.len() - keep;
            self.buf.drain(..excess);
            self.trims += 1;
        }
    }

    /// Append mixed samples now. See [`PacedRing::push_at`].
    pub fn push(&mut self, mono: &[i16]) {
        self.push_at(mono, Instant::now());
    }

    /// Take exactly one [`FRAME_48K`] frame. Silence while priming or on an underflow (then the
    /// ring re-primes to the target); audio otherwise, rendered by the resampler. See
    /// [`PacedRing::take_frame`] for the split the sender thread uses.
    pub fn pop_frame(&mut self) -> (Vec<i16>, PopKind) {
        let frame = self.take_frame();
        (frame.render(), frame.kind())
    }

    /// Plan one frame and copy out what its render reads, without rendering it: the paced sender
    /// takes this under the ring's lock and renders after releasing it ([`PoppedFrame::render`]),
    /// so the SCHED_FIFO block loop that pushes into the ring never waits on a sinc render (std
    /// mutexes have no priority inheritance; the copy is ~1000 samples).
    pub fn take_frame(&mut self) -> PoppedFrame {
        match self.fill.plan_pop(self.buf.len(), FRAME_48K) {
            PopPlan::Silent { ran_dry } => {
                let kind = if ran_dry {
                    self.underflows += 1;
                    PopKind::Underflow
                } else {
                    PopKind::Priming
                };
                PoppedFrame {
                    kind,
                    read: None,
                    input: Vec::new(),
                }
            }
            PopPlan::Audio {
                skip,
                read,
                consume,
            } => {
                self.buf.drain(..skip.min(self.buf.len()));
                let queued = self.buf.make_contiguous();
                let input = queued[..read.frames_needed(FRAME_48K).min(queued.len())].to_vec();
                self.buf.drain(..consume.min(self.buf.len()));
                PoppedFrame {
                    kind: PopKind::Audio,
                    read: Some(read),
                    input,
                }
            }
        }
    }

    /// Empty the ring and re-prime (a new session). The ratio is kept.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.fill.restart();
        self.origin = None;
    }

    /// The resampler leg's numbers (target, depth, rate, phase, resyncs).
    pub fn stats(&self) -> NetworkFillStats {
        self.fill.stats()
    }

    /// Samples currently buffered.
    pub fn fill(&self) -> usize {
        self.buf.len()
    }

    /// Mid-stream underflows bridged with silence.
    pub fn underflows(&self) -> u64 {
        self.underflows
    }

    /// Resyncs back to the target above the cap.
    pub fn trims(&self) -> u64 {
        self.trims
    }
}

/// One frame [`PacedRing::take_frame`] planned: silence, or the queued samples it reads and the
/// read point to render them from.
#[derive(Debug, Clone)]
pub struct PoppedFrame {
    kind: PopKind,
    read: Option<ReadPoint>,
    input: Vec<i16>,
}

impl PoppedFrame {
    /// What the frame is: audio, priming silence, or an underflow's silence.
    pub fn kind(&self) -> PopKind {
        self.kind
    }

    /// The frame's [`FRAME_48K`] samples: silence, or the resampler's render of the copied input.
    pub fn render(&self) -> Vec<i16> {
        let mut out = vec![0i16; FRAME_48K];
        if let Some(read) = self.read {
            asrc::render(read, &self.input, &mut out);
        }
        out
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
