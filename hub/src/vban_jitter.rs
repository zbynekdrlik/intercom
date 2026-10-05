//! The VBAN network legs' fill policy (issue 1401: short dropouts in the strih program audio).
//!
//! Every VBAN input (the FOH program feed `fohabl-strih`, `lv1-strih`, `mbc`, and the seven
//! camboxes' talkback `camN`) is popped by the hub's block loop as one 256-frame block every
//! 5.33 ms. The old network policy popped whatever was queued and zero-padded a short block. With
//! no target fill its depth was wherever the first packets' phase left it, often under one block,
//! so a packet a little late against the hub's block clock wrote a zero-padded block into the mix:
//! an audible dropout (4.10.2026: single underruns every 2-15 min on `fohabl`, a clean network).
//!
//! [`NetworkFill`] is the count-level controller the VBAN buffer
//! ([`crate::vban_io::JitterBuffer::vban_leg`]) delegates to:
//!
//! - **Prefill.** Nothing is consumed until the pre-pop fill reaches the target
//!   ([`VBAN_TARGET_BLOCKS`]); silence goes out meanwhile. Every prime (the first packets, a stream
//!   back after going stale, and the re-prime after an underrun) then starts EXACTLY at the target:
//!   what piled up past it is the oldest audio, and dropping it lands in the gap that is already
//!   there, the `janus_pacing::PacedRing` precedent. The depth never has to be walked back later.
//! - **Underrun = one whole silent block.** A pop that finds less than a block outputs ONE whole
//!   silent block and re-primes to the target. Never a zero-spliced partial block.
//! - **A lost hub tick** is answered in kind ([`NetworkFill::discard_for_missed_ticks`]). The block
//!   loop runs up to four missed ticks late ([`crate::block_clock`]); only a tick beyond that is
//!   lost, every output loses its block, and the leg gives up as many blocks of its oldest audio at
//!   once (never going more than half a block under its target), instead of the servo walking them
//!   off for seconds.
//! - **Drift servo.** The sender's clock (a Dante-ticked FOH desk, a cambox headset ADC) and the
//!   hub's disciplined monotonic clock differ by a few ppm (a cambox ADC by hundreds), which would
//!   walk any fixed depth to an edge over an 8 h program. Every second the mean pre-pop fill is
//!   compared with the target; [`servo_corrections`] turns the error into the next second's budget
//!   of single-frame drops (fill high) or repeats (fill low), spread evenly over that second and
//!   never closer than [`SERVO_MIN_SPACING_FRAMES`] (<= 1 ms/s, the `janus_pacing` precedent). Up
//!   to [`SERVO_KNEE_FRAMES`] of error it is gentle (at most 7 a second), so a start-up offset or a
//!   few-ppm drift is taken out a frame at a time; only a genuinely large drift past the knee gets
//!   the steep slope. Each frame is spread across its block by [`stretch_block`], so it never clicks.
//! - **Overrun.** Above the cap the OLDEST audio is dropped back down to the target (not just to the
//!   cap, where the next packet would overrun again).
//! - **A live setpoint.** A program feed's target follows its sender's gaps
//!   ([`crate::adaptive_target`]); [`NetworkFill::set_target`] moves the target and the cap with
//!   it. The servo then walks its setpoint to the new target GENTLY, at most
//!   [`SERVO_WALK_MAX_PER_WINDOW`] frames a second, on top of the drift correction's own budget
//!   (design 5981457044): a one-block raise takes about 37 s, never a second of steep-zone
//!   corrections.
//!
//! The same drift servo ([`NetworkFill::servo_step`]) also keeps the `pw-cat` egress pipes at their
//! depth: there the fill is measured outside the controller ([`crate::pipe_fill`], issue 1401), so
//! there is one drift policy for every hub audio path.
//!
//! Pure and std-only, so it verifies with a rustc `--test` replica under Tier-0 (issue 557), and
//! the hours-long two-clock bench runs on frame counts alone.

