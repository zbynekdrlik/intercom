//! The fill controller of an egress playback pipe (issues 1401 + 1461): the strih-pp program sink
//! (`pw-cat --playback`), the cutters' card and the vestibule HDMI (`aplay` on the hw PCM).
//!
//! The reader takes the pipe on its own clock (a PipeWire graph quantum, [`PW_GRAPH_BURST_FRAMES`],
//! or one ALSA period per period of the card's crystal / the HDMI audio clock). The hub writes one
//! 256-frame block per tick on its own clock. [`PipeFillControl`] decides every block write from
//! the measured pipe fill, and [`EgressQueue`] turns the decision into the samples written:
//!
//! - **The crossing is a resampler (issue 1461 lane 2, design comment 6376769274).** The hub's
//!   blocks queue up in hub frames; each write renders as many reader-clock frames as the queue
//!   holds from a fractional read point ([`crate::asrc::render_interleaved`], lane 1's Kaiser
//!   sinc), consuming `ratio` hub frames per written frame. The ratio follows the reader's rate,
//!   the least-squares slope of the frames it consumed (written minus the measured fill, at each
//!   detected read) against the hub's clock over 64 s ([`crate::asrc::RateControl::egress`]),
//!   followed at 5 ppm/s; a phase term only while the time-weighted fill sits outside +-1/4 of the
//!   setpoint, until it is back inside +-1/8; +-1000 ppm. No frame is ever inserted or dropped
//!   while the reader runs: a write is one frame shorter or longer than the block only because the
//!   resampler's output count follows the ratio.
//! - **A fixed setpoint, read at start** ([`pipe_setpoint_for`]): 1792 frames (37.3 ms) for a
//!   pw-cat pipe, 640 (13.3 ms) for aplay. It never moves while the hub runs.
//! - **The guards (last resort, counted, [`pipe_fill_plan`]).** A STARVED pipe is topped up with
//!   silence to [`PIPE_TARGET_FRAMES`] before the block; above [`PIPE_HIGH_FRAMES`] the block is
//!   dropped (a trim). The first write after a spawn reads 0 and gets the same top-up: the PRIME.
//!   A running pipe counts as starved only when its fill read under one hub block on consecutive
//!   readings for at least one hub period ([`PipeFillControl::plan_block`], design 5981457044):
//!   during a catch-up stall two of pw-cat's quantum reads can land in the write gap, and the pipe
//!   then reads low for a few ms with the late blocks about to arrive. A pw-cat blocked in its
//!   `fread` swallows every block the moment it lands, so a really starved pipe reads ~0 on every
//!   reading and is confirmed within one period. Every guard restarts the rate fit's series.
//! - **The start hold.** The reader takes nothing until its stream runs, and the hub keeps writing
//!   meanwhile. Until the fill first DROPS after the spawn, a block that would take the fill above
//!   [`PIPE_TARGET_FRAMES`] is dropped. So the first read always finds exactly the prime, whatever
//!   the reader's connect time.
//!
//! **The phase term reads the TIME-WEIGHTED fill, not the pre-write readings.** pw-cat takes four
//! hub blocks at once, so the pre-write readings repeat the same four values between two
//! read/write phase crossings. The sink thread also reads the fill about every
//! [`PIPE_SAMPLE_INTERVAL`] between blocks ([`PipeFillControl::sample`]); each block's depth is the
//! trapezoid mean of those readings since the last block (ROZHODNUTÉ 5979627274). The same
//! readings place each of the reader's reads within ~1 ms for the rate fit.
//!
//! Pure (no I/O, no external crate) and clocked by the caller (nanoseconds on any monotonic
//! clock), so the two-clock benches (`tests/egress_servo_1401.rs`, `tests/egress_asrc_1461.rs`) run
//! it on frame counts and samples alone. The pipe I/O is [`crate::local_audio::PipeFillWriter`].

use std::collections::VecDeque;
use std::time::Duration;

