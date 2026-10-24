//! The fill policy of every hub input that crosses a foreign clock (issue 1401 short dropouts,
//! issue 1461 the resampler at every crossing, design comment 6376769274).
//!
//! The VBAN inputs (the FOH program feed `fohabl-strih`, `lv1-strih`, `mbc`, the seven camboxes'
//! talkback `camN`) and the cutters' card capture are popped by the hub's block loop as one
//! 256-frame block every 5.33 ms of the hub's own clock. Each sender runs on its own clock (the
//! FOH desk, a cambox headset ADC, the card crystal). [`NetworkFill`] is the frame-count
//! controller the buffer ([`crate::vban_io::JitterBuffer`]) delegates to:
//!
//! - **A fixed target, read at start.** Each leg's target depth comes from its TOML `target_ms`
//!   ([`target_frames_from_ms`], rounded up to whole hub blocks) or its kind's default: a cambox
//!   [`VBAN_TARGET_BLOCKS`] (16 ms), a program feed [`VBAN_PROGRAM_TARGET_BLOCKS`] (37.3 ms), the
//!   card capture [`CARD_CAPTURE_TARGET_FRAMES`]. It never moves while the hub runs (the issue-1401
//!   adaptive program target is gone: it moved the cutters' and the vestibule's program latency
//!   while running).
//! - **Prefill.** Nothing is consumed until the queue holds the target plus the resampler's
//!   history; silence goes out meanwhile. Every prime (the first packets, a stream back after
//!   going stale, the re-prime after an underrun) then starts EXACTLY at the target: what piled up
//!   past it is the oldest audio, received while silence went out.
//! - **The crossing is a resampler** ([`crate::asrc`]). Each pop renders exactly one hub block from
//!   a fractional read point, consuming `block x ratio` input frames; the ratio follows the
//!   sender's rate (the least-squares slope of the arrivals over 64 s, followed at 5 ppm/s) plus a
//!   phase term only outside a dead band of +-1/4 of the target. No frame is ever inserted or
//!   dropped while a stream runs.
//! - **Faults are counted, never adapted to.** An underrun = ONE whole silent block and a re-prime
//!   to the SAME target (the buffer counts it). Above the cap = one counted resync back down to
//!   the target ([`NetworkFillStats::resyncs`]).
//! - **A lost hub tick** is answered in kind ([`NetworkFill::discard_for_missed_ticks`]): the block
//!   loop runs up to four missed ticks late ([`crate::block_clock`]); only a tick beyond that is
//!   lost, every output loses its block, and the leg gives up as many blocks of its oldest audio at
//!   once (never going more than half a block under its target), so its latency stays fixed.
//!
//! Pure and std-only, so it verifies with a rustc `--test` replica under Tier-0 (issue 557), and
//! the hours-long two-clock bench runs on frame counts alone.

use crate::asrc::{RateControl, ReadPoint, ASRC_HISTORY};

/// The hub rate every leg runs at unless told otherwise (`[hub] sample_rate`).
pub const HUB_RATE_HZ: u32 = 48_000;

/// The default target depth of a cambox VBAN leg, in hub blocks: 3 x 256 frames = 16 ms at the
/// 48 kHz hub rate. One block for the pop itself, half a packet of granularity ripple and the
/// resampler's 16-frame look-ahead below the mean, the rest (about 8 ms) for a late packet. The
/// camboxes are Linux senders that pace evenly (issue 1345 accepted 10-20 ms for the talkback).
pub const VBAN_TARGET_BLOCKS: usize = 3;

/// The spare block of a VBAN leg's cap above what a covered stall can bring ([`vban_cap_blocks`]):
/// the phase of the late wake inside its tick plus the arrival jitter.
pub const VBAN_CAP_HEADROOM_BLOCKS: usize = 1;

/// The cap of a VBAN leg with a target of `target_blocks`, in hub blocks: the target, plus the
/// blocks that arrive while the block loop is late but still catches up (its
/// [`crate::block_clock::CATCHUP_MAX_BLOCKS`] late ticks and the current one), plus
/// [`VBAN_CAP_HEADROOM_BLOCKS`] (design 5981457044). Under this cap a covered stall never trims a
/// leg, so the caught-up pops never run it dry.
pub const fn vban_cap_blocks(target_blocks: usize) -> usize {
    target_blocks + crate::block_clock::CATCHUP_MAX_BLOCKS as usize + 1 + VBAN_CAP_HEADROOM_BLOCKS
}