/// The target pre-pop fill of a VBAN network leg, in hub blocks: 3 x 256 frames = 16 ms at the
/// 48 kHz hub rate. The budget, at the 256-frame block:
///
/// - one whole block must be queued for the pop itself (5.33 ms);
/// - the packet granularity ripples the pre-pop fill by up to half a packet below its mean (a
///   cambox sends 128 frames per packet, the 96 kHz FOH feed about 52 after decimation, a VBAN
///   packet carries at most 256), and the servo holds the mean near the target;
/// - what is left, about 8 ms for a cambox or the FOH feed, absorbs a packet arriving late
///   against the mean. The live underrun pattern (one every 2-15 min with well under one block of
///   margin) puts the real arrival jitter far below that.
///
/// The hub's own tick only ever wakes LATE (tokio rounds a deadline up to the next ms). A late pop
/// sees more fill; the servo holds the MEAN, which includes that lateness, so an on-time pop sits
/// up to about 0.5 ms below it, small next to the margin.
///
/// The design (issue 1401) chose ~16 ms knowingly; for the cambox talkback the issue-1345 design
/// accepted about 10-20 ms for a target-fill ring (design comment 5813703805). The program feeds
/// (the FOH `fohabl` and the other non-cambox legs) first had it too, but their Windows sender
/// bursts past it, so since 4.10.2026 they target [`VBAN_PROGRAM_TARGET_BLOCKS`] instead.
pub const VBAN_TARGET_BLOCKS: usize = 3;

/// The spare block of a VBAN leg's cap above what a covered stall can bring ([`vban_cap_blocks`]):
/// the phase of the late wake inside its tick plus the arrival jitter.
pub const VBAN_CAP_HEADROOM_BLOCKS: usize = 1;

/// The cap of a VBAN network leg with a target of `target_blocks`, in hub blocks: the target, plus
/// the blocks that arrive while the block loop is late but still catches up (its
/// [`crate::block_clock::CATCHUP_MAX_BLOCKS`] late ticks and the current one), plus
/// [`VBAN_CAP_HEADROOM_BLOCKS`] (design 5981457044).
///
/// The pop before a stall leaves the fill about one block under its target; while the loop is up
/// to four ticks late, up to five more blocks plus the wake's phase and the jitter arrive before
/// the catch-up pops start. Under this cap those arrivals never trim the leg, so the caught-up
/// pops never run it dry (the step-3 bench: with 5 blocks of headroom a cambox leg overran and
/// then underran on most 26 ms stalls).
pub const fn vban_cap_blocks(target_blocks: usize) -> usize {
    target_blocks + crate::block_clock::CATCHUP_MAX_BLOCKS as usize + 1 + VBAN_CAP_HEADROOM_BLOCKS
}

/// The cap of a cambox VBAN leg, in hub blocks: [`vban_cap_blocks`] of [`VBAN_TARGET_BLOCKS`]
/// (9 blocks, 48 ms at 256 frames / 48 kHz).
pub const VBAN_CAP_BLOCKS: usize = vban_cap_blocks(VBAN_TARGET_BLOCKS);

/// The target fill of a PROGRAM feed (every VBAN leg that is not a cambox: the FOH `fohabl`, `lv1`,
/// `mbc`), in hub blocks: 32 ms at 256 frames / 48 kHz. The camboxes keep [`VBAN_TARGET_BLOCKS`].
///
/// Measured live on strih-lx, 4.10.2026: the Windows FOH sender does not pace its packets. 932
/// packets/s arrive in bursts (median gap 0, p90 5.94 ms, p99.9 17.64 ms) with gaps up to 19.4 ms
/// (30 gaps over 12 ms in 12 s), and with the 16 ms target the leg ran dry about every 2-3 s
/// without one missed hub tick. 32 ms covers the 19.4 ms gap plus one block plus the burst's own
/// sawtooth below the mean. The cost is a steady 16 ms more delay on the strih program audio (the
/// OBS `ASIO zvuk` input); the cambox talkback is unchanged (issue 1401, design comment 5979008527).
///
/// Since design 5980775411 this is the FLOOR of the program feeds' adaptive target
/// ([`crate::adaptive_target`]): the sender's gaps grew to 27.6 ms in the afternoon, and the
/// target now follows the largest gap of the last 10 min between this and 64 ms.
pub const VBAN_PROGRAM_TARGET_BLOCKS: usize = 6;

