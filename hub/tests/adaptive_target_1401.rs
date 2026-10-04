//! Issue 1401, design 5980775411: the program feeds' target follows the sender.
//!
//! The FOH desk's stalls grew during one day: the largest gap between two `fohabl-strih` packets
//! was 19.4 ms in the morning and 27.6 ms in the afternoon (4.10.2026), and the fixed 32 ms
//! program-feed target underran again about 1.7 times a minute. Each program feed (every VBAN leg
//! that is not a cambox) now tracks the largest inter-arrival gap of the last 10 min; its target is
//! that gap plus one block plus half a burst, in whole blocks between 32 and 64 ms. A larger gap
//! raises it at once; 10 min without a gap that needs it lowers it by one block. The change goes
//! through the leg's setpoint, so the drift servo walks the fill there (no jump, no click). The
//! camboxes keep their fixed 16 ms.

use std::time::{Duration, Instant};

use intercom_hub::adaptive_target::{
    program_target_blocks, AdaptiveTarget, ADAPTIVE_BUCKET, ADAPTIVE_LOWER_AFTER, ADAPTIVE_WINDOW,
    PROGRAM_HALF_BURST_FRAMES, VBAN_PROGRAM_MAX_TARGET_BLOCKS,
};
use intercom_hub::inputs::input_buffers;
use intercom_hub::matrix::{Matrix, ADAPTER_VBAN, CAMBOX_ROLE};
use intercom_hub::vban_io::{DecodedAudio, JitterBuffer, TargetChange, STALE_STREAM_MS};
use intercom_hub::vban_jitter::{
    NetworkFill, SERVO_MIN_SPACING_FRAMES, SERVO_WINDOW_FRAMES, VBAN_PROGRAM_CAP_BLOCKS,
    VBAN_PROGRAM_TARGET_BLOCKS, VBAN_TARGET_BLOCKS,
};

const BLOCK: usize = 256;
const RATE: u32 = 48_000;
const FLOOR: usize = VBAN_PROGRAM_TARGET_BLOCKS * BLOCK;

fn us(us: u64) -> Duration {
    Duration::from_micros(us)
}

fn blocks(gap_us: u64) -> usize {
    program_target_blocks(us(gap_us), BLOCK, RATE)
}

// --- the target rule -----------------------------------------------------------------------------

#[test]
fn the_target_is_the_largest_gap_plus_a_block_plus_half_a_burst_in_whole_blocks() {
    assert_eq!(VBAN_PROGRAM_TARGET_BLOCKS, 6, "the floor: 32 ms");
    assert_eq!(VBAN_PROGRAM_MAX_TARGET_BLOCKS, 12, "the cap: 64 ms");
    assert_eq!(
        PROGRAM_HALF_BURST_FRAMES, 144,
        "half of the FOH sender's ~6 ms burst"
    );
    assert_eq!(ADAPTIVE_WINDOW, Duration::from_secs(600));
    assert_eq!(ADAPTIVE_LOWER_AFTER, Duration::from_secs(600));
    assert!(
        ADAPTIVE_BUCKET <= Duration::from_secs(10),
        "the window is exact to 10 s"
    );
    assert_eq!(blocks(0), 6, "an even sender stays at the floor");
    assert_eq!(
        blocks(19_400),
        6,
        "the morning's 19.4 ms: 27.7 ms, under the floor"
    );
    assert_eq!(
        blocks(27_600),
        7,
        "the afternoon's 27.6 ms: 35.9 ms -> 37.3 ms"
    );
    assert_eq!(blocks(35_000), 9, "35 ms: 43.3 ms -> 48 ms");
    assert_eq!(blocks(55_000), 12, "the cap");
    assert_eq!(
        blocks(500_000),
        12,
        "a sender fault never takes it past the cap"
    );
    // The exact edges: a gap needs one more block once gap + block + half a burst passes it.
    let edge_us =
        |b: usize| ((b * BLOCK - BLOCK - PROGRAM_HALF_BURST_FRAMES) as u64) * 1_000_000 / 48_000;
    assert_eq!(blocks(edge_us(7)), 7);
    assert_eq!(
        blocks(edge_us(7) + 1),
        8,
        "any gap past the edge needs the next block"
    );
}