/// The cap of a cambox VBAN leg at its default target, in hub blocks (9 blocks, 48 ms).
pub const VBAN_CAP_BLOCKS: usize = vban_cap_blocks(VBAN_TARGET_BLOCKS);

/// The default target depth of a PROGRAM feed (every VBAN leg that is not a cambox: the FOH
/// `fohabl`, `lv1`, `mbc`), in hub blocks: 7 x 256 = 1792 frames = 37.3 ms. The Windows FOH sender
/// does not pace its packets; its largest measured gap was 27.6 ms (strih-snv, 4.10.2026), and the
/// target covers that gap plus one block plus half its ~6 ms burst. Fixed from the start (issue
/// 1461: the issue-1401 adaptive target followed the gaps between 6 and 12 blocks while running).
pub const VBAN_PROGRAM_TARGET_BLOCKS: usize = 7;

/// The cap of a program-feed leg at its default target, in hub blocks ([`vban_cap_blocks`]: 13).
pub const VBAN_PROGRAM_CAP_BLOCKS: usize = vban_cap_blocks(VBAN_PROGRAM_TARGET_BLOCKS);

/// The default target depth of the cutters' card capture: 2048 frames (42.7 ms), the depth the
/// issue-1345 capture ring held for the card's 1024-frame bursts (two bursts).
pub const CARD_CAPTURE_TARGET_FRAMES: usize = 2048;

/// The largest `target_ms` a participant may declare.
pub const TARGET_MS_MAX: f64 = 200.0;

/// A TOML `target_ms` in hub frames: the nearest frame, rounded UP to whole blocks (so 37.33 ms is
/// 1792 frames and 37.3 ms too, 16 ms is 768). `None` for a value outside `(0, TARGET_MS_MAX]`.
pub fn target_frames_from_ms(ms: f64, rate: u32, block: usize) -> Option<usize> {
    if block == 0 || !(f64::MIN_POSITIVE..=TARGET_MS_MAX).contains(&ms) {
        return None;
    }
    let frames = (ms * f64::from(rate) / 1000.0).round() as usize;
    Some(frames.div_ceil(block).max(1) * block)
}

/// What one pop of a leg does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PopPlan {
    /// Output a whole silent block and consume nothing (priming, or the stream ran short).
    /// `ran_dry` = this pop found a primed stream short of one block: ONE underrun (the buffer
    /// counts it only when the stream continues).
    Silent { ran_dry: bool },
    /// Drop the `skip` OLDEST frames (a prime's overshoot past the target, else 0), render one
    /// block from `read` over what is left, then drop the `consume` oldest frames.
    Audio {
        skip: usize,
        read: ReadPoint,
        consume: usize,
    },
}

/// A leg's live fill numbers for `/api/state` and the status line (issues 1401 + 1461).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NetworkFillStats {
    /// The target depth in frames: fixed from the start.
    pub target_frames: usize,
    /// The mean pre-pop depth (queued frames ahead of the read point) over the last 1 s (0 before
    /// the first second).
    pub depth_frames: usize,
    /// The lowest pre-pop depth in that second: the margin before an underrun is this minus one
    /// block and the resampler's 16-frame look-ahead.
    pub depth_min_frames: usize,
    /// The resampler's ratio in ppm off 1: the sender's clock against the hub's, plus the phase
    /// term while the depth sits outside the dead band.
    pub rate_ppm: f64,
    /// The mean depth of the last second minus the target, in frames: what the phase term acts on
    /// outside +-1/4 of the target.
    pub asrc_phase_frames: f64,
    /// Times the queue went above the cap and was resynced to the target.
    pub resyncs: u64,
    /// Times the stream stopped for longer than the stale limit and came back: a FOH sender outage,
    /// or for a cambox simply a mute (it sends only while unmuted).
    pub stalls: u64,
    /// Whether audio is flowing (false while priming, before the first packet or after an underrun).
    pub primed: bool,
}