/// The cap of a program-feed leg at its floor target, in hub blocks: [`vban_cap_blocks`] of
/// [`VBAN_PROGRAM_TARGET_BLOCKS`] (12 blocks). The cap moves with the adaptive target
/// ([`NetworkFill::set_target`] keeps the headroom), so at every target it is that target's
/// [`vban_cap_blocks`].
pub const VBAN_PROGRAM_CAP_BLOCKS: usize = vban_cap_blocks(VBAN_PROGRAM_TARGET_BLOCKS);

/// The servo's averaging window in output frames: 1 s at the 48 kHz hub rate. Long enough that the
/// packet ripple and the arrival jitter average out of the mean pre-pop fill.
pub const SERVO_WINDOW_FRAMES: usize = 48_000;

/// The servo leaves the mean pre-pop fill alone within this many frames of the target (0.33 ms):
/// wider than the 1 s mean's wobble under arrival jitter, so the jitter alone never triggers a
/// correction, and a small part of a block, so the leg settles close to its target.
pub const SERVO_DEADBAND_FRAMES: usize = 16;

/// Up to this mean error (2.7 ms) the servo is GENTLE: one corrected frame per second for every
/// [`SERVO_GENTLE_DIV`] frames beyond the band, at most 7 a second. That covers what a prime can
/// leave (a packet of granularity, a late hub wake, the jitter) and a few-ppm drift (+20 ppm settles
/// about 16 frames past the band, one correction a second).
pub const SERVO_KNEE_FRAMES: usize = 128;

/// The gentle slope below [`SERVO_KNEE_FRAMES`]: one corrected frame a second per this many frames.
pub const SERVO_GENTLE_DIV: usize = 16;

/// The steep slope past [`SERVO_KNEE_FRAMES`]: one more corrected frame a second per this many
/// frames. Only a genuinely large drift lives there: the +540 ppm cambox headset ADC (issue 1345,
/// cam1, ~26 frames/s) settles about 166 frames high (3.5 ms), well inside the cap, and a sender as
/// slow sits as far below the target, still ~5 ms above an underrun.
pub const SERVO_STEEP_DIV: usize = 2;

/// At most one single-frame correction per this many output frames: 1000 ppm = 1 ms/s at any
/// rate (one per four 256-frame blocks = 977 ppm). A sender further off than that still drains or
/// overruns its leg (the design's escalation is an ASRC).
pub const SERVO_MIN_SPACING_FRAMES: usize = 1_000;

/// The most corrections one second can hold at that spacing.
const SERVO_MAX_PER_WINDOW: usize = SERVO_WINDOW_FRAMES / SERVO_MIN_SPACING_FRAMES;

/// How fast a setpoint change is walked in: at most this many frames a second, the gentle zone's
/// own ceiling ([`servo_corrections`] at the knee: (128 - 16) / 16 = 7). A target that
/// [`NetworkFill::set_target`] moves by one block (256 frames) is reached in about 37 s with at
/// most 7 corrections a second, never at the steep zone's 48 (design 5981457044: the steep walk of
/// a raise was the flutter review 1 rejected). Drift correction is measured against the walked
/// setpoint and keeps its own full budget on top.
pub const SERVO_WALK_MAX_PER_WINDOW: usize =
    (SERVO_KNEE_FRAMES - SERVO_DEADBAND_FRAMES).div_ceil(SERVO_GENTLE_DIV);

/// The servo's budget for the next second: how many single frames to drop or repeat for a mean
/// pre-pop fill `error` frames away from the target. Zero inside the band, then the gentle slope up
/// to the knee, then the steep one, capped at the 1 ms/s spacing. Never the whole error at once,
/// so the half-window lag of the mean can never make the servo overshoot.
pub fn servo_corrections(error: usize) -> usize {
    if error <= SERVO_DEADBAND_FRAMES {
        return 0;
    }
    let gentle = (error.min(SERVO_KNEE_FRAMES) - SERVO_DEADBAND_FRAMES).div_ceil(SERVO_GENTLE_DIV);
    let steep = error
        .saturating_sub(SERVO_KNEE_FRAMES)
        .div_ceil(SERVO_STEEP_DIV);
    (gentle + steep).min(SERVO_MAX_PER_WINDOW)
}