#[test]
fn a_larger_gap_raises_the_target_at_once() {
    let t0 = Instant::now();
    let mut a = AdaptiveTarget::new(BLOCK, RATE);
    assert_eq!(a.target_frames(), FLOOR);
    assert_eq!(a.observe(us(6_000), t0), None);
    assert_eq!(a.observe(us(19_400), t0 + Duration::from_secs(1)), None);
    assert_eq!(
        a.observe(us(35_000), t0 + Duration::from_secs(2)),
        Some(9 * BLOCK)
    );
    assert_eq!(a.target_frames(), 9 * BLOCK);
    assert_eq!(a.max_gap_10min(), us(35_000));
    // Smaller gaps keep it, and the window keeps reporting the largest.
    assert_eq!(a.observe(us(6_000), t0 + Duration::from_secs(3)), None);
    assert_eq!(a.target_frames(), 9 * BLOCK);
    assert_eq!(a.max_gap_10min(), us(35_000));
    // A still larger one raises it again.
    assert_eq!(
        a.observe(us(45_000), t0 + Duration::from_secs(4)),
        Some(10 * BLOCK)
    );
}

/// Feed `a` one 6 ms gap per second from `from_s` to `to_s` and record every target change.
fn quiet(a: &mut AdaptiveTarget, t0: Instant, from_s: u64, to_s: u64) -> Vec<(u64, usize)> {
    (from_s..to_s)
        .filter_map(|s| {
            a.observe(us(6_000), t0 + Duration::from_secs(s))
                .map(|t| (s, t / BLOCK))
        })
        .collect()
}

#[test]
fn ten_minutes_without_a_gap_that_needs_it_lower_the_target_one_block_at_a_time() {
    let t0 = Instant::now();
    let mut a = AdaptiveTarget::new(BLOCK, RATE);
    assert_eq!(a.observe(us(35_000), t0), Some(9 * BLOCK));
    let changes = quiet(&mut a, t0, 1, 1900);
    assert_eq!(changes.len(), 3, "9 -> 8 -> 7 -> 6: {changes:?}");
    assert_eq!(
        changes.iter().map(|c| c.1).collect::<Vec<_>>(),
        vec![8, 7, 6]
    );
    // The first step once the 35 ms gap has left the 10 min window, then one every 10 min.
    assert!((590..=601).contains(&changes[0].0), "{changes:?}");
    assert_eq!(changes[1].0 - changes[0].0, 600, "{changes:?}");
    assert_eq!(changes[2].0 - changes[1].0, 600, "{changes:?}");
    assert!(
        changes[2].0 <= 1800,
        "back at the floor 30 min after the gap: {changes:?}"
    );
    assert_eq!(a.target_frames(), FLOOR);
    assert_eq!(a.max_gap_10min(), us(6_000));
    // Never below the floor.
    assert!(quiet(&mut a, t0, 1900, 4000).is_empty());
}

#[test]
fn a_large_gap_that_keeps_coming_back_holds_the_target() {
    let t0 = Instant::now();
    let mut a = AdaptiveTarget::new(BLOCK, RATE);
    for s in 0..3600 {
        let gap = if s % 300 == 0 { 35_000 } else { 6_000 };
        a.observe(us(gap), t0 + Duration::from_secs(s));
        assert_eq!(a.target_frames(), 9 * BLOCK, "at {s} s");
    }
}

#[test]
fn a_new_setpoint_moves_the_cap_with_it_and_drops_nothing() {
    let cap = VBAN_PROGRAM_CAP_BLOCKS * BLOCK;
    let headroom = cap - FLOOR;
    let mut f = NetworkFill::new(FLOOR, cap);
    f.set_target(9 * BLOCK);
    assert_eq!(f.target(), 9 * BLOCK);
    assert_eq!(
        f.overrun_keep(9 * BLOCK + headroom),
        None,
        "the cap moved up"
    );
    assert_eq!(f.overrun_keep(9 * BLOCK + headroom + 1), Some(9 * BLOCK));
    f.set_target(FLOOR);
    assert_eq!(f.overrun_keep(cap), None);
    assert_eq!(f.overrun_keep(cap + 1), Some(FLOOR));
    // A primed leg keeps every queued frame when its target moves: no jump, the servo walks it.
    let t0 = Instant::now();
    let mut jb = JitterBuffer::vban_leg(cap, FLOOR).with_adaptive_target("fohabl", BLOCK, RATE);
    jb.push_at(&mono(0, FLOOR), t0);
    let first = jb.pop_block_at(BLOCK, t0);
    assert_eq!(first[0], ramp(0, BLOCK), "primed at the floor");
    // A 35 ms gap: the late packet raises the target and carries the 35 ms of audio.
    let late = t0 + us(35_000);
    jb.push_at(&mono(FLOOR as i16, 1680), late);
    assert_eq!(jb.network_stats().unwrap().target_frames, 9 * BLOCK);
    assert_eq!(
        jb.buffered_frames(),
        FLOOR - BLOCK + 1680,
        "nothing dropped or padded"
    );
    let next = jb.pop_block_at(BLOCK, late);
    assert_eq!(
        next[0],
        ramp(BLOCK as i16, BLOCK),
        "the audio continues where it was"
    );
}