use crate::asrc::{self, RateControl, ReadPoint, ASRC_HISTORY, ASRC_LOOKAHEAD};
use crate::janus_pacing::hub_block_period;

/// The PipeWire graph quantum the MiniFuse drives on strih-snv (`clock.quantum 1024`, ALSA
/// `period-size 1024`, live-read 24.9.2026): a `pw-cat --record` child hands the hub its capture in
/// bursts of this many frames, whatever block size the hub pops, and a `pw-cat --playback` child
/// reads its stdin pipe in quanta of this size.
pub const PW_GRAPH_BURST_FRAMES: usize = 1024;

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

/// How often the sink thread reads the pipe fill between blocks for the time-weighted fill and the
/// read times (its `recv_timeout` on the egress channel). About 1000 `FIONREAD`s a second per sink,
/// accepted by ROZHODNUTÉ 5979627274.
pub const PIPE_SAMPLE_INTERVAL: Duration = Duration::from_millis(1);

/// The setpoint of a pw-cat pipe: the time-average depth the start hold leaves, the prime minus
/// half a graph quantum. pw-cat's first read finds the prime ([`PIPE_TARGET_FRAMES`] + one hub
/// block) and takes a whole quantum; the hub then writes the quantum back one block at a time
/// before the next read. Averaged over a read cycle, one spawn's pipe holds between the prime minus
/// a quantum plus 1.5 and 2.5 blocks, depending on where pw-cat's first read falls inside a hub
/// block (1664..1920 frames); the middle of that, over every such phase, is the prime minus half a
/// quantum: 2048 + 256 - 512 = 1792 frames (37.3 ms) at the 256-frame hub block. A spawn starts
/// within half a block (+-128 frames) of it, inside the phase term's dead band (+-448). The
/// resampler's queue holds its 16-frame look-ahead (0.33 ms) on top of the pipe.
pub const fn pipe_setpoint(block_frames: usize) -> usize {
    pipe_setpoint_for(block_frames, PW_GRAPH_BURST_FRAMES)
}

/// The fill a pipe whose reader takes `read_frames` at a time is primed to: two reads. pw-cat reads
/// a whole graph quantum ([`PIPE_TARGET_FRAMES`]); `aplay` on a hw PCM reads one ALSA period
/// (issue 1424 part 3: 256 frames, so 512 = 10.7 ms).
pub const fn pipe_target_frames_for(read_frames: usize) -> usize {
    2 * read_frames
}

/// The trim mark for that reader: the target + two more reads ([`PIPE_HIGH_FRAMES`] for pw-cat).
pub const fn pipe_high_frames_for(read_frames: usize) -> usize {
    pipe_target_frames_for(read_frames) + 2 * read_frames
}

/// The setpoint for that reader, derived exactly like [`pipe_setpoint`]: the prime minus half a
/// read. pw-cat at the 256-frame hub block: 1792; aplay reading 256-frame periods: 640 frames
/// (13.3 ms).
pub const fn pipe_setpoint_for(block_frames: usize, read_frames: usize) -> usize {
    (pipe_target_frames_for(read_frames) + block_frames).saturating_sub(read_frames / 2)
}

/// What one block write does to a playback pipe ([`PipeFillControl::plan_block`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeFillPlan {
    /// The pipe is starved (or freshly spawned) and holds less than one hub block: write
    /// `silence_frames` of silence, then the rendered audio, together the target plus one block
    /// (the prime also carries a reader's own buffer). On the prime the silence is the target plus
    /// the resampler's 16-frame look-ahead, which the first rendered block falls short of a block.
    TopUp { silence_frames: usize },
    /// The pipe holds more than [`PIPE_HIGH_FRAMES`]: drop the block (a trim).
    Drop,
    /// Healthy: write the rendered audio.
    Write,
    /// The reader has not read since the spawn and the block would take the fill above
    /// [`PIPE_TARGET_FRAMES`]: drop it (the start hold).
    StartHold,
}

