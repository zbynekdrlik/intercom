//! The Janus leg's own 20 ms clock (issue 1345, 25.9.2026: the phone voice sounds robotic).
//!
//! Measured on strih-lx: hub → Janus packets left on a 16/21 ms beat (sd 2.29 ms, min 15.3, max
//! 22.4 ms), because a packet went out whenever 960 frames had piled up from 256-frame (5.33 ms)
//! mix blocks. The Janus audiobridge mixes every 20 ms from a small buffer, so that beat made it
//! conceal. The fix: the block loop only FEEDS a ring, and a sender on its own monotonic clock pops
//! exactly one 20 ms frame per tick. These tests pin the pure pieces: the ring, the schedule, the
//! interval statistics the `/api/state` facet reports, and the receive-side loss plan.

use std::time::Duration;

use intercom_hub::janus_pacing::{
    hub_block_period, rx_gap, IntervalStats, PaceSchedule, PacedRing, PopKind, RxGap, FRAME_48K,
    MAX_CONCEAL_FRAMES, MAX_MISORDER, RING_CAP_FRAMES, RING_TARGET_FRAMES, TICK,
};

const BLOCK: usize = 256;

fn ramp(start: i16, n: usize) -> Vec<i16> {
    (0..n).map(|i| start.wrapping_add(i as i16)).collect()
}

// --- the ring --------------------------------------------------------------------------------

#[test]
fn constants_are_one_20ms_frame_at_48k() {
    assert_eq!(FRAME_48K, 960);
    assert_eq!(TICK, Duration::from_millis(20));
    // The target holds more than one frame (a late mix block never starves the next tick) and the
    // cap leaves room above the target before anything is trimmed.
    const { assert!(RING_TARGET_FRAMES > FRAME_48K) };
    const { assert!(RING_CAP_FRAMES >= RING_TARGET_FRAMES + FRAME_48K) };
}

#[test]
fn ring_primes_to_the_target_before_the_first_audio_frame() {
    let mut r = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    r.push(&ramp(1, RING_TARGET_FRAMES - 1));
    let (f, k) = r.pop_frame();
    assert_eq!(k, PopKind::Priming);
    assert_eq!(
        f,
        vec![0i16; FRAME_48K],
        "priming sends a whole silent frame"
    );
    assert_eq!(r.fill(), RING_TARGET_FRAMES - 1, "priming consumes nothing");
    r.push(&[7]);
    let (f, k) = r.pop_frame();
    assert_eq!(k, PopKind::Audio);
    assert_eq!(f, ramp(1, FRAME_48K), "exactly 960 frames, oldest first");
    assert_eq!(r.fill(), RING_TARGET_FRAMES - FRAME_48K);
    assert_eq!(r.underflows(), 0);
}

/// Review round 2: priming ends at the target, not above it. What piled up past the target while
/// silence went out is the oldest audio; dropping it is inaudible and keeps latency at the target.
#[test]
fn priming_ends_exactly_at_the_target() {
    let mut r = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    r.push(&ramp(0, RING_TARGET_FRAMES + 700));
    let (f, k) = r.pop_frame();
    assert_eq!(k, PopKind::Audio);
    assert_eq!(
        f,
        ramp(700, FRAME_48K),
        "the oldest 700 samples were dropped"
    );
    assert_eq!(r.fill(), RING_TARGET_FRAMES - FRAME_48K);
    assert_eq!(r.trims(), 0, "not an overflow trim");
}

#[test]
fn every_pop_is_exactly_one_frame() {
    let mut r = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    r.push(&ramp(0, RING_TARGET_FRAMES));
    for _ in 0..3 {
        let (f, k) = r.pop_frame();
        assert_eq!(k, PopKind::Audio);
        assert_eq!(f.len(), FRAME_48K);
        r.push(&ramp(0, FRAME_48K));
    }
}

#[test]
fn underflow_bridges_with_a_whole_silent_frame_and_reprimes() {
    let mut r = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    r.push(&ramp(1, RING_TARGET_FRAMES));
    assert_eq!(r.pop_frame().1, PopKind::Audio);
    assert_eq!(r.pop_frame().1, PopKind::Audio);
    // 0 frames left of the target (1920 - 2 x 960): a partial 500 must not be spliced out.
    r.push(&ramp(100, 500));
    let (f, k) = r.pop_frame();
    assert_eq!(k, PopKind::Underflow);
    assert_eq!(f, vec![0i16; FRAME_48K], "never a partial zero-splice");
    assert_eq!(r.underflows(), 1);
    assert_eq!(r.fill(), 500, "the partial audio is kept for later");
    // Re-priming: silent until the target is back, then audio resumes with the kept samples first.
    r.push(&ramp(600, 1000));
    assert_eq!(r.pop_frame().1, PopKind::Priming);
    // Refilled to exactly the target (priming trims only what lies ABOVE the target).
    r.push(&ramp(1600, RING_TARGET_FRAMES - 1500));
    let (f, k) = r.pop_frame();
    assert_eq!(k, PopKind::Audio);
    assert_eq!(f[..500], ramp(100, 500)[..]);
    assert_eq!(r.underflows(), 1, "priming is not counted as an underflow");
}

