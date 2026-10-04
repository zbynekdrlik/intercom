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
//!   ([`VBAN_TARGET_BLOCKS`]); silence goes out meanwhile. A COLD start (the first packets, or a
//!   stream back after going stale, [`NetworkFill::restart_cold`]) drops whatever piled up past the
//!   target, the oldest audio, received while silence went out; a re-prime after an underrun does
//!   not.
//! - **Underrun = one whole silent block.** A pop that finds less than a block outputs ONE whole
//!   silent block, keeps the partial tail and re-primes to the target. A late burst that refills the
//!   buffer at once resumes after that single block with nothing thrown away. Never a zero-spliced
//!   partial block.
//! - **Drift servo.** The sender's clock (a Dante-ticked FOH desk, a cambox headset ADC) and the
//!   hub's disciplined monotonic clock differ by a few ppm (a cambox ADC by hundreds), which would
//!   walk any fixed depth to an edge over an 8 h program. Every second the mean pre-pop fill is
//!   compared with the target. Outside a [`SERVO_DEADBAND_FRAMES`] band the next second drops (fill
//!   high) or repeats (fill low) one frame for every [`SERVO_GAIN_DIV`] frames of error beyond the
//!   band, spread evenly over that second and never closer than [`SERVO_MIN_SPACING_FRAMES`]
//!   (<= 1 ms/s, the `janus_pacing` precedent). A slow drift is thus taken out a frame at a time
//!   as it builds up, never in a second-long burst, and the servo never overshoots. Each frame is
//!   spread across its block by [`stretch_block`], so a correction never clicks.
//! - **Overrun.** Above the cap the OLDEST audio is dropped back down to the target (not just to the
//!   cap, where the next packet would overrun again).
//!
//! Pure and std-only, so it verifies with a rustc `--test` replica under Tier-0 (issue 557), and
//! the hours-long two-clock bench runs on frame counts alone.

/// The target pre-pop fill of a VBAN network leg, in hub blocks: 3 x 256 frames = 16 ms at the
/// 48 kHz hub rate. The budget, at the 256-frame block:
///
/// - one whole block must be queued for the pop itself (5.33 ms);
/// - the packet granularity ripples the pre-pop fill by up to half a packet below its mean (a
///   cambox sends 128 frames per packet, the 96 kHz FOH feed about 52 after decimation, a VBAN
///   packet carries at most 256), and the servo holds the mean within [`SERVO_DEADBAND_FRAMES`];
/// - what is left, about 8 ms for a cambox or the FOH feed, absorbs a packet arriving late
///   against the mean. The live underrun pattern (one every 2-15 min with well under one block of
///   margin) puts the real arrival jitter far below that.
///
/// The hub's own tick only ever wakes LATE (tokio rounds a deadline up to the next ms). A late pop
/// sees more fill; the servo holds the MEAN, which includes that lateness, so an on-time pop sits
/// up to about 0.5 ms below it, small next to the margin. The issue-1345 design accepted "about
/// 10-20 ms" of talkback latency for a target-fill ring (design comment 5813703805); 16 ms is
/// inside that.
pub const VBAN_TARGET_BLOCKS: usize = 3;

/// The cap of a VBAN network leg, in hub blocks (43 ms at 256 frames / 48 kHz): five blocks of
/// headroom above the target for a late burst before anything is dropped.
pub const VBAN_CAP_BLOCKS: usize = 8;

/// The servo's averaging window in output frames: 1 s at the 48 kHz hub rate. Long enough that the
/// packet ripple and the arrival jitter average out of the mean pre-pop fill.
pub const SERVO_WINDOW_FRAMES: usize = 48_000;

/// The servo leaves the mean pre-pop fill alone within this many frames of the target (0.33 ms):
/// wider than the 1 s mean's wobble under arrival jitter, so the jitter alone never triggers a
/// correction, and a small part of a block, so the leg settles close to its target.
pub const SERVO_DEADBAND_FRAMES: usize = 16;

/// The servo's proportional gain: beyond the band, ONE corrected frame in the next second for every
/// this many frames of mean error. A sender `r` frames/s off the hub settles `r x 4` frames past
/// the band (+20 ppm: about 4 frames; the +540 ppm cambox ADC: about 104 frames, 2.2 ms). Never the
/// whole error at once, so the half-window lag of the mean can never make it overshoot.
pub const SERVO_GAIN_DIV: usize = 4;

/// At most one single-frame correction per this many output frames: 1000 ppm = 1 ms/s at any
/// rate (one per four 256-frame blocks = 977 ppm). Covers a few-ppm program sender and a cambox
/// headset ADC measured at +540 ppm (issue 1345, cam1).
pub const SERVO_MIN_SPACING_FRAMES: usize = 1_000;

/// The most corrections one second can hold at that spacing.
const SERVO_MAX_PER_WINDOW: usize = SERVO_WINDOW_FRAMES / SERVO_MIN_SPACING_FRAMES;

/// What one pop of a VBAN network leg does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopPlan {
    /// Output a whole silent block and consume nothing (priming, or the stream ran short).
    /// `ran_dry` = this pop found a primed stream short of one block: ONE underrun (the buffer
    /// counts it only for a live, not-stale stream).
    Silent { ran_dry: bool },
    /// Drop the `skip` OLDEST frames (a cold start's prime overshoot, else 0), then consume `take`
    /// frames and output one block: `take` is the block size, one more for a servo drop, one fewer
    /// for a servo repeat (the block is then [`stretch_block`]ed to size).
    Audio { skip: usize, take: usize },
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
    /// Whether audio is flowing (false while priming, before the first packet or after an underrun).
    pub primed: bool,
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
    primed: bool,
    /// The next prime is a cold start: trim the overshoot back to the target.
    cold: bool,
    win_sum: u64,
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
}