/// The frame-count controller of one leg (see the module doc).
#[derive(Debug, Clone)]
pub struct NetworkFill {
    target: usize,
    cap: usize,
    primed: bool,
    /// The arrival series is running (primed once since the last restart).
    live: bool,
    read: ReadPoint,
    rate: RateControl,
    /// Input frames received since the last restart (the rate fit's arrival series).
    arrived: u64,
    win_sum: f64,
    win_min: f64,
    win_pops: u64,
    win_frames: usize,
    last_mean: f64,
    last_min: f64,
    resyncs: u64,
    stalls: u64,
}

impl NetworkFill {
    /// A controller at the hub rate [`HUB_RATE_HZ`] that primes to `target_frames` and resyncs to
    /// it above `cap_frames`. The target is clamped into `1..=cap`.
    pub fn new(target_frames: usize, cap_frames: usize) -> Self {
        Self::with_rate(target_frames, cap_frames, HUB_RATE_HZ)
    }

    /// [`NetworkFill::new`] for a hub running at `rate` Hz.
    pub fn with_rate(target_frames: usize, cap_frames: usize, rate: u32) -> Self {
        let cap = cap_frames.max(1);
        NetworkFill {
            target: target_frames.clamp(1, cap),
            cap,
            primed: false,
            live: false,
            read: ReadPoint::at_prime(1.0),
            rate: RateControl::new(rate),
            arrived: 0,
            win_sum: 0.0,
            win_min: f64::INFINITY,
            win_pops: 0,
            win_frames: 0,
            last_mean: 0.0,
            last_min: 0.0,
            resyncs: 0,
            stalls: 0,
        }
    }

    /// The target depth in frames.
    pub fn target(&self) -> usize {
        self.target
    }

    /// The cap in frames.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The read point the next pop starts from.
    pub fn read_point(&self) -> ReadPoint {
        self.read
    }

    /// A packet of `frames` frames arrived `at_s` seconds after the buffer's own origin on the
    /// hub's clock (`CLOCK_MONOTONIC`, the clock the block grid runs on): one point of the rate fit.
    /// Call it for every packet, before [`NetworkFill::overrun_keep`].
    pub fn note_arrival(&mut self, frames: usize, at_s: f64) {
        self.arrived = self.arrived.saturating_add(frames as u64);
        self.rate.arrival(at_s, self.arrived as f64);
    }

    /// The prime level: the target plus the resampler's history.
    fn prime_frames(&self) -> usize {
        self.target + ASRC_HISTORY
    }

    /// Called after a packet was appended, with the fill it left: `Some(keep)` = above the cap,
    /// drop the OLDEST audio down to `keep` frames, one counted resync (the caller counts its
    /// overrun too). A primed leg keeps its read point, so `keep` leaves its depth at the target.
    pub fn overrun_keep(&mut self, fill: usize) -> Option<usize> {
        if fill <= self.cap {
            return None;
        }
        let keep = if self.primed {
            (self.target as f64 + self.read.pos).round() as usize
        } else {
            self.prime_frames()
        };
        let keep = keep.min(fill);
        self.resyncs += 1;
        // The arrival series starts over: the burst that overran (and the audio trimmed) is a
        // step in it. The ratio is kept.
        self.arrived = 0;
        self.rate.restart();
        self.restart_window();
        Some(keep)
    }

    /// The buffer was emptied (its channel layout changed): prime again from nothing. The arrival
    /// series starts over; the ratio is kept (the sender's clock did not change).
    pub fn restart(&mut self) {
        self.primed = false;
        self.live = false;
        self.arrived = 0;
        self.rate.restart();
        self.restart_window();
    }

    /// The stream came back after going stale and its buffer was emptied: one more stall, then
    /// prime again from nothing.
    pub fn restart_after_stall(&mut self) {
        self.stalls += 1;
        self.restart();
    }