/// What one pop of a VBAN network leg does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopPlan {
    /// Output a whole silent block and consume nothing (priming, or the stream ran short).
    /// `ran_dry` = this pop found a primed stream short of one block: ONE underrun (the buffer
    /// counts it only when the stream continues).
    Silent { ran_dry: bool },
    /// Drop the `skip` OLDEST frames (a prime's overshoot past the target, else 0), then consume
    /// `take` frames and output one block: `take` is the block size, one more for a servo drop, one
    /// fewer for a servo repeat (the block is then [`stretch_block`]ed to size).
    Audio { skip: usize, take: usize },
}

/// What the drift servo does to one block ([`NetworkFill::servo_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServoStep {
    /// No correction on this block.
    Keep,
    /// The fill sits high: one frame of audio comes out of this block (a pop takes one more frame,
    /// an egress write puts one fewer into its pipe).
    Drop,
    /// The fill sits low: one frame of audio is added to this block (a pop takes one fewer frame,
    /// an egress write puts one more into its pipe).
    Repeat,
}

/// The VBAN leg's live fill numbers for `/api/state` (issue 1401).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetworkFillStats {
    /// The target pre-pop fill in frames.
    pub target_frames: usize,
    /// The mean pre-pop fill over the last completed 1 s window (0 before the first).
    pub depth_frames: usize,
    /// The lowest pre-pop fill in that window: the margin left before an underrun is this minus
    /// one block.
    pub depth_min_frames: usize,
    /// Single frames the servo dropped because the fill sat high.
    pub servo_drops: u64,
    /// Single frames the servo repeated because the fill sat low.
    pub servo_repeats: u64,
    /// Times the stream stopped for longer than the stale limit and came back: a FOH sender outage,
    /// or for a cambox simply a mute (it sends only while unmuted).
    pub stalls: u64,
    /// Whether audio is flowing (false while priming, before the first packet or after an underrun).
    pub primed: bool,
    /// A program feed's largest inter-arrival gap of the last 10 min in µs, which its adaptive
    /// target follows ([`crate::adaptive_target`]); `None` for a leg with a fixed target. Filled in
    /// by the buffer: the controller itself never sees the arrival times.
    pub max_gap_us_10min: Option<u64>,
}

/// The servo's decision for the current window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Correction {
    None,
    Drop,
    Repeat,
}

/// The count-level fill controller of one VBAN network leg (see the module doc).
#[derive(Debug, Clone)]
pub struct NetworkFill {
    target: usize,
    cap: usize,
    /// The fill the servo holds now: the target, except while a [`NetworkFill::set_target`]
    /// change is being walked in ([`SERVO_WALK_MAX_PER_WINDOW`] frames a second at most).
    setpoint: usize,
    /// How many of this second's corrections are the walk's (they move the setpoint with them).
    walk_budget: usize,
    primed: bool,
    win_sum: u64,
    /// The setpoint summed over the window's pops: a walk moves it during the window.
    win_setpoint_sum: u64,
    win_min: usize,
    win_pops: u64,
    win_frames: usize,
    /// This second's correction direction, how many are left, and their spacing in output frames.
    correction: Correction,
    budget: usize,
    interval: usize,
    since_correction: usize,
    last_mean: usize,
    last_min: usize,
    servo_drops: u64,
    servo_repeats: u64,
    stalls: u64,
}

impl NetworkFill {
    /// A controller that primes to `target_frames` and trims back to it above `cap_frames`. The
    /// target is clamped into `1..=cap`.
    pub fn new(target_frames: usize, cap_frames: usize) -> Self {
        let cap = cap_frames.max(1);
        let target = target_frames.clamp(1, cap);
        NetworkFill {
            target,
            cap,
            setpoint: target,
            walk_budget: 0,
            primed: false,
            win_sum: 0,
            win_setpoint_sum: 0,
            win_min: usize::MAX,
            win_pops: 0,
            win_frames: 0,
            correction: Correction::None,
            budget: 0,
            interval: SERVO_MIN_SPACING_FRAMES,
            since_correction: 0,
            last_mean: 0,
            last_min: 0,
            servo_drops: 0,
            servo_repeats: 0,
            stalls: 0,
        }
    }