impl NetworkFill {
    /// A controller that primes to `target_frames` and trims back to it above `cap_frames`. The
    /// target is clamped into `1..=cap`.
    pub fn new(target_frames: usize, cap_frames: usize) -> Self {
        let cap = cap_frames.max(1);
        NetworkFill {
            target: target_frames.clamp(1, cap),
            cap,
            primed: false,
            cold: true,
            win_sum: 0,
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
        }
    }

    /// The target pre-pop fill in frames.
    pub fn target(&self) -> usize {
        self.target
    }

    /// Called after a packet was appended, with the fill it left: `Some(keep)` = above the cap, drop
    /// the OLDEST audio down to `keep` frames (one overrun, counted by the caller). The servo
    /// window restarts, so the high fill before the trim does not steer the next second.
    pub fn overrun_keep(&mut self, fill: usize) -> Option<usize> {
        if fill <= self.cap {
            return None;
        }
        self.restart_window();
        Some(self.target)
    }

    /// The stream went stale and its buffer was emptied (a muted cambox, a restarted sender): the
    /// next prime is a cold start again, and nothing from before carries over.
    pub fn restart_cold(&mut self) {
        self.primed = false;
        self.cold = true;
        self.restart_window();
    }

    /// Plan one pop of `frames` frames with `fill` frames queued.
    pub fn plan_pop(&mut self, fill: usize, frames: usize) -> PopPlan {
        if frames == 0 {
            return PopPlan::Audio { skip: 0, take: 0 };
        }
        let mut skip = 0;
        if !self.primed {
            // Prefill to the target, and to at least one block, so a target below the block size
            // can never flap between "refilled" and "ran dry" on the same pop.
            let prime = self.target.max(frames);
            if fill < prime {
                return PopPlan::Silent { ran_dry: false };
            }
            self.primed = true;
            self.restart_window();
            if self.cold {
                // A cold start begins exactly at the target: what piled up past it is the oldest
                // audio, received while silence went out.
                self.cold = false;
                skip = fill - prime;
            }
        }
        let fill = fill - skip;
        if fill < frames {
            // Ran dry: ONE silent block, the partial tail kept, then re-prime to the target.
            self.primed = false;
            self.restart_window();
            return PopPlan::Silent { ran_dry: true };
        }
        self.observe(fill, frames);
        self.since_correction = self.since_correction.saturating_add(frames);
        if self.budget > 0 && self.since_correction >= self.interval {
            let take = match self.correction {
                Correction::Drop if fill > frames => {
                    self.servo_drops += 1;
                    frames + 1
                }
                Correction::Repeat if frames > 1 => {
                    self.servo_repeats += 1;
                    frames - 1
                }
                _ => frames,
            };
            if take != frames {
                self.budget -= 1;
                self.since_correction = 0;
            }
            return PopPlan::Audio { skip, take };
        }
        PopPlan::Audio { skip, take: frames }
    }

    /// Add one primed pre-pop fill to the window; at the window's end plan the next second's
    /// corrections from the mean.
    fn observe(&mut self, fill: usize, frames: usize) {
        self.win_sum = self.win_sum.saturating_add(fill as u64);
        self.win_min = self.win_min.min(fill);
        self.win_pops += 1;
        self.win_frames = self.win_frames.saturating_add(frames);
        if self.win_frames < SERVO_WINDOW_FRAMES {
            return;
        }
        let mean = usize::try_from(self.win_sum / self.win_pops).unwrap_or(usize::MAX);
        self.last_mean = mean;
        self.last_min = self.win_min;
        let error = mean.abs_diff(self.target);
        if error > SERVO_DEADBAND_FRAMES {
            // Proportional, and spread evenly over the next second.
            let n = (error - SERVO_DEADBAND_FRAMES)
                .div_ceil(SERVO_GAIN_DIV)
                .min(SERVO_MAX_PER_WINDOW);
            self.correction = if mean > self.target {
                Correction::Drop
            } else {
                Correction::Repeat
            };
            self.budget = n;
            self.interval = (SERVO_WINDOW_FRAMES / n).max(SERVO_MIN_SPACING_FRAMES);
        } else {
            self.correction = Correction::None;
            self.budget = 0;
        }
        self.win_sum = 0;
        self.win_min = usize::MAX;
        self.win_pops = 0;
        self.win_frames = 0;
    }

    /// Forget the current window and any pending correction (priming, an underrun, an overrun trim).
    fn restart_window(&mut self) {
        self.win_sum = 0;
        self.win_min = usize::MAX;
        self.win_pops = 0;
        self.win_frames = 0;
        self.correction = Correction::None;
        self.budget = 0;
    }

    /// The live numbers for `/api/state`.
    pub fn stats(&self) -> NetworkFillStats {
        NetworkFillStats {
            target_frames: self.target,
            depth_frames: self.last_mean,
            depth_min_frames: self.last_min,
            servo_drops: self.servo_drops,
            servo_repeats: self.servo_repeats,
            primed: self.primed,
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
