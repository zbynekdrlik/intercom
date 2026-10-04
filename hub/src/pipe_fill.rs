//! The fill controller of a `pw-cat --playback` stdin pipe (issue 1401): the program sink (the
//! strih OBS `ASIO zvuk`) and the cutters' MiniFuse cans.
//!
//! pw-cat `fread`s a whole graph quantum ([`PW_GRAPH_BURST_FRAMES`], 1024 frames) from the pipe in
//! each process callback, on its sink's clock (the MiniFuse crystal for the cans). The hub writes
//! one 256-frame block per tick on its own clock. [`PipeFillControl`] decides every block write
//! from the measured pipe fill:
//!
//! - **The guards (last resort, [`pipe_fill_plan`]).** Below one hub block the pipe is topped up
//!   with silence to [`PIPE_TARGET_FRAMES`] before the block; above [`PIPE_HIGH_FRAMES`] the block
//!   is dropped. The first write after a spawn reads 0 and gets the same top-up: the PRIME.
//! - **The start hold.** pw-cat reads nothing until its stream runs, and the hub keeps writing
//!   meanwhile. Until the fill first DROPS after the spawn (pw-cat consumed), a block that would
//!   take the fill above [`PIPE_TARGET_FRAMES`] is dropped. So pw-cat's first read always finds
//!   exactly the prime (the target plus the prime's own block), whatever its connect time.
//! - **The drift servo.** Any rate offset between the hub and the sink walks the fill. The VBAN
//!   legs' servo ([`NetworkFill::servo_step`]: the 1 s mean, the gentle-then-steep budget, at
//!   least 1000 frames between corrections) keeps the fill at [`pipe_servo_setpoint`] with
//!   single-frame drops and repeats, spread across the written block by [`stretch_interleaved`].
//!
//! **The servo reads the TIME-WEIGHTED fill, not the pre-write readings.** pw-cat takes four hub
//! blocks at once. Between two read/write phase crossings the pre-write readings repeat the same
//! four values, so a drift reaches them only as a whole-block (256-frame) step at each crossing
//! (every ~107 s at 50 ppm): the servo would correct in bursts of up to 47 a second (design
//! question 5979620130). So the sink thread also reads the fill about every [`PIPE_SAMPLE_INTERVAL`]
//! between blocks ([`PipeFillControl::sample`]); each block's servo input is the trapezoid mean of
//! those readings since the last block. A read is then placed within ~1 ms, and a crossing moves
//! the mean by ~48 frames instead of 256 (ROZHODNUTÉ 5979627274).
//!
//! Pure (no I/O, no external crate) and clocked by the caller (nanoseconds on any monotonic
//! clock), so the hours-long two-clock bench (`tests/egress_servo_1401.rs`) runs it on frame
//! counts alone. The pipe I/O is [`crate::local_audio::PipeFillWriter`].

use std::time::Duration;

use crate::local_audio::PW_GRAPH_BURST_FRAMES;
use crate::vban_jitter::{stretch_block, NetworkFill, ServoStep};

/// The fill a `pw-cat --playback` stdin pipe is primed to (issue 1401, 4.10.2026): two graph quanta,
/// 2048 frames (42.7 ms at 48 kHz). pw-cat `fread`s a whole quantum ([`PW_GRAPH_BURST_FRAMES`])
/// from the pipe inside each process callback. A pipe holding less blocks that callback until the
/// hub has written the rest, the graph cycle overruns and the stream xruns. Nothing but the hub
/// refills the pipe, so a block the hub never wrote is gone from it for good (live: ERR ~50/s on
/// the program sink and the cutters' cans until a hub restart). A pipe below one hub block is
/// topped up to this with silence ([`pipe_fill_plan`]).
pub const PIPE_TARGET_FRAMES: usize = 2 * PW_GRAPH_BURST_FRAMES;

/// Above this fill (the target + two more quanta, 4096 frames, 85 ms) a block is dropped instead of
/// written, so a pipe that grew never holds more delay than this.
pub const PIPE_HIGH_FRAMES: usize = PIPE_TARGET_FRAMES + 2 * PW_GRAPH_BURST_FRAMES;

/// How often the sink thread reads the pipe fill between blocks for the servo's time-weighted fill
/// (its `recv_timeout` on the egress channel). About 1000 `FIONREAD`s a second per sink, accepted by
/// ROZHODNUTÉ 5979627274.
pub const PIPE_SAMPLE_INTERVAL: Duration = Duration::from_millis(1);

