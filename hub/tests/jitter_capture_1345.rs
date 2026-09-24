//! issue 1345 (24.9.2026 production fix): the hub's per-participant input buffer.
//!
//! Three defects, each pinned here against the public [`JitterBuffer`] API:
//!
//! * (a) the LOCAL CAPTURE ring. The MiniFuse drives the PipeWire graph at quantum 1024, so the
//!   `pw-cat --record` child delivers >= 1024-frame bursts that the hub pops as 256-frame blocks.
//!   The generic 2048-frame, no-prefill buffer spliced about 8 times a second (4 underruns/s +
//!   4 overruns/s live). The capture ring prefills to a target fill before the first pop. After an
//!   underrun it waits to refill without zero-splicing a partial block. On overrun it drops back
//!   down to the target.
//! * (e) a STALE stream (no packet for > `STALE_STREAM_MS`) no longer counts an underrun every
//!   block, and its level reads as silence (-120 dBFS) instead of the last packet's level forever.
//! * the mono cambox fan-out: a cambox sends MONO VBAN, so a mono packet for a participant
//!   declared with >= 2 input channels is copied into ch2 too. Without that, the cam comes out
//!   left-only and 6 dB quieter after the phones' stereo->mono average.

use std::time::{Duration, Instant};

use intercom_hub::vban_io::{DecodedAudio, JitterBuffer, STALE_STREAM_MS};

const BLOCK: usize = 256;
const BURST: usize = 1024;
const TARGET: usize = 2 * BURST;
const CAP: usize = 32 * BLOCK;

/// A stereo packet whose two channels carry a contiguous ramp starting at `start` (so any splice —
/// a dropped or zero-filled sample — shows up as a discontinuity in the popped sequence).
fn stereo_ramp(start: i64, frames: usize) -> DecodedAudio {
    let ch: Vec<i16> = (0..frames as i64)
        .map(|i| ((start + i) % 30000) as i16)
        .collect();
    DecodedAudio {
        stream_name: "cutters".into(),
        channels: vec![ch.clone(), ch],
        frames,
    }
}

fn mono(samples: Vec<i16>) -> DecodedAudio {
    let frames = samples.len();
    DecodedAudio {
        stream_name: "cam2".into(),
        channels: vec![samples],
        frames,
    }
}

#[test]
fn capture_ring_prefills_to_the_target_before_the_first_pop() {
    let mut jb = JitterBuffer::local_capture(CAP, TARGET);
    let t0 = Instant::now();
    jb.push_at(&stereo_ramp(1, BURST), t0);
    // Below the target: the pop is silence and consumes NOTHING (prefill), and no underrun counts.
    let b = jb.pop_block_at(BLOCK, t0);
    assert!(b.iter().all(|c| c.iter().all(|&s| s == 0)));
    assert_eq!(jb.buffered_frames(), BURST, "prefill must not consume");
    assert_eq!(jb.underruns, 0);
    // A second burst reaches the target: audio starts at the very first captured sample.
    jb.push_at(&stereo_ramp(1 + BURST as i64, BURST), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b[0][0], 1,
        "the first popped sample is the first captured one"
    );
    assert_eq!(b[0][BLOCK - 1], BLOCK as i16);
    assert_eq!(jb.buffered_frames(), TARGET - BLOCK);
}

#[test]
fn capture_ring_absorbs_1024_frame_bursts_without_a_single_splice() {
    // pw-cat delivers one 1024-frame burst per 4 hub ticks (quantum 1024 vs 256-frame blocks),
    // sometimes a tick LATE. The popped audio must be one unbroken ramp: 0 underruns, 0 overruns.
    let mut jb = JitterBuffer::local_capture(CAP, TARGET);
    let t0 = Instant::now();
    let mut next_push: i64 = 1;
    let mut popped: Vec<i16> = Vec::new();
    for tick in 0..4000u64 {
        let now = t0 + Duration::from_micros(tick * 5333);
        // A burst every 4 ticks; every 7th burst arrives one tick late.
        let burst_idx = tick / 4;
        let due = if burst_idx % 7 == 3 {
            tick % 4 == 1
        } else {
            tick % 4 == 0
        };
        if due {
            jb.push_at(&stereo_ramp(next_push, BURST), now);
            next_push += BURST as i64;
        }
        let b = jb.pop_block_at(BLOCK, now);
        if b[0].iter().any(|&s| s != 0) || !popped.is_empty() {
            popped.extend_from_slice(&b[0]);
        }
    }
    assert_eq!(
        jb.underruns, 0,
        "bursty-but-steady capture must never underrun"
    );
    assert_eq!(
        jb.overruns, 0,
        "bursty-but-steady capture must never overrun"
    );
    for (i, w) in popped.windows(2).enumerate() {
        assert_eq!(
            (w[1] as i64 - w[0] as i64).rem_euclid(30000),
            1,
            "splice at popped sample {i}: {} -> {}",
            w[0],
            w[1]
        );
    }
    assert!(popped.len() > 3000 * BLOCK);
}