#[test]
fn overflow_trims_the_oldest_down_to_the_target() {
    let mut r = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    r.push(&ramp(0, RING_CAP_FRAMES));
    assert_eq!(r.trims(), 0, "exactly the cap is not an overflow");
    r.push(&[1]);
    assert_eq!(r.trims(), 1);
    assert_eq!(
        r.fill(),
        RING_TARGET_FRAMES,
        "trimmed to the target, not to the cap"
    );
    // The NEWEST samples are kept: the last one pushed is the last one in the ring.
    let mut last = 0;
    for _ in 0..(RING_TARGET_FRAMES / FRAME_48K) {
        last = *r.pop_frame().0.last().unwrap();
    }
    assert_eq!(last, 1);
}

#[test]
fn reset_empties_and_reprimes() {
    let mut r = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    r.push(&ramp(0, RING_CAP_FRAMES));
    r.reset();
    assert_eq!(r.fill(), 0);
    assert_eq!(r.pop_frame().1, PopKind::Priming);
}

/// The live case: the block loop feeds 256 frames every 5.333 ms (plus its measured jitter), the
/// sender pops 960 every 20 ms. Over 60 s of simulated time the ring never underflows after
/// priming and never trims, and its fill stays bounded — so every tick carries real audio.
#[test]
fn block_loop_feed_against_the_20ms_pop_never_underflows_or_trims() {
    let mut r = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    let block_us = 256.0 * 1_000_000.0 / 48_000.0; // 5333.33 us
    let tick_us = 20_000.0;
    let mut next_block = 0.0f64;
    let mut next_tick = tick_us;
    let mut blocks = 0u64;
    let mut audio = 0u64;
    let mut max_fill = 0usize;
    // A deterministic +-0.6 ms wobble on the block loop (its measured sd).
    let wobble = |n: u64| ((n * 7919 % 13) as f64 - 6.0) * 100.0;
    while next_tick < 60_000_000.0 {
        let block_at = next_block + wobble(blocks);
        if block_at <= next_tick {
            r.push(&vec![1i16; BLOCK]);
            blocks += 1;
            next_block += block_us;
            max_fill = max_fill.max(r.fill());
        } else {
            let (_, k) = r.pop_frame();
            if k == PopKind::Audio {
                audio += 1;
            }
            next_tick += tick_us;
        }
    }
    assert_eq!(r.underflows(), 0, "no silence bridge in steady state");
    assert_eq!(r.trims(), 0, "no trim in steady state");
    assert!(
        audio >= 2990,
        "almost every tick is audio after priming, got {audio}"
    );
    assert!(
        max_fill <= RING_TARGET_FRAMES + 2 * BLOCK,
        "fill bounded, max {max_fill}"
    );
}

// --- the block clock + the ring's fill servo (review round 1) --------------------------------

/// The block loop's period must be exact. `256 * 1_000_000 / 48_000` in whole µs is 5333 µs, not
/// 5333.33: the loop then ran 62.5 ppm fast, so every egress (the Janus ring, VBAN to the camboxes,
/// the PipeWire sinks) gained ~3 samples/s against a 48 kHz consumer.
#[test]
fn hub_block_period_is_exact_to_the_nanosecond() {
    assert_eq!(
        hub_block_period(256, 48_000),
        Duration::from_nanos(5_333_333)
    );
    assert_eq!(hub_block_period(480, 48_000), Duration::from_millis(10));
    // One hour of blocks is one hour to well under a millisecond (the µs version was 225 ms off).
    let blocks_per_hour = 3600 * 48_000 / 256;
    let hour = hub_block_period(256, 48_000) * blocks_per_hour;
    assert!(Duration::from_secs(3600) - hour < Duration::from_millis(1));
    // A degenerate rate never divides by zero.
    assert_eq!(hub_block_period(256, 0), Duration::ZERO);
}