/// The servo's setpoint: the time-average depth the start hold leaves, the prime minus half a
/// graph quantum. pw-cat's first read finds the prime ([`PIPE_TARGET_FRAMES`] + one hub block) and
/// takes a whole quantum; the hub then writes the quantum back one block at a time before the next
/// read, so averaged over a read cycle (and over where the read falls inside a hub block) the pipe
/// holds the prime minus half a quantum: 2048 + 256 - 512 = 1792 frames (37.3 ms) at the 256-frame
/// hub block. Holding the servo there means a spawn starts on its setpoint: no walk after a
/// restart, and the same depth after every restart.
pub const fn pipe_servo_setpoint(block_frames: usize) -> usize {
    (PIPE_TARGET_FRAMES + block_frames).saturating_sub(PW_GRAPH_BURST_FRAMES / 2)
}

/// What one block write does to a `pw-cat` playback pipe ([`PipeFillControl::plan_block`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeFillPlan {
    /// The pipe holds less than one hub block: write `silence_frames` of silence (up to
    /// [`PIPE_TARGET_FRAMES`]), then the block.
    TopUp { silence_frames: usize },
    /// The pipe holds more than [`PIPE_HIGH_FRAMES`]: drop the block (a trim).
    Drop,
    /// Healthy: write the block as it is.
    Write,
    /// pw-cat has not read since the spawn and the block would take the fill above
    /// [`PIPE_TARGET_FRAMES`]: drop it (the start hold).
    StartHold,
    /// The servo's drop: write the block one frame short.
    ServoDrop,
    /// The servo's repeat: write the block one frame long.
    ServoRepeat,
}

impl PipeFillPlan {
    /// The frames this plan puts into the pipe for a `block_frames`-frame block.
    pub fn written_frames(self, block_frames: usize) -> usize {
        match self {
            PipeFillPlan::TopUp { silence_frames } => silence_frames + block_frames,
            PipeFillPlan::Write => block_frames,
            PipeFillPlan::ServoDrop => block_frames.saturating_sub(1),
            PipeFillPlan::ServoRepeat => block_frames + 1,
            PipeFillPlan::Drop | PipeFillPlan::StartHold => 0,
        }
    }
}

/// The two guards of a block write from the pipe's measured fill (issue 1401). `fill_frames` is the
/// fill before the write, `block_frames` the block being written (one hub block):
///
/// - below one block: top up with silence to [`PIPE_TARGET_FRAMES`], then write the block. A
///   fresh pipe (the first write after a spawn) reads 0 and gets the same top-up;
/// - above [`PIPE_HIGH_FRAMES`]: drop the block;
/// - otherwise: write it. A healthy steady state never triggers either guard.
///
/// [`PipeFillControl::plan_block`] adds the start hold and the drift servo to the `Write` case.
pub fn pipe_fill_plan(fill_frames: usize, block_frames: usize) -> PipeFillPlan {
    if fill_frames < block_frames {
        PipeFillPlan::TopUp {
            silence_frames: PIPE_TARGET_FRAMES.saturating_sub(fill_frames),
        }
    } else if fill_frames > PIPE_HIGH_FRAMES {
        PipeFillPlan::Drop
    } else {
        PipeFillPlan::Write
    }
}

/// What one block write did, for the `local_audio` facet and the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeWriteReport {
    /// The pipe fill measured before the write, in frames.
    pub fill_frames: usize,
    /// What the write did.
    pub plan: PipeFillPlan,
    /// The first write into this pipe (just after the spawn). Its top-up is the prime, not a
    /// refill: `pipe_refills` counts only a pipe that drained under a running pw-cat.
    pub first: bool,
}

/// The servo's live depth for `/api/state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeServoDepth {
    /// The servo's mean time-weighted fill over its last complete 1 s window, in frames.
    pub depth_frames: usize,
    /// The depth it holds ([`pipe_servo_setpoint`]).
    pub setpoint_frames: usize,
}

/// The fill controller of one `pw-cat` playback pipe from its spawn (see the module doc). A new
/// pw-cat child gets a new controller.
#[derive(Debug, Clone, Default)]
pub struct PipeFillControl {
    /// A block has been planned since the spawn (the next one is not the prime).
    primed: bool,
    /// pw-cat has read from the pipe since the spawn: the start hold is over.
    started: bool,
    /// The last known fill (a reading, or what the last write left) and its time.
    last_fill: usize,
    last_at: u64,
    /// Twice the time-weighted integral of the fill since `period_at` (trapezoids), in frame-ns.
    area2: u128,
    period_at: u64,
    /// The drift servo, built at the first block (its setpoint depends on the block size).
    servo: Option<NetworkFill>,
}