    /// The target pre-pop fill in frames.
    pub fn target(&self) -> usize {
        self.target
    }

    /// The fill the servo holds now: the target, or, while a [`NetworkFill::set_target`] change
    /// is being walked in, the point the walk has reached.
    pub fn setpoint(&self) -> usize {
        self.setpoint
    }

    /// Move the target to `target_frames` (at least 1), and the cap with it: the headroom above the
    /// target stays what it was (for a VBAN leg the [`vban_cap_blocks`] rule at every target).
    /// Nothing is dropped or padded. The servo keeps holding its current setpoint and walks it to
    /// the new target at most [`SERVO_WALK_MAX_PER_WINDOW`] frames a second, on top of the drift
    /// correction (design 5981457044). The current window and its drift budget stay: they were
    /// measured against that setpoint, which did not move; only an unspent part of this second's
    /// walk toward an older target is cancelled. A leg that is priming primes to the new target.
    pub fn set_target(&mut self, target_frames: usize) {
        let headroom = self.cap - self.target;
        self.target = target_frames.max(1);
        self.cap = self.target.saturating_add(headroom);
        if !self.primed {
            self.setpoint = self.target;
        }
        self.budget = self.budget.saturating_sub(self.walk_budget);
        self.walk_budget = 0;
        if self.budget == 0 {
            self.correction = Correction::None;
        }
    }

    /// Called after a packet was appended, with the fill it left: `Some(keep)` = above the cap, drop
    /// the OLDEST audio down to `keep` frames (one overrun, counted by the caller). The servo
    /// window restarts, so the high fill before the trim does not steer the next second. The trim
    /// leaves the fill exactly at the target, so a walk in progress ends there.
    pub fn overrun_keep(&mut self, fill: usize) -> Option<usize> {
        if fill <= self.cap {
            return None;
        }
        self.setpoint = self.target;
        self.restart_window();
        Some(self.target)
    }

    /// The buffer was emptied (its channel layout changed): prime again from nothing.
    pub fn restart(&mut self) {
        self.primed = false;
        self.setpoint = self.target;
        self.restart_window();
    }

    /// The stream came back after going stale and its buffer was emptied: one more stall, then
    /// prime again from nothing.
    pub fn restart_after_stall(&mut self) {
        self.stalls += 1;
        self.restart();
    }

    /// The hub's block loop LOST `missed` ticks before this pop (the part of a late wake beyond the
    /// four it runs late, [`crate::block_clock::CATCHUP_MAX_BLOCKS`]): the outputs lost those
    /// blocks, so the leg gives up as many `frames`-frame blocks of its OLDEST audio, which leaves
    /// it where those pops would have. The block loop calls it right before the pop that follows
    /// the catch-up cycles. The floor is half a block under the target: a single lost block is
    /// given up whole even when the jitter has the fill a little under its mean at that moment,
    /// and after a hub stall long enough to overrun (already trimmed to the target) the leg keeps
    /// at least the target minus half a block. Returns how many frames to drop now. Nothing while
    /// priming (the prime trims anyway). The servo keeps its window and this second's budget: the
    /// discard leaves the depth where the lost pops would have, and no fill from before it was ever
    /// measured, so cancelling the budget would only starve the drift correction when the hub
    /// loses ticks often.
    pub fn discard_for_missed_ticks(&self, fill: usize, frames: usize, missed: u64) -> usize {
        if !self.primed || missed == 0 {
            return 0;
        }
        let lost = usize::try_from(missed)
            .unwrap_or(usize::MAX)
            .saturating_mul(frames);
        let floor = self.target.saturating_sub(frames / 2).max(frames);
        lost.min(fill.saturating_sub(floor))
    }