/// What a long feed-vs-pop run did to the ring.
struct SimResult {
    ring: PacedRing,
    audio: u64,
    min_before_pop: usize,
    max_before_pop: usize,
}

/// Feed 256-frame blocks every `block_us` (skipping one block every `skip_every_s` seconds, as a
/// late tokio tick with `MissedTickBehavior::Skip` does) against the 20 ms pop, for `secs`.
fn simulate(block_us: f64, secs: f64, skip_every_s: Option<f64>) -> SimResult {
    simulate_from(0.0, block_us, secs, skip_every_s)
}

/// [`simulate`] with the first mix block arriving `start_us` after the clocks start (the phase of
/// the block feed against the 20 ms pop).
fn simulate_from(start_us: f64, block_us: f64, secs: f64, skip_every_s: Option<f64>) -> SimResult {
    let mut ring = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    let mut next_block = start_us;
    let mut next_tick = 20_000.0f64;
    let mut next_skip = skip_every_s.map(|s| s * 1e6);
    let mut audio = 0u64;
    let mut min_before_pop = usize::MAX;
    let mut max_before_pop = 0usize;
    let end = secs * 1e6;
    while next_tick < end {
        if next_block <= next_tick {
            let skip = matches!(next_skip, Some(at) if next_block >= at);
            if skip {
                next_skip = next_skip.map(|at| at + skip_every_s.unwrap_or(0.0) * 1e6);
            } else {
                ring.push(&[1i16; BLOCK]);
            }
            next_block += block_us;
        } else {
            let before = ring.fill();
            let (_, k) = ring.pop_frame();
            if k == PopKind::Audio {
                audio += 1;
                if next_tick > 5e6 {
                    min_before_pop = min_before_pop.min(before);
                    max_before_pop = max_before_pop.max(before);
                }
            }
            next_tick += 20_000.0;
        }
    }
    SimResult {
        ring,
        audio,
        min_before_pop,
        max_before_pop,
    }
}

#[test]
fn a_fast_feed_is_absorbed_by_1ms_drops_never_a_60ms_trim() {
    // The old truncated 5333 µs period: 62.5 ppm fast, 20 minutes.
    let r = simulate(5333.0, 1200.0, None);
    assert_eq!(r.ring.trims(), 0, "no overflow cliff");
    assert_eq!(r.ring.underflows(), 0);
    assert!(r.ring.servo_drops() > 0, "the servo dropped the excess");
    assert_eq!(r.ring.servo_repeats(), 0);
    assert!(
        r.max_before_pop <= RING_TARGET_FRAMES + FRAME_48K,
        "latency stays bounded, max fill {}",
        r.max_before_pop
    );
}

#[test]
fn a_slow_feed_is_absorbed_by_1ms_repeats_never_a_silent_frame() {
    // 62.5 ppm slow, 20 minutes.
    let r = simulate(5333.667, 1200.0, None);
    assert_eq!(r.ring.underflows(), 0, "no silence bridge");
    assert_eq!(r.ring.trims(), 0);
    assert!(r.ring.servo_repeats() > 0, "the servo filled the deficit");
    assert!(
        r.min_before_pop >= FRAME_48K,
        "min fill {}",
        r.min_before_pop
    );
}

#[test]
fn skipped_mix_blocks_are_recovered_without_an_underflow() {
    // The exact rate, but the block loop misses one tick every 30 s (256 samples lost each time).
    let r = simulate(1e6 * 256.0 / 48_000.0, 1200.0, Some(30.0));
    assert_eq!(r.ring.underflows(), 0, "no silence bridge");
    assert_eq!(r.ring.trims(), 0);
    assert!(r.ring.servo_repeats() > 0);
    assert!(
        r.audio >= 59_990,
        "every tick carries audio, got {}",
        r.audio
    );
}

#[test]
fn the_servo_leaves_a_steady_exact_feed_alone() {
    let r = simulate(1e6 * 256.0 / 48_000.0, 600.0, None);
    assert_eq!(r.ring.servo_drops(), 0);
    assert_eq!(r.ring.servo_repeats(), 0);
    assert_eq!(r.ring.underflows(), 0);
    assert_eq!(r.ring.trims(), 0);
}