    /// The hub's block loop LOST `missed` ticks before this pop (the part of a late wake beyond the
    /// four it runs late, [`crate::block_clock::CATCHUP_MAX_BLOCKS`]): every output lost those
    /// blocks, so the leg reads past them exactly as `missed` pops would (the read point advances
    /// by `missed x frames x ratio`), which keeps its latency where it was. Never below a floor of
    /// half a block under the target: a fill that low gives up only what lies above the floor.
    /// Returns how many of the OLDEST frames to drop now. Nothing while priming (the prime trims
    /// anyway).
    pub fn discard_for_missed_ticks(&mut self, fill: usize, frames: usize, missed: u64) -> usize {
        if !self.primed || missed == 0 {
            return 0;
        }
        let lost = usize::try_from(missed)
            .unwrap_or(usize::MAX)
            .saturating_mul(frames);
        let (next, consume) = self.read.advanced(lost);
        let floor = self.target.saturating_sub(frames / 2).max(frames) as f64;
        if next.depth(fill.saturating_sub(consume)) >= floor {
            self.read = next;
            return consume;
        }
        (fill as f64 - self.read.pos - floor).max(0.0) as usize
    }

    /// Plan one pop of `frames` frames with `fill` frames queued.
    pub fn plan_pop(&mut self, fill: usize, frames: usize) -> PopPlan {
        if frames == 0 {
            return PopPlan::Audio {
                skip: 0,
                read: self.read,
                consume: 0,
            };
        }
        let mut skip = 0;
        if !self.primed {
            // Prefill to the target plus the history, and to at least what one block needs, so a
            // target below the block size can never flap between "refilled" and "ran dry". Then
            // start exactly there: what piled up past it is the oldest audio.
            let read = ReadPoint::at_prime(self.rate.ratio());
            let prime = self.prime_frames().max(read.frames_needed(frames));
            if fill < prime {
                if self.live {
                    self.feed_rate(frames);
                }
                return PopPlan::Silent { ran_dry: false };
            }
            self.primed = true;
            self.live = true;
            self.read = read;
            self.restart_window();
            skip = fill - prime;
        }
        let fill = fill - skip;
        let ratio = self.feed_rate(frames);
        self.read.ratio = ratio;
        if fill < self.read.frames_needed(frames) {
            // Ran dry: ONE silent block, then re-prime to the same target. The arrival series
            // starts over too: whatever ran the leg dry (a lost burst, a held one) is a step in it
            // that would bend the 64 s fit for a minute. The ratio is kept.
            self.primed = false;
            self.arrived = 0;
            self.rate.restart();
            self.restart_window();
            return PopPlan::Silent { ran_dry: true };
        }
        self.observe(self.read.depth(fill), frames);
        let read = self.read;
        let (next, consume) = read.advanced(frames);
        self.read = next;
        PopPlan::Audio {
            skip,
            read,
            consume,
        }
    }

    /// One block of the rate controller, with the leg's depth error of the last second.
    fn feed_rate(&mut self, frames: usize) -> f64 {
        let err = if self.primed {
            self.last_mean - self.target as f64
        } else {
            0.0
        };
        let band = self.target as f64 / 4.0;
        self.rate.step(err, band, frames)
    }

    /// Add one primed pre-pop depth to the 1 s window.
    fn observe(&mut self, depth: f64, frames: usize) {
        self.win_sum += depth;
        self.win_min = self.win_min.min(depth);
        self.win_pops += 1;
        self.win_frames = self.win_frames.saturating_add(frames);
        if self.win_frames < HUB_RATE_HZ as usize {
            return;
        }
        self.last_mean = self.win_sum / self.win_pops as f64;
        self.last_min = self.win_min;
        self.restart_window();
    }

    /// Start the next 1 s window of the depth statistics.
    fn restart_window(&mut self) {
        self.win_sum = 0.0;
        self.win_min = f64::INFINITY;
        self.win_pops = 0;
        self.win_frames = 0;
    }

    /// The live numbers for `/api/state`.
    pub fn stats(&self) -> NetworkFillStats {
        let tenth = |x: f64| (x * 10.0).round() / 10.0;
        NetworkFillStats {
            target_frames: self.target,
            depth_frames: self.last_mean.max(0.0).round() as usize,
            depth_min_frames: self.last_min.max(0.0).round() as usize,
            rate_ppm: tenth(self.rate.rate_ppm()),
            asrc_phase_frames: if self.last_mean > 0.0 {
                tenth(self.last_mean - self.target as f64)
            } else {
                0.0
            },
            resyncs: self.resyncs,
            stalls: self.stalls,
            primed: self.primed,
        }
    }
}
