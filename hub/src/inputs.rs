//! Which ingress buffer each participant gets (issues 1345 + 1401 + 1461).
//!
//! The block loop pops one block per participant from these buffers. The choice depends only on
//! the declared matrix, so it lives here (pure, testable against the deployed TOML) rather than in
//! the daemon's `main`.

use std::collections::HashSet;

use crate::local_audio::{LOCAL_CAPTURE_CAP_BLOCKS, LOCAL_CAPTURE_TARGET_FRAMES};
use crate::matrix::{Matrix, Participant, ADAPTER_JANUS, ADAPTER_VBAN, CAMBOX_ROLE};
use crate::vban_io::JitterBuffer;
use crate::vban_jitter::{
    target_frames_from_ms, vban_cap_blocks, CARD_CAPTURE_TARGET_FRAMES, VBAN_CAP_BLOCKS,
    VBAN_PROGRAM_TARGET_BLOCKS, VBAN_TARGET_BLOCKS,
};

/// A participant's fixed ingress target in frames (issue 1461): its TOML `target_ms` rounded up to
/// whole blocks, else `default_frames`. Read once, at start.
pub fn ingress_target_frames(matrix: &Matrix, p: &Participant, default_frames: usize) -> usize {
    p.target_ms
        .and_then(|ms| target_frames_from_ms(ms, matrix.hub.sample_rate, matrix.hub.block_frames))
        .unwrap_or(default_frames)
}

/// One input buffer per participant, id-indexed like `matrix.participants`:
///
/// - every VBAN leg (the FOH program feed, the camboxes' talkback) gets the resampler leg with a
///   FIXED target (issue 1461): its `target_ms`, else a cambox [`VBAN_TARGET_BLOCKS`] (16 ms) and
///   every other VBAN leg, a program feed from a Windows sender that bursts,
///   [`VBAN_PROGRAM_TARGET_BLOCKS`] (37.3 ms). The cap is [`vban_cap_blocks`] of the target;
/// - the cutters' local capture (since issue 1438 the card's own `arecord`, else a PipeWire
///   capture) gets the same resampler leg ([`JitterBuffer::card_capture`]): the card crystal is a
///   foreign clock too. Its target is its `target_ms`, else [`CARD_CAPTURE_TARGET_FRAMES`] (two of
///   the card's 1024-frame bursts), its cap the issue-1345 ring's 32 blocks;
/// - the Janus (phones) ingress keeps the local-capture target-fill ring that absorbs its
///   960-frame bursts (issue 1345; its crossing is issue 1461's second lane);
/// - a participant with no ingress (`adapter = "none"`, the `program_out` sink) gets the plain
///   buffer.
///
/// Every buffer fans a mono packet into ch2 for a participant with >= 2 input channels (the
/// camboxes send mono VBAN).
pub fn input_buffers(matrix: &Matrix) -> Vec<JitterBuffer> {
    let block_frames = matrix.hub.block_frames;
    let rate = matrix.hub.sample_rate;
    let capture_ids: HashSet<usize> = matrix
        .local_inputs()
        .into_iter()
        .map(|(pid, _, _)| pid)
        .chain(
            matrix
                .alsa_cards()
                .into_iter()
                .filter(|c| c.in_channels > 0)
                .map(|c| c.participant),
        )
        .collect();
    matrix
        .participants
        .iter()
        .enumerate()
        .map(|(id, p)| {
            let jb = if p.adapter == ADAPTER_JANUS {
                JitterBuffer::local_capture(
                    block_frames * LOCAL_CAPTURE_CAP_BLOCKS,
                    LOCAL_CAPTURE_TARGET_FRAMES,
                )
            } else if capture_ids.contains(&id) {
                let target = ingress_target_frames(matrix, p, CARD_CAPTURE_TARGET_FRAMES);
                let cap = (block_frames * LOCAL_CAPTURE_CAP_BLOCKS).max(target + block_frames);
                JitterBuffer::card_capture(cap, target, rate)
            } else if p.adapter == ADAPTER_VBAN {
                let blocks = if p.role == CAMBOX_ROLE {
                    VBAN_TARGET_BLOCKS
                } else {
                    VBAN_PROGRAM_TARGET_BLOCKS
                };
                let target = ingress_target_frames(matrix, p, block_frames * blocks);
                let cap = block_frames * vban_cap_blocks(target.div_ceil(block_frames.max(1)));
                JitterBuffer::vban_leg_at(cap, target, rate)
            } else {
                JitterBuffer::new(block_frames * VBAN_CAP_BLOCKS)
            };
            jb.with_min_channels(p.in_channels)
        })
        .collect()
}