/// Review round 2: priming used to hand out audio at whatever fill it first reached (up to ~2940
/// samples, since nothing is consumed while priming), so the servo then dropped ~8 ms right after
/// every (re)prime and latency started near 60 ms. Whatever the phase of the block feed, a steady
/// exact feed must never wake the servo.
#[test]
fn the_servo_leaves_a_steady_exact_feed_alone_at_every_start_phase() {
    let block_us = 1e6 * 256.0 / 48_000.0;
    for step in 0..16 {
        let start_us = step as f64 * 20_000.0 / 16.0;
        let r = simulate_from(start_us, block_us, 120.0, None);
        assert_eq!(r.ring.servo_drops(), 0, "phase {start_us} us");
        assert_eq!(r.ring.servo_repeats(), 0, "phase {start_us} us");
        assert_eq!(r.ring.underflows(), 0, "phase {start_us} us");
        assert!(
            r.max_before_pop <= RING_TARGET_FRAMES + BLOCK,
            "phase {start_us} us: primed too full, max pre-pop fill {}",
            r.max_before_pop
        );
    }
}

/// Run a continuous slow ramp (one step per 8 samples) through the ring with the pre-pop fill held
/// at `level` until the servo has acted, and return everything that was popped.
fn servo_run(level: usize) -> (PacedRing, Vec<i16>) {
    let mut r = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    let mut counter: i32 = 0;
    let mut feed = |r: &mut PacedRing, n: usize| {
        let chunk: Vec<i16> = (0..n)
            .map(|_| {
                counter += 1;
                (counter / 8) as i16
            })
            .collect();
        r.push(&chunk);
    };
    feed(&mut r, RING_TARGET_FRAMES);
    let mut out = Vec::new();
    for _ in 0..200 {
        if r.fill() < level {
            let n = level - r.fill();
            feed(&mut r, n);
        }
        let (f, k) = r.pop_frame();
        assert_eq!(k, PopKind::Audio);
        assert_eq!(f.len(), FRAME_48K, "a servo action keeps the frame whole");
        out.extend(f);
        if r.servo_repeats() + r.servo_drops() > 0 {
            break;
        }
    }
    (r, out)
}

fn max_step(x: &[i16]) -> i32 {
    x.windows(2)
        .map(|w| (i32::from(w[1]) - i32::from(w[0])).abs())
        .max()
        .unwrap_or(0)
}

/// Review round 2: a 1 ms repeat is crossfaded, never a hard splice (a click). On the slow ramp a
/// hard splice jumps by 6; the crossfaded output never steps by more than 2.
#[test]
fn a_servo_repeat_is_crossfaded_not_a_click() {
    let (r, out) = servo_run(RING_TARGET_FRAMES - FRAME_48K / 2);
    assert!(r.servo_repeats() > 0, "the servo repeated 1 ms");
    assert!(max_step(&out) <= 2, "max step {}", max_step(&out));
}

/// Review round 2: the same for a 1 ms drop.
#[test]
fn a_servo_drop_is_crossfaded_not_a_click() {
    let (r, out) = servo_run(RING_TARGET_FRAMES + FRAME_48K);
    assert!(r.servo_drops() > 0, "the servo dropped 1 ms");
    assert!(max_step(&out) <= 2, "max step {}", max_step(&out));
}

// --- the schedule ----------------------------------------------------------------------------

/// Deadlines are exact multiples of 20 ms from the start, however late each wake-up is — the
/// grid never drifts (no `+= elapsed` accumulation).
#[test]
fn schedule_deadlines_stay_on_the_exact_20ms_grid() {
    let mut s = PaceSchedule::new();
    let mut now = Duration::ZERO;
    let mut sends = Vec::new();
    for i in 0..1000u64 {
        let wait = s.wait(now);
        now += wait;
        sends.push(now);
        // Wake-up + encode + send cost 0..0.4 ms, varying.
        now += Duration::from_micros(i * 37 % 400);
        s.advance(now);
    }
    for (i, t) in sends.iter().enumerate() {
        let due = TICK * (i as u32 + 1);
        assert!(*t >= due, "never early: tick {i} at {t:?} < {due:?}");
        assert!(
            *t - due < Duration::from_micros(400),
            "tick {i} late {:?}",
            *t - due
        );
    }
    assert_eq!(s.resyncs(), 0);
}

#[test]
fn schedule_catches_up_a_short_stall_on_the_same_grid() {
    let mut s = PaceSchedule::new();
    assert_eq!(s.wait(Duration::ZERO), TICK);
    s.advance(TICK);
    // The thread stalls 30 ms past the 40 ms deadline: the missed tick is sent at once, then the
    // grid continues at 60 ms, 80 ms... (the RTP timestamps stay contiguous either way).
    let now = Duration::from_millis(70);
    assert_eq!(s.wait(now), Duration::ZERO);
    s.advance(now);
    assert_eq!(s.wait(now), Duration::ZERO, "60 ms is also due");
    s.advance(now);
    assert_eq!(
        s.wait(now),
        Duration::from_millis(10),
        "back on the grid at 80 ms"
    );
    assert_eq!(s.resyncs(), 0);
}