/// What the resampler renders for one block (issue 1461): `frames` reader-clock frames from `read`
/// over the queued hub frames, after which the `consume` oldest queued frames are dropped. A
/// dropped or held block is rendered by none but still consumed, so the queue keeps its place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EgressRender {
    pub read: ReadPoint,
    pub frames: usize,
    pub consume: usize,
}

impl Default for EgressRender {
    fn default() -> Self {
        EgressRender {
            read: ReadPoint::at_prime(1.0),
            frames: 0,
            consume: 0,
        }
    }
}

/// The two guards of a block write from the pipe's measured fill (issue 1401). `fill_frames` is the
/// fill before the write, `block_frames` the block being written (one hub block), `starved` whether
/// the pipe is starved: the first write after a spawn (a fresh pipe reads 0), or a fill that read
/// under one block on consecutive readings for at least one hub period
/// ([`PipeFillControl::plan_block`] decides it, design 5981457044):
///
/// - starved and below one block: top up with silence to [`PIPE_TARGET_FRAMES`], then write the
///   block;
/// - above [`PIPE_HIGH_FRAMES`]: drop the block;
/// - otherwise (a momentary low included): write it. A healthy steady state never triggers either
///   guard.
///
/// [`PipeFillControl::plan_block`] adds the start hold to the `Write` case.
pub fn pipe_fill_plan(fill_frames: usize, block_frames: usize, starved: bool) -> PipeFillPlan {
    pipe_fill_plan_marks(
        fill_frames,
        block_frames,
        starved,
        PIPE_TARGET_FRAMES,
        PIPE_HIGH_FRAMES,
    )
}

/// [`pipe_fill_plan`] with its own top-up target and trim mark (a [`PipeFillControl`] built for
/// another reader, issue 1424).
pub fn pipe_fill_plan_marks(
    fill_frames: usize,
    block_frames: usize,
    starved: bool,
    target_frames: usize,
    high_frames: usize,
) -> PipeFillPlan {
    if starved && fill_frames < block_frames {
        PipeFillPlan::TopUp {
            silence_frames: target_frames.saturating_sub(fill_frames),
        }
    } else if fill_frames > high_frames {
        PipeFillPlan::Drop
    } else {
        PipeFillPlan::Write
    }
}

/// What one block write did, for the `local_audio` facet, the log and [`EgressQueue`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PipeWriteReport {
    /// The pipe fill measured before the write, in frames.
    pub fill_frames: usize,
    /// What the write did.
    pub plan: PipeFillPlan,
    /// The first write into this pipe (just after the spawn). Its top-up is the prime, not a
    /// refill: `pipe_refills` counts only a pipe that drained under a running reader.
    pub first: bool,
    /// What the resampler renders for this block.
    pub render: EgressRender,
}

impl Default for PipeWriteReport {
    fn default() -> Self {
        PipeWriteReport {
            fill_frames: 0,
            plan: PipeFillPlan::Write,
            first: false,
            render: EgressRender::default(),
        }
    }
}

impl PipeWriteReport {
    /// The frames this write puts into the pipe: the top-up's silence plus the rendered audio, or
    /// nothing for a trimmed or held block.
    pub fn written_frames(&self) -> usize {
        match self.plan {
            PipeFillPlan::TopUp { silence_frames } => silence_frames + self.render.frames,
            PipeFillPlan::Write => self.render.frames,
            PipeFillPlan::Drop | PipeFillPlan::StartHold => 0,
        }
    }
}

/// The resampler leg's live numbers for `/api/state` (issue 1461).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PipeAsrcStats {
    /// The mean time-weighted pipe fill over the last complete 1 s, in frames (0 before the first).
    pub depth_frames: usize,
    /// The fixed setpoint ([`pipe_setpoint_for`]).
    pub setpoint_frames: usize,
    /// The resampler's ratio in ppm off 1 (hub frames per written frame): a reader running fast
    /// reads negative.
    pub rate_ppm: f64,
    /// The last least-squares fit, in the same sense; `None` before the first after a (re)start.
    pub rate_fit_ppm: Option<f64>,
    /// The last second's mean depth minus the setpoint: what the phase term acts on outside +-1/4.
    pub phase_frames: f64,
}

