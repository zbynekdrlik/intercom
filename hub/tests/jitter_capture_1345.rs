//! issue 1345 (24.9.2026 production fix): the hub's per-participant input buffer.
//!
//! Three defects, each pinned here against the public [`JitterBuffer`] API:
//!
//! * (a) the LOCAL CAPTURE ring. The MiniFuse drives the PipeWire graph at quantum 1024, so the
//!   `pw-cat --record` child delivers >= 1024-frame bursts that the hub pops as 256-frame blocks.
//!   The generic 2048-frame, no-prefill buffer spliced about 8 times a second (4 underruns/s +
//!   4 overruns/s live). Issue 1461 replaced that ring with the resampler leg (the card capture in
//!   lane 1, the phones in lane 2), whose prefill, whole-silent-block underruns and resyncs are
//!   pinned in `vban_jitter_1401.rs` / `asrc_1461.rs`; the ring and its tests here are gone.
//! * (e) a STALE stream (no packet for > `STALE_STREAM_MS`) no longer counts an underrun every
//!   block, and its level reads as silence (-120 dBFS) instead of the last packet's level forever.
//! * the mono cambox fan-out: a cambox sends MONO VBAN, so a mono packet for a participant
//!   declared with >= 2 input channels is copied into ch2 too. Without that, the cam comes out
//!   left-only and 6 dB quieter after the phones' stereo->mono average.

use std::time::{Duration, Instant};

use intercom_hub::vban_io::{DecodedAudio, JitterBuffer, STALE_STREAM_MS};

const BLOCK: usize = 256;

fn mono(samples: Vec<i16>) -> DecodedAudio {
    let frames = samples.len();
    DecodedAudio {
        stream_name: "cam2".into(),
        channels: vec![samples],
        frames,
    }
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