#[test]
fn capture_ring_underrun_waits_to_refill_and_never_zero_splices_a_partial_block() {
    let mut jb = JitterBuffer::local_capture(CAP, TARGET);
    let t0 = Instant::now();
    jb.push_at(&stereo_ramp(1, TARGET + 100), t0);
    // Drain every full block: TARGET + 100 frames = 8 full blocks + 100 left over.
    for _ in 0..(TARGET / BLOCK) {
        let b = jb.pop_block_at(BLOCK, t0);
        assert!(b[0][0] != 0);
    }
    assert_eq!(jb.buffered_frames(), 100);
    // 100 < 256: the pop is a WHOLE silent block, counts one underrun, and keeps the 100 frames.
    let b = jb.pop_block_at(BLOCK, t0);
    assert!(b[0].iter().all(|&s| s == 0), "never a partial-voice block");
    assert_eq!(jb.underruns, 1);
    assert_eq!(jb.buffered_frames(), 100, "the leftover voice is kept");
    // Still refilling: silence again, no second underrun.
    jb.push_at(&stereo_ramp(1 + (TARGET + 100) as i64, BURST), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert!(b[0].iter().all(|&s| s == 0));
    assert_eq!(
        jb.underruns, 1,
        "waiting to refill is one underrun, not one per block"
    );
    // Back at the target: the voice resumes exactly where it stopped.
    jb.push_at(&stereo_ramp(1 + (TARGET + 100 + BURST) as i64, BURST), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(b[0][0], (TARGET + 1) as i16, "resumes at the kept leftover");
    for w in b[0].windows(2) {
        assert_eq!(w[1] - w[0], 1);
    }
}

#[test]
fn capture_ring_overrun_drops_back_to_the_target_keeping_the_newest_audio() {
    let mut jb = JitterBuffer::local_capture(CAP, TARGET);
    let t0 = Instant::now();
    let total = CAP + BLOCK;
    jb.push_at(&stereo_ramp(1, total), t0);
    assert_eq!(jb.overruns, 1);
    assert_eq!(
        jb.buffered_frames(),
        TARGET,
        "an overrun drops down to the target"
    );
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b[0][0],
        (total - TARGET + 1) as i16,
        "the newest TARGET frames are kept"
    );
}

#[test]
fn a_stale_stream_counts_no_underruns_and_reads_as_silence() {
    // A cambox whose mic went muted keeps its buffer "previously live" forever; the old code counted
    // one underrun per 5.3 ms block from then on (187/s) and kept showing its last level.
    let mut jb = JitterBuffer::new(8 * BLOCK);
    let t0 = Instant::now();
    jb.push_at(&mono(vec![16384; BLOCK]), t0);
    let _ = jb.pop_block_at(BLOCK, t0);
    assert_eq!(jb.underruns, 0);
    // Live stream, a late packet: that IS an underrun.
    let _ = jb.pop_block_at(BLOCK, t0 + Duration::from_millis(10));
    assert_eq!(jb.underruns, 1);
    let live_level = jb.last_level_dbfs_at(t0 + Duration::from_millis(10));
    assert!(
        (live_level - (-6.02)).abs() < 0.1,
        "live level {live_level}"
    );
    // Stale: no more underruns, and the level reads silence.
    let stale = t0 + Duration::from_millis(STALE_STREAM_MS + 1);
    for _ in 0..200 {
        let _ = jb.pop_block_at(BLOCK, stale);
    }
    assert_eq!(jb.underruns, 1, "a stale stream must not count underruns");
    assert_eq!(jb.last_level_dbfs_at(stale), -120.0);
    // The stream comes back: counting resumes.
    let back = stale + Duration::from_millis(1);
    jb.push_at(&mono(vec![100; 10]), back);
    let _ = jb.pop_block_at(BLOCK, back);
    assert_eq!(jb.underruns, 2);
}

#[test]
fn a_mono_packet_fans_ch1_into_ch2_for_a_stereo_participant() {
    let mut jb = JitterBuffer::new(8 * BLOCK).with_min_channels(2);
    jb.push(&mono(vec![1, 2, 3]));
    let b = jb.pop_block(3);
    assert_eq!(b.len(), 2, "a 2-channel participant pops 2 channels");
    assert_eq!(b[0], vec![1, 2, 3]);
    assert_eq!(b[1], vec![1, 2, 3], "ch1 copied into ch2");

    // An 8-channel participant (cam2) also gets ch1 into ch2 — never beyond (the matrix routes
    // ch1->out1 and ch2->out2).
    let mut jb8 = JitterBuffer::new(8 * BLOCK).with_min_channels(8);
    jb8.push(&mono(vec![5, 6]));
    let b = jb8.pop_block(2);
    assert_eq!(b.len(), 2);
    assert_eq!(b[1], vec![5, 6]);

    // A mono participant stays mono, and a real stereo packet is never touched.
    let mut jb1 = JitterBuffer::new(8 * BLOCK).with_min_channels(1);
    jb1.push(&mono(vec![7]));
    assert_eq!(jb1.pop_block(1).len(), 1);
    let mut jbs = JitterBuffer::new(8 * BLOCK).with_min_channels(2);
    jbs.push(&DecodedAudio {
        stream_name: "cam1".into(),
        channels: vec![vec![1], vec![-1]],
        frames: 1,
    });
    assert_eq!(jbs.pop_block(1), vec![vec![1], vec![-1]]);
}