/// The largest number of output frames a queue of `queued` frames renders from `read`.
fn renderable(read: ReadPoint, queued: usize) -> usize {
    if read.frames_needed(1) > queued {
        return 0;
    }
    let room = (queued - ASRC_LOOKAHEAD - 1) as f64 - read.pos;
    let mut n = ((room / read.ratio).floor() + 1.0).max(0.0) as usize;
    while n > 0 && read.frames_needed(n) > queued {
        n -= 1;
    }
    while read.frames_needed(n + 1) <= queued {
        n += 1;
    }
    n
}

/// The fill controller of one playback pipe from its spawn (see the module doc). A new reader
/// child gets a new controller.
#[derive(Debug, Clone)]
pub struct PipeFillControl {
    /// The pipe's sample rate (the hub's): one hub block's duration, the period a low fill must
    /// last before it counts as starved.
    sample_rate: u32,
    /// How many frames the pipe's reader takes at a time: pw-cat's graph quantum, or aplay's ALSA
    /// period (issue 1424). The prime, the trim mark and the setpoint follow it.
    read_frames: usize,
    /// The reader's own buffer a spawn fills before the pipe holds anything (aplay: its ALSA buffer
    /// plus the period it holds while blocked on the PCM; 0 for pw-cat). Added to the prime only.
    reader_buffer: usize,
    /// The hub block, as of the last planned write (0 before the prime).
    block_frames: usize,
    /// When the current run of readings under one block began; `None` while the last reading held
    /// at least one block.
    low_since: Option<u64>,
    /// A block has been planned since the spawn (the next one is not the prime).
    primed: bool,
    /// The reader has read from the pipe since the spawn: the start hold is over.
    started: bool,
    /// The last known fill (a reading, or what the last write left) and its time.
    last_fill: usize,
    last_at: u64,
    /// Twice the time-weighted integral of the fill since `period_at` (trapezoids), in frame-ns.
    area2: u128,
    period_at: u64,
    /// The resampler: the read point inside the queued hub frames, and how many are queued
    /// (history included; [`EgressQueue`] holds the same).
    read: ReadPoint,
    queued: usize,
    /// The rate controller and its series: the reader's frames consumed since the series started,
    /// since the spawn, and the total before which reads only fill the reader's own buffer.
    rate: RateControl,
    consumed: u64,
    consumed_total: u64,
    fit_from: u64,
    /// The 1 s depth window (blocks' time-weighted fills, the hub frames they span) and its last
    /// mean.
    win_sum: f64,
    win_blocks: u32,
    win_frames: usize,
    last_mean: Option<f64>,
}

impl PipeFillControl {
    /// A controller for a freshly spawned pw-cat pipe of `sample_rate` frames a second (the hub's
    /// rate): pw-cat reads a whole graph quantum ([`PW_GRAPH_BURST_FRAMES`]).
    pub fn new(sample_rate: u32) -> Self {
        Self::with_quantum(sample_rate, PW_GRAPH_BURST_FRAMES)
    }

    /// A controller for a pipe whose reader takes `read_frames` at a time (issue 1424: aplay on a
    /// hw PCM reads one 256-frame ALSA period, so its pipe holds 640 frames, not 1792).
    pub fn with_quantum(sample_rate: u32, read_frames: usize) -> Self {
        asrc::warm();
        PipeFillControl {
            sample_rate,
            read_frames: read_frames.max(1),
            reader_buffer: 0,
            block_frames: 0,
            low_since: None,
            primed: false,
            started: false,
            last_fill: 0,
            last_at: 0,
            area2: 0,
            period_at: 0,
            read: ReadPoint::at_prime(1.0),
            queued: ASRC_HISTORY,
            rate: RateControl::egress(sample_rate),
            consumed: 0,
            consumed_total: 0,
            fit_from: 0,
            win_sum: 0.0,
            win_blocks: 0,
            win_frames: 0,
            last_mean: None,
        }
    }