#[test]
fn schedule_resyncs_after_a_long_stall_instead_of_bursting() {
    let mut s = PaceSchedule::new();
    s.advance(TICK);
    // Stalled for a second: do not fire ~50 packets back to back.
    let now = Duration::from_millis(1040);
    assert_eq!(s.wait(now), Duration::ZERO);
    s.advance(now);
    assert_eq!(s.resyncs(), 1);
    assert_eq!(
        s.wait(now),
        TICK,
        "the next packet is one tick after the stall"
    );
}

// --- the interval statistics (the /api/state proof) -------------------------------------------

#[test]
fn interval_stats_report_sd_and_max_in_ms() {
    let mut st = IntervalStats::new(250);
    assert_eq!(st.sd_ms(), 0.0);
    assert_eq!(st.max_ms(), 0.0);
    st.record(Duration::from_millis(100));
    assert_eq!(st.max_ms(), 0.0, "one timestamp is no interval yet");
    // Intervals 20, 20, 20, 20 -> sd 0, max 20.
    for i in 1..=4 {
        st.record(Duration::from_millis(100 + 20 * i));
    }
    assert!(st.sd_ms().abs() < 1e-9);
    assert!((st.max_ms() - 20.0).abs() < 1e-9);
    // The old beat: 16 and 21 ms alternating -> sd 2.5, max 21.
    let mut beat = IntervalStats::new(250);
    let mut t = Duration::ZERO;
    beat.record(t);
    for i in 0..100 {
        t += Duration::from_millis(if i % 2 == 0 { 16 } else { 21 });
        beat.record(t);
    }
    assert!((beat.sd_ms() - 2.5).abs() < 1e-6, "sd {}", beat.sd_ms());
    assert!((beat.max_ms() - 21.0).abs() < 1e-9);
}

#[test]
fn interval_stats_window_and_reset() {
    let mut st = IntervalStats::new(3);
    let mut t = Duration::ZERO;
    st.record(t);
    t += Duration::from_millis(50);
    st.record(t);
    for _ in 0..3 {
        t += Duration::from_millis(20);
        st.record(t);
    }
    assert!(
        (st.max_ms() - 20.0).abs() < 1e-9,
        "the 50 ms interval left the window"
    );
    // A reset (a re-join) forgets the last timestamp: the join gap is not an interval.
    st.reset();
    t += Duration::from_secs(5);
    st.record(t);
    assert_eq!(st.max_ms(), 0.0);
}

// --- the receive-side loss plan (Opus FEC / PLC) --------------------------------------------

#[test]
fn rx_gap_classifies_sequence_numbers() {
    assert_eq!(rx_gap(None, 10), RxGap::First);
    assert_eq!(rx_gap(Some(10), 11), RxGap::InOrder);
    assert_eq!(rx_gap(Some(u16::MAX), 0), RxGap::InOrder, "wraps");
    assert_eq!(rx_gap(Some(10), 12), RxGap::Lost(1));
    assert_eq!(
        rx_gap(Some(10), 11 + MAX_CONCEAL_FRAMES),
        RxGap::Lost(MAX_CONCEAL_FRAMES)
    );
    assert_eq!(
        rx_gap(Some(10), 12 + MAX_CONCEAL_FRAMES),
        RxGap::Resync,
        "a long gap is a restart, not something to conceal"
    );
    assert_eq!(rx_gap(Some(10), 10), RxGap::Stale, "duplicate");
    assert_eq!(rx_gap(Some(10), 9), RxGap::Stale, "reordered late packet");
}

/// Review round 1: only a SMALL backward step is a late packet. A big backward jump (a new sender,
/// or one stray packet far ahead) is a fresh start, so the real stream is never locked out for
/// up to 32 768 packets (~11 minutes).
#[test]
fn rx_gap_treats_a_big_backward_jump_as_a_restart() {
    assert_eq!(MAX_MISORDER, 100);
    assert_eq!(rx_gap(Some(30_000), 0), RxGap::Resync);
    assert_eq!(
        rx_gap(Some(10), 10u16.wrapping_sub(MAX_MISORDER)),
        RxGap::Stale
    );
    assert_eq!(
        rx_gap(Some(10), 10u16.wrapping_sub(MAX_MISORDER + 1)),
        RxGap::Resync
    );
    assert_eq!(rx_gap(Some(10), 10u16.wrapping_add(0x8000)), RxGap::Resync);
}