#[test]
fn a_gap_across_a_stall_never_reaches_the_adaptive_target() {
    // A FOH outage longer than the stale limit is a stall: the leg starts over, and the silence
    // before the stream came back is no inter-arrival gap. Counted as one, a 2 s outage would hold
    // the program feed at the 64 ms cap for about 10 min and above its floor for about 70 min.
    let t0 = Instant::now();
    let mut jb = JitterBuffer::vban_leg(VBAN_PROGRAM_CAP_BLOCKS * BLOCK, FLOOR)
        .with_adaptive_target("fohabl", BLOCK, RATE);
    jb.push_at(&mono(0, 288), t0);
    let back = t0 + Duration::from_millis(STALE_STREAM_MS + 100);
    jb.push_at(&mono(288, 288), back);
    let s = jb.network_stats().unwrap();
    assert_eq!(s.stalls, 1, "the outage is a stall");
    assert_eq!(
        s.target_frames, FLOOR,
        "and never a gap that raises the target"
    );
    assert_eq!(s.max_gap_us_10min, Some(0));
    assert_eq!(jb.take_target_change(), None);
    // The next packet inside the running stream is a gap again.
    jb.push_at(&mono(576, 288), back + us(6_000));
    assert_eq!(jb.network_stats().unwrap().max_gap_us_10min, Some(6_000));
}

#[test]
fn a_target_change_is_handed_out_once_for_the_caller_to_log_off_the_lock() {
    // The receive task holds the jitter lock while it pushes, and the real-time hub-mix thread
    // takes the same lock every block: the buffer only records the change, the caller logs it once
    // the lock is released.
    let t0 = Instant::now();
    let mut jb = JitterBuffer::vban_leg(VBAN_PROGRAM_CAP_BLOCKS * BLOCK, FLOOR)
        .with_adaptive_target("fohabl", BLOCK, RATE);
    jb.push_at(&mono(0, 288), t0);
    jb.push_at(&mono(288, 288), t0 + us(6_000));
    assert_eq!(jb.take_target_change(), None, "a 6 ms gap changes nothing");
    jb.push_at(&mono(576, 1680), t0 + us(41_000));
    assert_eq!(
        jb.take_target_change(),
        Some(TargetChange {
            leg: "fohabl".into(),
            from_frames: FLOOR,
            to_frames: 9 * BLOCK,
            max_gap_10min: us(35_000),
        })
    );
    assert_eq!(jb.take_target_change(), None, "handed out once");
    // A cambox leg never has one.
    let mut cam = JitterBuffer::vban_leg(8 * BLOCK, 3 * BLOCK);
    cam.push_at(&mono(0, 128), t0);
    cam.push_at(&mono(128, 128), t0 + us(35_000));
    assert_eq!(cam.take_target_change(), None);
}

#[test]
fn the_receive_task_logs_a_target_change_after_releasing_the_lock() {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&p).expect("read main.rs");
    let start = src
        .find("let change = jitter.lock()")
        .expect("the receive task takes the change under the lock");
    let end = start + src[start..].find("});").expect("the lock closure ends");
    let take = src
        .find("b.take_target_change()")
        .expect("the change is taken");
    let log = src.find("change.log()").expect("and logged");
    assert!(
        start < take && take < end,
        "taken while the buffer is locked"
    );
    assert!(end < log, "logged after the lock is released");
}