    /// The same controller whose reader first fills `frames` of its own buffer (issue 1424: aplay
    /// pulls its 1024-frame ALSA buffer + one period from the pipe the moment it starts). The prime
    /// adds them, so the pipe still holds the target once the reader is full and the first reads
    /// never starve it. The trim mark rises by the same amount: after a block-loop stall the catch-up
    /// burst lands in the pipe before aplay has moved it on into its drained buffer, and a trim there
    /// would cut real audio. The rate fit skips the reads that fill that buffer: they are the
    /// spawn's burst, not the reader's clock.
    ///
    /// After the prime such a pipe is never topped up with silence: the reader's own buffer is the
    /// reserve. A pipe left low (a block-loop stall the ALSA buffer rode out leaves aplay taking
    /// each block the moment it lands, the pipe under one block on every reading while ALSA stays
    /// full) is brought back by the phase term, never by a silent gap written in front of the
    /// audio; a reserve that really runs dry is the reader's own xrun, which aplay reports. pw-cat
    /// (no reader buffer) keeps the starvation top-up: it reads a whole graph quantum or xruns.
    pub fn with_reader_buffer(mut self, frames: usize) -> Self {
        self.reader_buffer = frames;
        self.fit_from = frames as u64;
        self
    }

    /// The trim mark: the target + two reads, plus the reader's own buffer
    /// ([`PipeFillControl::with_reader_buffer`]).
    fn high_frames(&self) -> usize {
        pipe_high_frames_for(self.read_frames) + self.reader_buffer
    }

    /// Make the next write a prime again (issue 1424: after the reader's own xrun it refills its
    /// whole buffer from the pipe, which then reads near empty; one prime top-up lands inside the gap
    /// the xrun already made). The start hold and the ratio stay as they are; the rate fit's series
    /// starts over after the reader's refill; the top-up still fires only when the pipe holds under
    /// one block.
    pub fn rearm_prime(&mut self) {
        self.primed = false;
        self.restart_series(self.reader_buffer);
    }

    /// Whether the reader has read since the spawn (the start hold is over).
    pub fn started(&self) -> bool {
        self.started
    }

    /// The rate fit's series starts over (a guard acted, the reader xrun'd); the reads that fill
    /// the next `skip` frames of the reader's own buffer are not points. The ratio is kept.
    fn restart_series(&mut self, skip: usize) {
        self.rate.restart();
        self.consumed = 0;
        self.fit_from = self.consumed_total + skip as u64;
    }

    /// The reader took `frames` between the last two readings, about `at_ns`: one point of the
    /// rate fit (unless it only filled the reader's own buffer).
    fn note_read(&mut self, frames: usize, at_ns: u64) {
        let before = self.consumed_total;
        self.consumed_total += frames as u64;
        if self.consumed_total <= self.fit_from {
            return;
        }
        self.consumed += self.consumed_total - before.max(self.fit_from);
        self.rate.arrival(at_ns as f64 / 1e9, self.consumed as f64);
    }

    /// One reading of the pipe fill between blocks, at `at_ns` on the caller's monotonic clock. A
    /// read by the reader between two readings is equally likely anywhere in between, so the
    /// interval counts as the trapezoid of its two readings, and a drop in the fill is a read at
    /// its middle.
    pub fn sample(&mut self, at_ns: u64, fill_frames: usize) {
        let at = at_ns.max(self.last_at);
        let span = u128::from(at - self.last_at);
        let sum = self.last_fill as u128 + fill_frames as u128;
        self.area2 = self.area2.saturating_add(sum.saturating_mul(span));
        if fill_frames < self.last_fill && (self.started || self.primed) {
            if !self.started {
                // The reader's first read: the start hold is over, the depth is measured from here.
                self.started = true;
                self.area2 = 0;
                self.period_at = at;
            }
            let mid = self.last_at + (at - self.last_at) / 2;
            self.note_read(self.last_fill - fill_frames, mid);
        }
        self.last_fill = fill_frames;
        self.last_at = at;
        if fill_frames < self.block_frames {
            self.low_since.get_or_insert(at);
        } else {
            self.low_since = None;
        }
    }

