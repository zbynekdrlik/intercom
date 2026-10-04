//! Which ingress buffer each participant gets (issues 1345 + 1401).
//!
//! The block loop pops one block per participant from these buffers. The choice depends only on
//! the declared matrix, so it lives here (pure, testable against the deployed TOML) rather than in
//! the daemon's `main`.

use std::collections::HashSet;

use crate::local_audio::{LOCAL_CAPTURE_CAP_BLOCKS, LOCAL_CAPTURE_TARGET_FRAMES};
use crate::matrix::{Matrix, ADAPTER_JANUS, ADAPTER_VBAN};
use crate::vban_io::JitterBuffer;
use crate::vban_jitter::{VBAN_CAP_BLOCKS, VBAN_TARGET_BLOCKS};

/// One input buffer per participant, id-indexed like `matrix.participants`:
///
/// - a local PipeWire capture (the MiniFuse talkback) and the Janus (phones) ingress get the
///   local-capture target-fill ring that absorbs their 1024- / 960-frame bursts (issue 1345: the
///   generic no-prefill buffer spliced ~8x/s, and the phones leg overran ~60x/s);
/// - every VBAN leg (the FOH program feed, the camboxes' talkback) gets the VBAN-leg buffer with
///   its target fill and drift servo (issue 1401: the no-target buffer zero-padded a block whenever
///   a packet was a little late, dropouts in the strih program audio);
/// - a participant with no ingress (`adapter = "none"`, the `program_out` sink) gets the plain
///   buffer.
///
/// Every buffer fans a mono packet into ch2 for a participant with >= 2 input channels (the
/// camboxes send mono VBAN).
pub fn input_buffers(matrix: &Matrix) -> Vec<JitterBuffer> {
    let block_frames = matrix.hub.block_frames;
    let cap = block_frames * VBAN_CAP_BLOCKS;
    let local_capture_ids: HashSet<usize> = matrix
        .local_inputs()
        .into_iter()
        .map(|(pid, _, _)| pid)
        .collect();
    matrix
        .participants
        .iter()
        .enumerate()
        .map(|(id, p)| {
            let jb = if local_capture_ids.contains(&id) || p.adapter == ADAPTER_JANUS {
                JitterBuffer::local_capture(
                    block_frames * LOCAL_CAPTURE_CAP_BLOCKS,
                    LOCAL_CAPTURE_TARGET_FRAMES,
                )
            } else if p.adapter == ADAPTER_VBAN {
                JitterBuffer::vban_leg(cap, block_frames * VBAN_TARGET_BLOCKS)
            } else {
                JitterBuffer::new(cap)
            };
            jb.with_min_channels(p.in_channels)
        })
        .collect()
}
