//! Issue 1401, live 4.10.2026: the FOH program feed (`fohabl`, a Windows VBAN sender) does not send
//! evenly. A 12 s capture on strih-lx: 932 packets/s in bursts about every 6 ms, and a gap of up to
//! 19.4 ms (30 gaps over 12 ms in those 12 s). With the 3-block (16 ms) VBAN target the leg ran dry
//! about every 2-3 s (`underruns=26(fohabl)` -> `48(fohabl)` in one minute, no missed hub tick), so
//! the strih program audio still dropped out. The program feeds (every VBAN leg that is not a
//! cambox) get a target that covers that gap plus one hub block; the camboxes (Linux, even pacing)
//! keep the 3-block target and its lower talkback latency.

use std::time::{Duration, Instant};

use intercom_hub::inputs::input_buffers;
use intercom_hub::matrix::{Matrix, ADAPTER_VBAN, CAMBOX_ROLE};
use intercom_hub::vban_io::{DecodedAudio, JitterBuffer};
use intercom_hub::vban_jitter::{
    VBAN_CAP_BLOCKS, VBAN_PROGRAM_CAP_BLOCKS, VBAN_PROGRAM_TARGET_BLOCKS, VBAN_TARGET_BLOCKS,
};

const BLOCK: usize = 256;
const RATE: u64 = 48_000;

/// The measured worst gap between two fohabl bursts (4.10.2026), in microseconds.
const MEASURED_MAX_GAP_US: u64 = 19_400;

fn burst(frames: usize, first: i16) -> DecodedAudio {
    DecodedAudio {
        stream_name: "fohabl-strih".into(),
        channels: vec![(0..frames).map(|i| first.wrapping_add(i as i16)).collect()],
        frames,
    }
}

/// Plays `seconds` of the measured fohabl pattern into `jb` and counts its underruns:
/// - the sender produces audio at exactly 48 kHz and hands it out in a burst every 6 ms;
/// - every 402 ms (67 bursts) the burst after one on time is held back so that the next arrival
///   comes the measured 19.4 ms later, carrying everything produced meanwhile (nothing is lost,
///   as the capture showed);
/// - the hub pops one 256-frame block every 5.333 ms.
fn underruns_under_the_measured_pattern(mut jb: JitterBuffer, seconds: u64) -> u64 {
    let t0 = Instant::now();
    let end_us = seconds * 1_000_000;
    let burst_period_us = 6_000u64;
    let stall_every_us = 402_000u64;
    // (arrival_us, produced_up_to_us): the bursts inside a stall arrive at its end.
    let mut arrivals: Vec<(u64, u64)> = Vec::new();
    let mut t = burst_period_us;
    while t <= end_us {
        let offset = t % stall_every_us;
        let arrive = if offset > 0 && offset < MEASURED_MAX_GAP_US {
            t - offset + MEASURED_MAX_GAP_US
        } else {
            t
        };
        arrivals.push((arrive, t));
        t += burst_period_us;
    }
    arrivals.sort();
    let mut sent_frames = 0u64;
    let mut next_arrival = 0usize;
    let mut pop_n = 0u64;
    loop {
        // The hub's n-th pop at n * 256 / 48000 s.
        let pop_us = pop_n * BLOCK as u64 * 1_000_000 / RATE;
        if pop_us > end_us {
            break;
        }
        while next_arrival < arrivals.len() && arrivals[next_arrival].0 <= pop_us {
            let (arrive, produced_to) = arrivals[next_arrival];
            let produced = produced_to * RATE / 1_000_000;
            let frames = (produced - sent_frames) as usize;
            if frames > 0 {
                jb.push_at(
                    &burst(frames, sent_frames as i16),
                    t0 + Duration::from_micros(arrive),
                );
                sent_frames = produced;
            }
            next_arrival += 1;
        }
        jb.pop_block_at(BLOCK, t0 + Duration::from_micros(pop_us));
        pop_n += 1;
    }
    jb.underruns
}

#[test]
fn the_program_feed_target_covers_the_measured_gap_plus_one_block() {
    let target_us = (VBAN_PROGRAM_TARGET_BLOCKS * BLOCK) as u64 * 1_000_000 / RATE;
    let block_us = BLOCK as u64 * 1_000_000 / RATE;
    assert!(
        target_us >= MEASURED_MAX_GAP_US + block_us,
        "target {target_us} us must cover the 19.4 ms gap + one block"
    );
    assert_eq!(VBAN_PROGRAM_TARGET_BLOCKS, 6, "32 ms");
    assert_eq!(
        VBAN_PROGRAM_CAP_BLOCKS - VBAN_PROGRAM_TARGET_BLOCKS,
        VBAN_CAP_BLOCKS - VBAN_TARGET_BLOCKS,
        "the same headroom above the target as the cambox legs"
    );
    assert_eq!(VBAN_TARGET_BLOCKS, 3, "the camboxes keep 16 ms");
}

#[test]
fn the_measured_fohabl_pattern_drains_a_three_block_leg() {
    // The live symptom: the 16 ms target is not enough for this sender.
    let leg = JitterBuffer::vban_leg(VBAN_CAP_BLOCKS * BLOCK, VBAN_TARGET_BLOCKS * BLOCK);
    let n = underruns_under_the_measured_pattern(leg, 60);
    assert!(
        n > 0,
        "the 3-block leg should underrun under the measured gaps, got {n}"
    );
}

#[test]
fn the_program_feed_leg_rides_out_the_measured_fohabl_pattern_without_an_underrun() {
    let leg = JitterBuffer::vban_leg(
        VBAN_PROGRAM_CAP_BLOCKS * BLOCK,
        VBAN_PROGRAM_TARGET_BLOCKS * BLOCK,
    );
    let n = underruns_under_the_measured_pattern(leg, 600);
    assert_eq!(
        n, 0,
        "10 min of the measured pattern must play without one underrun"
    );
}

#[test]
fn the_deployed_matrix_gives_program_feeds_the_larger_target_and_camboxes_the_small_one() {
    let m = Matrix::from_toml(include_str!("../../intercom.strih-lx.toml")).unwrap();
    let buffers = input_buffers(&m);
    let mut program_feeds = 0;
    let mut camboxes = 0;
    for (p, b) in m.participants.iter().zip(&buffers) {
        if p.adapter != ADAPTER_VBAN {
            continue;
        }
        let target = b
            .network_stats()
            .expect("a VBAN leg has network stats")
            .target_frames;
        if p.role == CAMBOX_ROLE {
            camboxes += 1;
            assert_eq!(target, VBAN_TARGET_BLOCKS * BLOCK, "{}", p.name);
        } else {
            program_feeds += 1;
            assert_eq!(target, VBAN_PROGRAM_TARGET_BLOCKS * BLOCK, "{}", p.name);
        }
    }
    assert_eq!(camboxes, 7);
    assert_eq!(program_feeds, 3, "fohabl, lv1, mbc");
}