    /// Whether the fill has read under one block on every reading for at least one hub period up to
    /// `at`: the pipe is starved, not just low for a moment between a read and the next write. A
    /// reader with its own buffer is never starved after the prime (see
    /// [`PipeFillControl::with_reader_buffer`]).
    fn starved_at(&self, at: u64) -> bool {
        if self.reader_buffer > 0 {
            return false;
        }
        let period = hub_block_period(self.block_frames, self.sample_rate);
        self.low_since
            .is_some_and(|since| u128::from(at - since) >= period.as_nanos())
    }

    /// Plan the write of one `block_frames`-frame block, with `fill_frames` read from the pipe at
    /// `at_ns` just before it: the block joins the resampler's queue, the ratio takes its step,
    /// and the guards come first, then the start hold. A guard restarts the rate fit's series (like
    /// the VBAN legs' underrun and resync). The top-up guard fires only for the prime or a starved
    /// pipe (see [`pipe_fill_plan`]).
    pub fn plan_block(
        &mut self,
        at_ns: u64,
        fill_frames: usize,
        block_frames: usize,
    ) -> PipeWriteReport {
        self.block_frames = block_frames;
        self.sample(at_ns, fill_frames);
        let at = self.last_at;
        let first = !self.primed;
        self.primed = true;
        let starved = first || self.starved_at(at);
        let setpoint = pipe_setpoint_for(block_frames, self.read_frames);
        if self.started {
            let span = u128::from(at - self.period_at);
            let mean = match self.area2.checked_div(2 * span) {
                Some(mean) => mean as f64,
                // The start was seen at this very reading: nothing measured since but the reading.
                None => fill_frames as f64,
            };
            self.observe(mean, block_frames);
            let err = mean - setpoint as f64;
            self.read.ratio = self.rate.step(err, setpoint as f64 / 4.0, block_frames);
        }
        self.queued += block_frames;
        let frames = renderable(self.read, self.queued);
        let (next, consume) = self.read.advanced(frames);
        let render = EgressRender {
            read: self.read,
            frames,
            consume,
        };
        self.read = next;
        self.queued -= consume;
        let plan = match pipe_fill_plan_marks(
            fill_frames,
            block_frames,
            starved,
            pipe_target_frames_for(self.read_frames),
            self.high_frames(),
        ) {
            PipeFillPlan::Write if !self.started => {
                if fill_frames + frames > pipe_target_frames_for(self.read_frames) {
                    PipeFillPlan::StartHold
                } else {
                    PipeFillPlan::Write
                }
            }
            // Before the reader's first read a block over the trim mark is the start hold, not a
            // trim (issue 1424: an aplay prime holds the ALSA buffer too, above its 1024 mark).
            PipeFillPlan::Drop if !self.started => PipeFillPlan::StartHold,
            PipeFillPlan::TopUp { silence_frames } if first => PipeFillPlan::TopUp {
                silence_frames: silence_frames + self.reader_buffer,
            },
            PipeFillPlan::Write => PipeFillPlan::Write,
            guard => {
                if self.started {
                    self.restart_series(0);
                }
                guard
            }
        };
        // A top-up's silence also covers what the rendered audio falls short of a whole block (the
        // resampler's 16-frame look-ahead on the prime), so it always leaves the target plus one
        // block: the first read finds the same prime as before the resampler, and the setpoint is
        // still the time-average the start hold leaves.
        let plan = match plan {
            PipeFillPlan::TopUp { silence_frames } => PipeFillPlan::TopUp {
                silence_frames: silence_frames + block_frames.saturating_sub(frames),
            },
            other => other,
        };
        let report = PipeWriteReport {
            fill_frames,
            plan,
            first,
            render,
        };
        // The write is immediate: a new period starts with what it left in the pipe.
        self.last_fill = fill_frames + report.written_frames();
        self.area2 = 0;
        self.period_at = at;
        if matches!(plan, PipeFillPlan::TopUp { .. }) {
            // The pipe now holds the target: any low run before it is over. A plain block write
            // never ends one by itself, since a pw-cat blocked in its read swallows it at once;
            // the next reading tells.
            self.low_since = None;
        }
        report
    }