    /// Plan one pop of `frames` frames with `fill` frames queued.
    pub fn plan_pop(&mut self, fill: usize, frames: usize) -> PopPlan {
        if frames == 0 {
            return PopPlan::Audio { skip: 0, take: 0 };
        }
        let mut skip = 0;
        if !self.primed {
            // Prefill to the target, and to at least one block, so a target below the block size
            // can never flap between "refilled" and "ran dry" on the same pop. Then start exactly
            // there: what piled up past it is the oldest audio, received while silence went out.
            let prime = self.target.max(frames);
            if fill < prime {
                return PopPlan::Silent { ran_dry: false };
            }
            self.primed = true;
            // The prime starts exactly at the target: a walk in progress ends there.
            self.setpoint = self.target;
            self.restart_window();
            skip = fill - prime;
        }
        let fill = fill - skip;
        if fill < frames {
            // Ran dry: ONE silent block, then re-prime to the target.
            self.primed = false;
            self.restart_window();
            return PopPlan::Silent { ran_dry: true };
        }
        let take = match self.servo_step(fill, frames) {
            ServoStep::Keep => frames,
            ServoStep::Drop => frames + 1,
            ServoStep::Repeat => frames - 1,
        };
        PopPlan::Audio { skip, take }
    }

    /// The drift servo alone: add one block's fill to the 1 s window and decide this block's
    /// correction. [`NetworkFill::plan_pop`] runs exactly this once its leg is primed. A buffer
    /// whose fill is measured outside this controller uses it directly (the `pw-cat` egress pipes,
    /// [`crate::pipe_fill::PipeFillControl`], issue 1401). The caller owns any prime / underrun
    /// handling, and [`NetworkFill::restart`] forgets the window and the pending correction.
    ///
    /// `fill` is the fill this block sees, `frames` the block. A drop needs `fill > frames`, a
    /// repeat `frames > 1`, the same guards as a pop.
    pub fn servo_step(&mut self, fill: usize, frames: usize) -> ServoStep {
        self.observe(fill, frames);
        self.since_correction = self.since_correction.saturating_add(frames);
        if self.budget == 0 || self.since_correction < self.interval {
            return ServoStep::Keep;
        }
        let step = match self.correction {
            Correction::Drop if fill > frames => {
                self.servo_drops += 1;
                ServoStep::Drop
            }
            Correction::Repeat if frames > 1 => {
                self.servo_repeats += 1;
                ServoStep::Repeat
            }
            _ => ServoStep::Keep,
        };
        if step != ServoStep::Keep {
            self.budget -= 1;
            self.since_correction = 0;
            if self.budget < self.walk_budget {
                // The drift's share of this second is spent first (so a correction it planned is
                // never lost to the walk), then each walk frame done moves the setpoint with it.
                self.walk_budget -= 1;
                self.move_setpoint(1);
            }
        }
        step
    }

    /// Move the setpoint up to `frames` toward the target, never past it.
    fn move_setpoint(&mut self, frames: usize) {
        self.setpoint = if self.target > self.setpoint {
            self.setpoint.saturating_add(frames).min(self.target)
        } else {
            self.setpoint.saturating_sub(frames).max(self.target)
        };
    }

    /// Add one primed pre-pop fill to the window; at the window's end plan the next second's
    /// corrections from the mean.
    fn observe(&mut self, fill: usize, frames: usize) {
        self.win_sum = self.win_sum.saturating_add(fill as u64);
        self.win_setpoint_sum = self.win_setpoint_sum.saturating_add(self.setpoint as u64);
        self.win_min = self.win_min.min(fill);
        self.win_pops += 1;
        self.win_frames = self.win_frames.saturating_add(frames);
        if self.win_frames < SERVO_WINDOW_FRAMES {
            return;
        }
        let mean = usize::try_from(self.win_sum / self.win_pops).unwrap_or(usize::MAX);
        let setpoint_mean =
            usize::try_from(self.win_setpoint_sum / self.win_pops).unwrap_or(usize::MAX);
        self.last_mean = mean;
        self.last_min = self.win_min;
        self.plan_window(mean, setpoint_mean);
        self.reset_window_sums();
    }

