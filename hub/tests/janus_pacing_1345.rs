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
    rx_gap, IntervalStats, PaceSchedule, PacedRing, PopKind, RxGap, FRAME_48K, MAX_CONCEAL_FRAMES,
    RING_CAP_FRAMES, RING_TARGET_FRAMES, TICK,
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

#[test]
fn every_pop_is_exactly_one_frame() {
    let mut r = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    r.push(&ramp(0, RING_CAP_FRAMES));
    for _ in 0..3 {
        let (f, k) = r.pop_frame();
        assert_eq!(k, PopKind::Audio);
        assert_eq!(f.len(), FRAME_48K);
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
    r.push(&ramp(1600, RING_TARGET_FRAMES));
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