    /// Add one block's time-weighted fill to the 1 s depth window.
    fn observe(&mut self, mean: f64, block_frames: usize) {
        self.win_sum += mean;
        self.win_blocks += 1;
        self.win_frames += block_frames;
        if self.win_frames >= self.sample_rate as usize {
            self.last_mean = Some(self.win_sum / f64::from(self.win_blocks));
            self.win_sum = 0.0;
            self.win_blocks = 0;
            self.win_frames = 0;
        }
    }

    /// The resampler leg's numbers, once the reader runs (after its first read). The depth is the
    /// mean of the last complete second: 0 until the first one completes.
    pub fn asrc_stats(&self) -> Option<PipeAsrcStats> {
        if !self.started {
            return None;
        }
        let tenth = |x: f64| (x * 10.0).round() / 10.0;
        let setpoint = pipe_setpoint_for(self.block_frames, self.read_frames);
        Some(PipeAsrcStats {
            depth_frames: self.last_mean.map_or(0, |m| m.max(0.0).round() as usize),
            setpoint_frames: setpoint,
            rate_ppm: tenth(self.rate.rate_ppm()),
            rate_fit_ppm: self.rate.fitted_ppm().map(tenth),
            phase_frames: self.last_mean.map_or(0.0, |m| tenth(m - setpoint as f64)),
        })
    }
}

/// The samples side of one egress pipe (issue 1461): the hub frames given to the pipe and not yet
/// written (the resampler's queue, from [`ASRC_HISTORY`] frames of silence at the spawn), and what
/// each planned write ([`PipeFillControl::plan_block`]) puts into the pipe, as interleaved PCM16.
/// Pure, so a two-clock bench can push a tone through the real controller and grade what the
/// reader plays.
#[derive(Debug, Clone)]
pub struct EgressQueue {
    channels: usize,
    samples: VecDeque<i16>,
}

impl EgressQueue {
    /// A queue for `channels` interleaved channels, holding the resampler's history of silence.
    pub fn new(channels: usize) -> Self {
        let channels = channels.max(1);
        let mut samples = VecDeque::with_capacity((ASRC_HISTORY + 8 * 1024) * channels);
        samples.extend(std::iter::repeat_n(0i16, ASRC_HISTORY * channels));
        EgressQueue { channels, samples }
    }

    /// Queue `block` (one hub block), then return the interleaved samples the write `report`
    /// (planned for it) puts into the pipe: the top-up's silence, then the audio the resampler
    /// renders; `None` when the block is not written (a trim or the start hold). The rendered hub
    /// frames leave the queue either way.
    pub fn render(&mut self, report: &PipeWriteReport, block: &[i16]) -> Option<Vec<i16>> {
        let n = self.channels;
        let frames = block.len() / n;
        self.samples.extend(&block[..frames * n]);
        let r = report.render;
        let silence = match report.plan {
            PipeFillPlan::Drop | PipeFillPlan::StartHold => None,
            PipeFillPlan::Write => Some(0),
            PipeFillPlan::TopUp { silence_frames } => Some(silence_frames),
        };
        let out = silence.map(|silence| {
            let mut out = vec![0i16; (silence + r.frames) * n];
            let input = self.samples.make_contiguous();
            asrc::render_interleaved(r.read, input, n, &mut out[silence * n..]);
            out
        });
        let drop = (r.consume * n).min(self.samples.len());
        self.samples.drain(..drop);
        out
    }
}
