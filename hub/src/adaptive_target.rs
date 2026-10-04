//! The program feeds' adaptive target (issue 1401, design 5980775411).
//!
//! The program feeds (every VBAN leg that is not a cambox: the FOH `fohabl`, `lv1`, `mbc`) come
//! from Windows senders that do not pace their packets. The FOH desk's stalls grew during one day:
//! the largest gap between two packets was 19.4 ms in the morning and 27.6 ms in the afternoon (31
//! gaps over 20 ms in 30 s, 4.10.2026), so the fixed 32 ms target that covered the morning
//! underran again about 1.7 times a minute. Any fixed number is one bad hour away from that.
//!
//! [`AdaptiveTarget`] follows the sender instead:
//!
//! - it tracks the largest inter-arrival gap of the last 10 min ([`ADAPTIVE_WINDOW`], kept in
//!   buckets of [`ADAPTIVE_BUCKET`]);
//! - the target is that gap plus one hub block plus half a burst ([`PROGRAM_HALF_BURST_FRAMES`]),
//!   rounded up to whole blocks, between the floor [`VBAN_PROGRAM_TARGET_BLOCKS`] (32 ms) and the
//!   cap [`VBAN_PROGRAM_MAX_TARGET_BLOCKS`] (64 ms) ([`program_target_blocks`]);
//! - a larger gap raises the target at once; after [`ADAPTIVE_LOWER_AFTER`] (10 min) without a gap
//!   that needs it, the target comes down by one block, then by one more every 10 min.
//!
//! The leg applies a change through `NetworkFill::set_target`, so the existing drift servo walks the
//! fill to it: no jump and no click. The camboxes (Linux, evenly paced) keep their fixed target.
//!
//! Pure and std-only (clocked by the caller's `Instant`s), so it verifies with a rustc `--test`
//! replica under Tier-0 (issue 557).

use std::time::{Duration, Instant};

use crate::vban_jitter::VBAN_PROGRAM_TARGET_BLOCKS;

/// The highest target a program feed can get, in hub blocks: 64 ms at 256 frames / 48 kHz. A sender
/// whose gaps need more than that is a sender fault, not jitter.
pub const VBAN_PROGRAM_MAX_TARGET_BLOCKS: usize = 12;

/// Half a burst of the FOH sender, in frames (3 ms at 48 kHz). The Windows sender hands its packets
/// out in bursts about every 6 ms (live 4.10.2026: median inter-packet gap 0, p90 5.94 ms), so just
/// before a burst the fill sits about half a burst under its mean. The target covers that sawtooth
/// on top of the largest gap.
pub const PROGRAM_HALF_BURST_FRAMES: usize = 144;

/// How far back the largest gap is tracked: 10 min.
pub const ADAPTIVE_WINDOW: Duration = Duration::from_secs(600);

/// The window's resolution: the largest gap is kept per 10 s bucket, so the window is 590-600 s.
pub const ADAPTIVE_BUCKET: Duration = Duration::from_secs(10);

/// How long a target stays after the last gap that needed it before it comes down by one block,
/// and between two such steps: 10 min.
pub const ADAPTIVE_LOWER_AFTER: Duration = Duration::from_secs(600);

const BUCKETS: usize = (ADAPTIVE_WINDOW.as_secs() / ADAPTIVE_BUCKET.as_secs()) as usize;

/// The target in hub blocks that a largest gap of `gap` needs: the gap plus one block plus half a
/// burst, rounded up to whole blocks, between [`VBAN_PROGRAM_TARGET_BLOCKS`] and
/// [`VBAN_PROGRAM_MAX_TARGET_BLOCKS`].
pub fn program_target_blocks(gap: Duration, block_frames: usize, sample_rate: u32) -> usize {
    let block = block_frames.max(1);
    let gap_frames = (gap.as_nanos() * u128::from(sample_rate)).div_ceil(1_000_000_000);
    let need = usize::try_from(gap_frames)
        .unwrap_or(usize::MAX)
        .saturating_add(block + PROGRAM_HALF_BURST_FRAMES);
    need.div_ceil(block)
        .clamp(VBAN_PROGRAM_TARGET_BLOCKS, VBAN_PROGRAM_MAX_TARGET_BLOCKS)
}

/// One 10 s bucket of the window: which bucket it is and its largest gap.
#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    index: Option<u64>,
    max_gap: Duration,
}

/// The adaptive target of one program feed (see the module doc).
#[derive(Debug, Clone)]
pub struct AdaptiveTarget {
    block_frames: usize,
    sample_rate: u32,
    target_blocks: usize,
    origin: Option<Instant>,
    buckets: [Bucket; BUCKETS],
    /// The last raise or lowering (or the first gap): the 10 min hold counts from here.
    last_change: Option<Instant>,
    /// The window's largest gap as of the last observation.
    window_max: Duration,
}

impl AdaptiveTarget {
    /// A program feed's target, starting at the floor [`VBAN_PROGRAM_TARGET_BLOCKS`].
    pub fn new(block_frames: usize, sample_rate: u32) -> Self {
        AdaptiveTarget {
            block_frames: block_frames.max(1),
            sample_rate,
            target_blocks: VBAN_PROGRAM_TARGET_BLOCKS,
            origin: None,
            buckets: [Bucket::default(); BUCKETS],
            last_change: None,
            window_max: Duration::ZERO,
        }
    }

    /// The current target in frames.
    pub fn target_frames(&self) -> usize {
        self.target_blocks * self.block_frames
    }

    /// The largest inter-arrival gap of the last 10 min, as of the last observation.
    pub fn max_gap_10min(&self) -> Duration {
        self.window_max
    }

    /// A packet arrived at `now`, `gap` after the one before it (the caller passes only gaps inside
    /// a running stream, never the silence before a stalled stream came back). Returns the new
    /// target in frames when it changed: raised at once when the window's largest gap needs more,
    /// lowered by one block when the target has held for [`ADAPTIVE_LOWER_AFTER`] without a gap
    /// that needs it.
    pub fn observe(&mut self, gap: Duration, now: Instant) -> Option<usize> {
        let origin = *self.origin.get_or_insert(now);
        let last_change = *self.last_change.get_or_insert(now);
        let index = now.saturating_duration_since(origin).as_secs() / ADAPTIVE_BUCKET.as_secs();
        let slot = &mut self.buckets[(index % BUCKETS as u64) as usize];
        if slot.index != Some(index) {
            *slot = Bucket {
                index: Some(index),
                max_gap: Duration::ZERO,
            };
        }
        slot.max_gap = slot.max_gap.max(gap);
        self.window_max = self
            .buckets
            .iter()
            .filter(|b| b.index.is_some_and(|i| i + BUCKETS as u64 > index))
            .map(|b| b.max_gap)
            .max()
            .unwrap_or(Duration::ZERO);
        let need = program_target_blocks(self.window_max, self.block_frames, self.sample_rate);
        if need > self.target_blocks {
            self.target_blocks = need;
        } else if need < self.target_blocks
            && now.saturating_duration_since(last_change) >= ADAPTIVE_LOWER_AFTER
        {
            self.target_blocks -= 1;
        } else {
            return None;
        }
        self.last_change = Some(now);
        Some(self.target_frames())
    }
}
