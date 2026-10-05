//! The hub block loop's clock (issue 1401, design 5980775411): the exact block grid, the ticks a
//! wake finds due, and the catch-up plan.
//!
//! The block loop pops one block from every input, mixes N-1 and writes every output once per
//! tick (256 frames = 5.33 ms at 48 kHz). It used to be a tokio task on `tokio::time::interval`
//! with `MissedTickBehavior::Skip`: a wake more than a period late (a few ms of contention on the
//! strih-lx E-cores, the dantesync NTP bursts) skipped the missed ticks, and every output lost
//! those blocks, a click in the program audio, the operator's cans and every cambox headset (18
//! missed ticks in one hour, 4.10.2026).
//!
//! Now the loop runs on its own real-time thread ([`crate::mix_thread`]) and sleeps to absolute
//! deadlines on [`BlockGrid`]. When a wake finds k missed ticks it runs them all at once, in order,
//! before the current one ([`catch_up_plan`], [`run_batch`]), up to [`CATCHUP_MAX_BLOCKS`]. The
//! buffers such a late burst touches hold more than a block or two: the VBAN legs at least 16 ms
//! (the program feeds at least 32 ms), the `pw-cat` pipes about 37 ms on average, and every cambox
//! has its own receive jitter buffer. Only the part beyond it is given up, through the VBAN legs'
//! `skip_missed`, and counted (`lost_ticks`).
//!
//! What the two-clock bench (`tests/hub_catchup_1401.rs`) measured since step 4 (design
//! 5981457044): the VBAN legs stay clean up to a 26 ms late wake (four ticks run late), because
//! every leg's cap holds what arrives while the loop is late
//! ([`crate::vban_jitter::vban_cap_blocks`]). Both `pw-cat` pipes stay refill-free up to 24 ms: a
//! pipe that reads under one block for a moment when two of its quantum reads land inside the
//! write gap is no longer topped up, only one starved for a whole hub period is. On 25-26 ms
//! stalls the drifting cans pipe still refills now and then (3-6 in 57 stalls), and from 24 ms a
//! pw-cat read inside the gap can come up short. Past the catch-up a stall is a counted loss,
//! never a silent one.
//!
//! Pure and std-only, so it verifies with a rustc `--test` replica under Tier-0 (issue 557).

use std::time::Duration;

/// The most missed ticks one wake runs late: 4 blocks (21.3 ms at 256 frames / 48 kHz, design
/// 5980775411). Every VBAN leg's cap is derived from it ([`crate::vban_jitter::vban_cap_blocks`]:
/// the target + these ticks + the current one + one block of headroom), so the blocks that arrive
/// while the loop is up to 4 ticks late never trim a leg. Beyond this the part past the first four
/// is given up instead (`TickBatch::lost`).
pub const CATCHUP_MAX_BLOCKS: u64 = 4;

/// What one wake of the block loop does with the ticks that are due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickBatch {
    /// Missed ticks run now, oldest first, before the current one: each one a whole cycle (pop,
    /// mix, send, write), so every output gets its block late instead of never.
    pub catch_up: u64,
    /// Missed ticks beyond [`CATCHUP_MAX_BLOCKS`]: given up. Every output loses these blocks, and
    /// the VBAN legs drop as many blocks of their oldest audio (`JitterBuffer::skip_missed`) right
    /// before the current cycle's pop, so their depth stays where those pops would have left it.
    pub lost: u64,
}

impl TickBatch {
    /// The cycles this wake runs: the catch-up ones plus the current one.
    pub fn cycles(&self) -> u64 {
        self.catch_up + 1
    }
}

/// Split `missed` ticks into the ones run late (at most [`CATCHUP_MAX_BLOCKS`]) and the ones given
/// up.
pub fn catch_up_plan(missed: u64) -> TickBatch {
    let catch_up = missed.min(CATCHUP_MAX_BLOCKS);
    TickBatch {
        catch_up,
        lost: missed - catch_up,
    }
}

/// Run one wake's cycles in order: every catch-up cycle with nothing given up, then the current
/// one, which first gives up the lost part. `cycle(lost)` is one tick: the VBAN legs skip `lost`
/// blocks, then every input pops, the engine mixes and every output is written.
///
/// The lost part goes before the CURRENT pop, not before the catch-up pops: the give-up keeps a leg
/// at least half a block under its target for the ONE pop that follows it, so given up first, the
/// catch-up pops would drain the leg up to [`CATCHUP_MAX_BLOCKS`] blocks below that floor.
pub fn run_batch(batch: TickBatch, mut cycle: impl FnMut(u64)) {
    for _ in 0..batch.catch_up {
        cycle(0);
    }
    cycle(batch.lost);
}

/// The block loop's grid: tick `n` is due `n x block_frames / sample_rate` s after the loop's
/// origin, computed exactly in nanoseconds (rounded down). That is
/// [`crate::janus_pacing::hub_block_period`] per tick to within 1 ns, without the per-tick
/// truncation summing up (0.33 ns a tick at 256 frames / 48 kHz, 62.5 ppb), so the loop never
/// drifts against its 48 kHz consumers. Each deadline depends only on `n`, never on when an earlier
/// wake happened: a late wake never moves the grid.
#[derive(Debug, Clone)]
pub struct BlockGrid {
    block_frames: u128,
    sample_rate: u128,
    /// The next tick to run.
    next: u64,
}

const NS_PER_S: u128 = 1_000_000_000;

impl BlockGrid {
    /// The grid of `block_frames`-frame blocks at `sample_rate`, from tick 0 at the origin (both are
    /// clamped to at least 1; the matrix already refuses 0).
    pub fn new(block_frames: usize, sample_rate: u32) -> Self {
        BlockGrid {
            block_frames: (block_frames as u128).max(1),
            sample_rate: u128::from(sample_rate.max(1)),
            next: 0,
        }
    }

    /// When tick `n` is due, from the origin.
    pub fn deadline(&self, n: u64) -> Duration {
        let ns = u128::from(n) * self.block_frames * NS_PER_S / self.sample_rate;
        Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
    }

    /// The deadline of the next tick to run: the loop sleeps until it.
    pub fn next_deadline(&self) -> Duration {
        self.deadline(self.next)
    }

    /// The next tick to run.
    pub fn next_tick(&self) -> u64 {
        self.next
    }

    /// The last tick due at `now` (from the origin): the largest `n` with `deadline(n) <= now`.
    /// `deadline(n) <= now` is `n x B x 1e9 < (now + 1) x R` with the rounding down, hence the
    /// closed form.
    fn last_due(&self, now: Duration) -> u64 {
        let lhs = (now.as_nanos() + 1) * self.sample_rate - 1;
        u64::try_from(lhs / (self.block_frames * NS_PER_S)).unwrap_or(u64::MAX)
    }

    /// The loop woke at `now` (from the origin): take every tick due by then, from the next one.
    /// `None` = nothing is due yet (an early wake; sleep again). Otherwise the batch: the ticks
    /// before the newest due one are missed, split by [`catch_up_plan`]; the grid moves past all of
    /// them, so a lost tick is never run later.
    pub fn take_due(&mut self, now: Duration) -> Option<TickBatch> {
        let last = self.last_due(now);
        if last < self.next {
            return None;
        }
        let missed = last - self.next;
        self.next = last.saturating_add(1);
        Some(catch_up_plan(missed))
    }
}