impl PipeFillControl {
    /// A controller for a freshly spawned pipe.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether pw-cat has read since the spawn (the start hold is over).
    pub fn started(&self) -> bool {
        self.started
    }

    /// One reading of the pipe fill between blocks, at `at_ns` on the caller's monotonic clock. A
    /// read by pw-cat between two readings is equally likely anywhere in between, so the interval
    /// counts as the trapezoid of its two readings.
    pub fn sample(&mut self, at_ns: u64, fill_frames: usize) {
        let at = at_ns.max(self.last_at);
        let span = u128::from(at - self.last_at);
        let sum = self.last_fill as u128 + fill_frames as u128;
        self.area2 = self.area2.saturating_add(sum.saturating_mul(span));
        if self.primed && !self.started && fill_frames < self.last_fill {
            // pw-cat's first read: the start hold is over, and the servo measures from here.
            self.started = true;
            self.area2 = 0;
            self.period_at = at;
        }
        self.last_fill = fill_frames;
        self.last_at = at;
    }

    /// Plan the write of one `block_frames`-frame block, with `fill_frames` read from the pipe at
    /// `at_ns` just before it. The guards come first, then the start hold, then the servo; a guard
    /// restarts the servo's window (like the VBAN legs' underrun and overrun).
    pub fn plan_block(
        &mut self,
        at_ns: u64,
        fill_frames: usize,
        block_frames: usize,
    ) -> PipeWriteReport {
        self.sample(at_ns, fill_frames);
        let at = self.last_at;
        let first = !self.primed;
        self.primed = true;
        let plan = match pipe_fill_plan(fill_frames, block_frames) {
            PipeFillPlan::Write if !self.started => {
                if fill_frames + block_frames > PIPE_TARGET_FRAMES {
                    PipeFillPlan::StartHold
                } else {
                    PipeFillPlan::Write
                }
            }
            PipeFillPlan::Write => self.servo_plan(at, fill_frames, block_frames),
            guard => {
                if let Some(servo) = &mut self.servo {
                    servo.restart();
                }
                guard
            }
        };
        // The write is immediate: a new period starts with what it left in the pipe.
        self.last_fill = fill_frames + plan.written_frames(block_frames);
        self.area2 = 0;
        self.period_at = at;
        PipeWriteReport {
            fill_frames,
            plan,
            first,
        }
    }

    /// The servo's decision for a block, from the time-weighted fill since the last block.
    fn servo_plan(&mut self, at: u64, fill_frames: usize, block_frames: usize) -> PipeFillPlan {
        let span = u128::from(at - self.period_at);
        let mean = match self.area2.checked_div(2 * span) {
            Some(mean) => usize::try_from(mean).unwrap_or(usize::MAX),
            // The hold ended at this very reading: nothing measured since but the reading itself.
            None => fill_frames,
        };
        let servo = self.servo.get_or_insert_with(|| {
            NetworkFill::new(pipe_servo_setpoint(block_frames), PIPE_HIGH_FRAMES)
        });
        match servo.servo_step(mean, block_frames) {
            ServoStep::Keep => PipeFillPlan::Write,
            ServoStep::Drop => PipeFillPlan::ServoDrop,
            ServoStep::Repeat => PipeFillPlan::ServoRepeat,
        }
    }

    /// The servo's depth and setpoint, once it runs (after pw-cat's first read).
    pub fn servo_depth(&self) -> Option<PipeServoDepth> {
        self.servo.as_ref().map(|servo| PipeServoDepth {
            depth_frames: servo.stats().depth_frames,
            setpoint_frames: servo.target(),
        })
    }
}

/// Stretch an interleaved PCM16 block of `channels` channels to `out_frames` frames, every channel
/// on its own by [`stretch_block`] (linear, both ends kept): the servo's one-frame drop or repeat
/// spread across the block, so it never clicks. A trailing partial frame is dropped.
pub fn stretch_interleaved(interleaved: &[i16], channels: usize, out_frames: usize) -> Vec<i16> {
    let n_ch = channels.max(1);
    let frames = interleaved.len() / n_ch;
    let mut out = vec![0i16; out_frames * n_ch];
    for ch in 0..n_ch {
        let channel: Vec<i16> = (0..frames).map(|f| interleaved[f * n_ch + ch]).collect();
        for (f, sample) in stretch_block(&channel, out_frames).into_iter().enumerate() {
            out[f * n_ch + ch] = sample;
        }
    }
    out
}