fn ramp(start: i16, n: usize) -> Vec<i16> {
    (0..n).map(|i| start.wrapping_add(i as i16)).collect()
}

fn mono(start: i16, frames: usize) -> DecodedAudio {
    DecodedAudio {
        stream_name: "fohabl-strih".into(),
        channels: vec![ramp(start, frames)],
        frames,
    }
}

// --- the replay ------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Replay {
    underruns: u64,
    /// When each underrun was counted (us).
    underrun_at: Vec<u64>,
    /// (us, new target in blocks) at every change.
    changes: Vec<(u64, usize)>,
    /// The fewest output frames between two servo corrections.
    min_correction_gap: usize,
    /// The most servo corrections in one second of output.
    max_corrections_per_s: u64,
    corrections: u64,
    max_target: usize,
    final_target: usize,
    silent_blocks: u64,
}

/// Plays the FOH sender into the real program-feed buffer for `secs`: 48 kHz audio handed out in
/// a burst every 6 ms; every 402 ms (67 bursts) the burst after one on time is held back so that
/// the next arrival comes `gap_at(t)` later, carrying everything produced meanwhile (nothing lost,
/// as the captures showed). The hub pops one 256-frame block every 5.333 ms.
fn replay(secs: u64, gap_at: impl Fn(u64) -> u64) -> Replay {
    let t0 = Instant::now();
    let mut jb = JitterBuffer::vban_leg(
        VBAN_PROGRAM_CAP_BLOCKS * BLOCK,
        VBAN_PROGRAM_TARGET_BLOCKS * BLOCK,
    )
    .with_adaptive_target("fohabl", BLOCK, RATE);
    let end_us = secs * 1_000_000;
    let burst_us = 6_000u64;
    let stall_every_us = 402_000u64;
    let mut out = Replay {
        min_correction_gap: usize::MAX,
        ..Replay::default()
    };
    let mut burst_t = burst_us;
    let mut sent_frames = 0u64;
    let mut pop_n = 0u64;
    let mut target = VBAN_PROGRAM_TARGET_BLOCKS;
    let mut corrections_seen = 0u64;
    let mut output_frames = 0usize;
    let mut last_correction: Option<usize> = None;
    let mut second_corrections = 0u64;
    let mut second = 0u64;
    loop {
        // The hub's n-th pop at n x 256 / 48000 s.
        let pop_us = pop_n * BLOCK as u64 * 1_000_000 / u64::from(RATE);
        if pop_us > end_us {
            break;
        }
        // Every burst that has arrived by the pop.
        loop {
            let offset = burst_t % stall_every_us;
            let gap = gap_at(burst_t - offset);
            let arrive = if offset > 0 && offset < gap {
                burst_t - offset + gap
            } else {
                burst_t
            };
            if arrive > pop_us {
                break;
            }
            let produced = burst_t * u64::from(RATE) / 1_000_000;
            let frames = (produced - sent_frames) as usize;
            let before = jb.underruns;
            jb.push_at(&mono(sent_frames as i16, frames), t0 + us(arrive));
            sent_frames = produced;
            if jb.underruns > before {
                out.underrun_at.push(arrive);
            }
            let now = jb.network_stats().unwrap().target_frames / BLOCK;
            if now != target {
                out.changes.push((arrive, now));
                target = now;
            }
            burst_t += burst_us;
        }
        let block = jb.pop_block_at(BLOCK, t0 + us(pop_us));
        if block[0].iter().all(|&s| s == 0) && pop_n > 200 {
            out.silent_blocks += 1;
        }
        output_frames += BLOCK;
        let s = jb.network_stats().unwrap();
        let total = s.servo_drops + s.servo_repeats;
        if total > corrections_seen {
            if let Some(prev) = last_correction {
                out.min_correction_gap = out.min_correction_gap.min(output_frames - prev);
            }
            last_correction = Some(output_frames);
            second_corrections += total - corrections_seen;
            corrections_seen = total;
        }
        if pop_us / 1_000_000 != second {
            out.max_corrections_per_s = out.max_corrections_per_s.max(second_corrections);
            second_corrections = 0;
            second = pop_us / 1_000_000;
        }
        pop_n += 1;
    }
    out.underruns = jb.underruns;
    out.corrections = corrections_seen;
    out.max_target = out
        .changes
        .iter()
        .map(|c| c.1)
        .max()
        .unwrap_or(VBAN_PROGRAM_TARGET_BLOCKS);
    out.final_target = target;
    out
}