    /// Plan the next second's corrections from the window's mean fill and its mean setpoint.
    ///
    /// The fill is judged against where the setpoint WAS over the same window, then carried to the
    /// setpoint's end point: a walk moves the setpoint (and the fill with it) during the window, and
    /// against its end point the walk's own progress would read as an error and add a drift
    /// correction to every walk second. Without a walk the two are the same number.
    fn plan_window(&mut self, mean: usize, setpoint_mean: usize) {
        let fill = (mean + self.setpoint).saturating_sub(setpoint_mean);
        // A fill that sits more than the band past the setpoint toward the target (a sender drifting
        // that way, a given-up block) has already done that part of the walk: the setpoint moves to
        // the band behind it (never past the target) with no correction, and nothing pulls the fill
        // back against the walk. Within the band the setpoint stays, so the 1 s mean's own wobble
        // never ratchets it forward.
        let ahead = if self.target > self.setpoint {
            fill.saturating_sub(self.setpoint)
        } else {
            self.setpoint.saturating_sub(fill)
        };
        self.move_setpoint(ahead.saturating_sub(SERVO_DEADBAND_FRAMES));
        // Drift is measured against the setpoint, with its full budget. While a walk is left the
        // fill sits inside the band past the setpoint (no drift correction) or on its far side, so
        // the drift budget points the same way as the walk and the two simply add up.
        let drift = servo_corrections(fill.abs_diff(self.setpoint));
        // The walk toward a moved target: at most SERVO_WALK_MAX_PER_WINDOW corrections this
        // second, and only what the 1 ms/s spacing leaves after the drift's budget.
        let walk = self
            .target
            .abs_diff(self.setpoint)
            .min(SERVO_WALK_MAX_PER_WINDOW)
            .min(SERVO_MAX_PER_WINDOW - drift);
        let n = drift + walk;
        if n == 0 {
            self.correction = Correction::None;
            self.budget = 0;
            self.walk_budget = 0;
            return;
        }
        let raise = if walk > 0 {
            self.target > self.setpoint
        } else {
            fill < self.setpoint
        };
        self.correction = if raise {
            Correction::Repeat
        } else {
            Correction::Drop
        };
        // Spread evenly over the next second.
        self.budget = n;
        self.walk_budget = walk;
        self.interval = (SERVO_WINDOW_FRAMES / n).max(SERVO_MIN_SPACING_FRAMES);
    }

    /// Start the next window's sums.
    fn reset_window_sums(&mut self) {
        self.win_sum = 0;
        self.win_setpoint_sum = 0;
        self.win_min = usize::MAX;
        self.win_pops = 0;
        self.win_frames = 0;
    }

    /// Forget the current window and any pending correction (priming, an underrun, an overrun trim).
    fn restart_window(&mut self) {
        self.reset_window_sums();
        self.correction = Correction::None;
        self.budget = 0;
        self.walk_budget = 0;
    }

    /// The live numbers for `/api/state`.
    pub fn stats(&self) -> NetworkFillStats {
        NetworkFillStats {
            target_frames: self.target,
            depth_frames: self.last_mean,
            depth_min_frames: self.last_min,
            servo_drops: self.servo_drops,
            servo_repeats: self.servo_repeats,
            stalls: self.stalls,
            primed: self.primed,
            max_gap_us_10min: None,
        }
    }
}

/// Resample one block of `input` to `out_len` samples by linear interpolation, keeping both end
/// samples. With `input.len() == out_len ± 1` this is the servo's single-frame drop or repeat
/// spread across the whole block: a 0.4 % time stretch for 5.33 ms instead of a one-sample jump,
/// and the next block continues exactly where this one ends, so no click either way.
pub fn stretch_block(input: &[i16], out_len: usize) -> Vec<i16> {
    let n_in = input.len();
    if n_in == out_len {
        return input.to_vec();
    }
    if out_len == 0 {
        return Vec::new();
    }
    let Some(&first) = input.first() else {
        return vec![0; out_len];
    };
    if n_in == 1 || out_len == 1 {
        return vec![first; out_len];
    }
    let num = (n_in - 1) as u64;
    let den = (out_len - 1) as u64;
    (0..out_len)
        .map(|i| {
            let pos = i as u64 * num;
            let idx = (pos / den) as usize;
            let rem = pos % den;
            if rem == 0 {
                input[idx]
            } else {
                let a = f64::from(input[idx]);
                let b = f64::from(input[idx + 1]);
                (a + (b - a) * rem as f64 / den as f64).round() as i16
            }
        })
        .collect()
}