/// The servo's own budget: at most one correction per 1000 output frames, 48 in a second; a
/// measured second can straddle two of the servo's.
fn assert_no_correction_burst(r: &Replay) {
    assert!(
        r.min_correction_gap >= SERVO_MIN_SPACING_FRAMES,
        "corrections closer than the servo's spacing: {r:?}"
    );
    assert!(
        r.max_corrections_per_s <= (SERVO_WINDOW_FRAMES / SERVO_MIN_SPACING_FRAMES) as u64 + 1,
        "more corrections in a second than the servo's budget: {r:?}"
    );
}

#[test]
fn a_gap_trace_growing_from_19_to_35ms_underruns_at_most_once_then_settles_back_to_the_floor() {
    // 19 ms for 2 min, then 2 ms more every 2 min up to 35 ms (at 16 min), held until 20 min, then
    // back to the morning's 19 ms for 31 min.
    let gap_at = |t_us: u64| {
        let s = t_us / 1_000_000;
        if s >= 1200 {
            19_000
        } else {
            (19_000 + 2_000 * (s / 120)).min(35_000)
        }
    };
    let r = replay(1200 + 1860, gap_at);
    let ctx = format!("{r:?}");
    assert!(r.underruns <= 1, "at most one underrun\n{ctx}");
    if let Some(&at) = r.underrun_at.first() {
        assert_eq!(
            Some(at),
            r.changes.first().map(|c| c.0),
            "an underrun only at the first new maximum, the gap that raises the target\n{ctx}"
        );
    }
    assert_eq!(r.max_target, 9, "35 ms needs 9 blocks\n{ctx}");
    assert_no_correction_burst(&r);
    // Back down: no step before the last 35 ms gap has left the window, the floor 30 min after it.
    let lowered: Vec<_> = r.changes.iter().filter(|c| c.0 > 1_200_000_000).collect();
    assert_eq!(lowered.len(), 3, "9 -> 8 -> 7 -> 6\n{ctx}");
    assert!(lowered[0].0 >= 1_200_000_000 + 589_000_000, "{ctx}");
    assert!(lowered[2].0 <= 1_200_000_000 + 1_801_000_000, "{ctx}");
    assert_eq!(r.final_target, VBAN_PROGRAM_TARGET_BLOCKS, "{ctx}");
    assert_eq!(
        r.silent_blocks, 0,
        "the target walks, nothing is padded\n{ctx}"
    );
}

#[test]
fn a_gap_that_jumps_from_19_to_35ms_underruns_once_and_never_again() {
    // The worst case: no warning, 35 ms gaps from the 60th second on, every 402 ms.
    let r = replay(300, |t_us| if t_us >= 60_000_000 { 35_000 } else { 19_000 });
    let ctx = format!("{r:?}");
    assert_eq!(
        r.underruns, 1,
        "the first 35 ms gap at the 32 ms floor runs dry\n{ctx}"
    );
    assert_eq!(
        r.underrun_at.first().copied(),
        r.changes.first().map(|c| c.0),
        "the underrun is the gap that raises the target\n{ctx}"
    );
    assert_eq!(r.max_target, 9, "{ctx}");
    assert_no_correction_burst(&r);
}

#[test]
fn the_deployed_matrix_gives_only_the_program_feeds_an_adaptive_target() {
    let m = Matrix::from_toml(include_str!("../../intercom.strih-lx.toml")).unwrap();
    let buffers = input_buffers(&m);
    for (p, b) in m.participants.iter().zip(&buffers) {
        if p.adapter != ADAPTER_VBAN {
            continue;
        }
        let s = b.network_stats().expect("a VBAN leg");
        if p.role == CAMBOX_ROLE {
            assert_eq!(s.target_frames, VBAN_TARGET_BLOCKS * BLOCK, "{}", p.name);
            assert_eq!(s.max_gap_us_10min, None, "{}: a fixed target", p.name);
        } else {
            assert_eq!(s.target_frames, FLOOR, "{}: starts at the floor", p.name);
            assert_eq!(s.max_gap_us_10min, Some(0), "{}: adaptive", p.name);
        }
    }
}
