//! Issue 1401 (owner, 4.10.2026: "zvuk na strih obs co ide strihacovi tak raz za cas kratke ma
//! vypadky"): the VBAN network legs had no jitter target. The old network policy popped whatever
//! was queued and zero-padded a short block, so its depth was wherever the first packets' phase
//! left it, often under one block, and a packet a little late against the hub's block clock wrote
//! a zero-padded block into the strih program mix (live: single underruns every 2-15 min on the
//! FOH feed over a clean network).
//!
//! These tests pin the VBAN-leg policy through the public API: the prefill (a cold start trims its
//! overshoot back to the target), one whole silent block per underrun and the re-prime, the overrun
//! trim to the target, the stale / never-received / muted-cambox rules (an underrun is counted only
//! when the stream continues, a restarted stream never replays its stale tail), the mono cambox
//! fan-out, the proportional drift servo's single-frame corrections and their <= 1 ms/s bound, and
//! the click-free spread of one frame across a block. The hours-long two-clock bench is
//! `vban_jitter_bench_1401.rs`.

use std::time::{Duration, Instant};

use intercom_hub::vban_io::{DecodedAudio, JitterBuffer, STALE_STREAM_MS};
use intercom_hub::vban_jitter::{
    stretch_block, NetworkFill, PopPlan, SERVO_DEADBAND_FRAMES, SERVO_GAIN_DIV,
    SERVO_MIN_SPACING_FRAMES, SERVO_WINDOW_FRAMES, VBAN_CAP_BLOCKS, VBAN_TARGET_BLOCKS,
};

const BLOCK: usize = 256;
const TARGET: usize = VBAN_TARGET_BLOCKS * BLOCK;
const CAP: usize = VBAN_CAP_BLOCKS * BLOCK;

fn ramp(start: i16, n: usize) -> Vec<i16> {
    (0..n).map(|i| start.wrapping_add(i as i16)).collect()
}

fn mono(samples: Vec<i16>) -> DecodedAudio {
    let frames = samples.len();
    DecodedAudio {
        stream_name: "fohabl-strih".into(),
        channels: vec![samples],
        frames,
    }
}

fn leg() -> JitterBuffer {
    JitterBuffer::vban_leg(CAP, TARGET)
}

fn is_silent(block: &[Vec<i16>]) -> bool {
    block
        .iter()
        .all(|ch| ch.len() == BLOCK && ch.iter().all(|&s| s == 0))
}

// --- the budget ----------------------------------------------------------------------------------

#[test]
fn the_target_is_three_hub_blocks_inside_the_cap() {
    assert_eq!(VBAN_TARGET_BLOCKS, 3, "the design's ~16 ms target");
    assert_eq!(TARGET, 768);
    assert_eq!(TARGET * 1000 / 48_000, 16, "16 ms at the 48 kHz hub rate");
    assert_eq!(VBAN_CAP_BLOCKS, 8, "the cap stays 8 blocks (43 ms)");
    assert_eq!(SERVO_WINDOW_FRAMES, 48_000, "the servo averages over 1 s");
    // At most one corrected frame per 1000 output frames = 1 ms/s at any rate.
    const { assert!(SERVO_MIN_SPACING_FRAMES >= 1_000) };
    // A late burst has five blocks of headroom before anything is dropped.
    const { assert!(CAP >= TARGET + 5 * BLOCK) };
    // Proportional: one corrected frame per second for every SERVO_GAIN_DIV frames of mean error
    // beyond the band, never the whole error at once, so the servo cannot overshoot; the band is a
    // small part of a block, so the leg settles close to its target.
    const { assert!(SERVO_GAIN_DIV >= 2) };
    const { assert!(SERVO_DEADBAND_FRAMES <= BLOCK / 8) };
}

// --- prefill, underrun, re-prime, overrun -------------------------------------------------------

#[test]
fn a_vban_leg_prefills_to_the_target_before_the_first_pop() {
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(1, TARGET - 1)), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert!(is_silent(&b), "a whole silent block while priming");
    assert_eq!(jb.buffered_frames(), TARGET - 1, "priming consumes nothing");
    assert_eq!(jb.underruns, 0, "priming is not an underrun");

    jb.push_at(&mono(vec![7]), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b,
        vec![ramp(1, BLOCK)],
        "audio starts with the first sample received, nothing trimmed"
    );
    assert_eq!(jb.buffered_frames(), TARGET - BLOCK);
    assert_eq!(jb.underruns, 0);
    let s = jb.network_stats().expect("a VBAN leg reports its fill");
    assert!(s.primed);
    assert_eq!(s.target_frames, TARGET);
}

#[test]
fn a_cold_start_trims_its_prime_overshoot_back_to_the_target() {
    // The first packets arrive in a burst 300 frames past the target while silence goes out: the
    // OLDEST 300 are dropped (inaudible, the leg is just starting), so the leg starts exactly at its
    // target instead of the servo walking 300 frames off one at a time.
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(0, TARGET + 300)), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b,
        vec![ramp(300, BLOCK)],
        "starts at the newest TARGET frames"
    );
    assert_eq!(jb.buffered_frames(), TARGET - BLOCK);
    assert_eq!(jb.network_stats().unwrap().servo_drops, 0);
}

#[test]
fn an_underrun_is_one_whole_silent_block_and_the_late_burst_resumes_seamlessly() {
    let t0 = Instant::now();
    let mut jb = leg();
    // 868 samples, 0..=867: three blocks play, 100 are left.
    jb.push_at(&mono(ramp(0, TARGET + 100)), t0);
    for i in 0..3 {
        let b = jb.pop_block_at(BLOCK, t0);
        assert_eq!(b, vec![ramp((i * BLOCK) as i16, BLOCK)]);
    }
    assert_eq!(jb.buffered_frames(), 100);

    // The next packet is late: ONE whole silent block, never 100 samples + 156 zeros.
    let b = jb.pop_block_at(BLOCK, t0);
    assert!(is_silent(&b), "an underrun is a whole silent block");
    assert_eq!(jb.buffered_frames(), 100, "the partial tail is kept");
    assert_eq!(
        jb.underruns, 0,
        "counted only once the stream continues (a mute is not a dropout)"
    );

    // The late packets arrive in one burst: the underrun counts, the very next pop resumes,
    // continuous, nothing lost (a re-prime after an underrun never trims).
    jb.push_at(&mono(ramp((TARGET + 100) as i16, 700)), t0);
    assert_eq!(jb.underruns, 1);
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b,
        vec![ramp(TARGET as i16, BLOCK)],
        "the kept tail plays first, then the burst"
    );
    assert_eq!(jb.underruns, 1, "still one underrun");
}

#[test]
fn after_an_underrun_it_reprimes_to_the_target_counting_one_underrun() {
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(0, TARGET + 100)), t0);
    for _ in 0..3 {
        jb.pop_block_at(BLOCK, t0);
    }
    // The stream ran dry, then refills at the normal rate of one block per pop.
    let mut silent = 0;
    let mut next = (TARGET + 100) as i16;
    let first_audio = loop {
        let b = jb.pop_block_at(BLOCK, t0);
        if !is_silent(&b) {
            break b;
        }
        silent += 1;
        jb.push_at(&mono(ramp(next, BLOCK)), t0);
        next = next.wrapping_add(BLOCK as i16);
        assert!(silent < 10, "re-priming must end");
    };
    // The underrun block leaves 100 + 256 = 356, one re-priming block 612 (< 768), the next
    // 868 >= 768: audio again after three silent blocks.
    assert_eq!(silent, 3, "one underrun block + two re-priming blocks");
    assert_eq!(jb.underruns, 1, "the re-prime counts no further underruns");
    assert_eq!(
        first_audio,
        vec![ramp(TARGET as i16, BLOCK)],
        "no audio lost"
    );
}

#[test]
fn an_overrun_drops_the_oldest_down_to_the_target() {
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(0, CAP)), t0);
    assert_eq!(jb.overruns, 0, "exactly the cap is not an overrun");
    jb.push_at(&mono(vec![CAP as i16]), t0);
    assert_eq!(jb.overruns, 1);
    assert_eq!(
        jb.buffered_frames(),
        TARGET,
        "back to the target, not the cap"
    );
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b,
        vec![ramp((CAP + 1 - TARGET) as i16, BLOCK)],
        "the newest audio kept"
    );
}

#[test]
fn a_never_received_vban_leg_pops_silence_without_an_underrun() {
    let mut jb = leg();
    for _ in 0..5 {
        let b = jb.pop_block(BLOCK);
        assert_eq!(b, vec![vec![0i16; BLOCK]]);
    }
    assert_eq!(jb.underruns, 0);
    assert!(!jb.network_stats().unwrap().primed);
}

#[test]
fn a_stale_vban_leg_counts_no_underrun_and_a_live_one_counts_one() {
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(0, TARGET)), t0);
    for _ in 0..3 {
        jb.pop_block_at(BLOCK, t0);
    }
    // The stream stopped: by the time it runs dry it is stale.
    let stale = t0 + Duration::from_millis(STALE_STREAM_MS + 1);
    jb.pop_block_at(BLOCK, stale);
    jb.pop_block_at(BLOCK, stale);
    assert_eq!(jb.underruns, 0, "a stale stream counts no underrun");

    // It comes back, re-primes, plays, then runs dry while live and continues 10 ms later: one
    // underrun.
    let back = stale + Duration::from_secs(1);
    jb.push_at(&mono(ramp(0, TARGET)), back);
    for _ in 0..3 {
        assert!(!is_silent(&jb.pop_block_at(BLOCK, back)));
    }
    assert!(is_silent(&jb.pop_block_at(BLOCK, back)));
    jb.push_at(&mono(ramp(0, BLOCK)), back + Duration::from_millis(10));
    assert_eq!(jb.underruns, 1);
}

#[test]
fn a_muted_cambox_counts_no_underrun_and_never_replays_its_stale_tail() {
    // A cambox sends only while unmuted. It runs dry right after the mute, while its last packet
    // is still fresh, and the next packet comes long after: no underrun (the status line would
    // otherwise name the cambox and hide a real dropout), and the 100 frames left from before the
    // mute are dropped, never played in front of the fresh audio.
    let t0 = Instant::now();
    let mut jb = leg().with_min_channels(2);
    jb.push_at(&mono(ramp(0, TARGET + 100)), t0);
    for _ in 0..3 {
        jb.pop_block_at(BLOCK, t0);
    }
    assert!(is_silent(&jb.pop_block_at(BLOCK, t0)), "ran dry");
    let unmute = t0 + Duration::from_secs(2);
    jb.push_at(&mono(ramp(10_000, TARGET)), unmute);
    assert_eq!(jb.underruns, 0, "a mute is not a dropout");
    assert_eq!(jb.buffered_frames(), TARGET, "the stale tail is gone");
    let b = jb.pop_block_at(BLOCK, unmute);
    assert_eq!(b, vec![ramp(10_000, BLOCK), ramp(10_000, BLOCK)]);
}

#[test]
fn a_mono_cambox_vban_leg_fans_into_ch2() {
    let t0 = Instant::now();
    let mut jb = leg().with_min_channels(2);
    jb.push_at(&mono(ramp(5, TARGET)), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(b, vec![ramp(5, BLOCK), ramp(5, BLOCK)]);
}

#[test]
fn the_other_policies_report_no_vban_fill() {
    assert!(JitterBuffer::new(CAP).network_stats().is_none());
    assert!(JitterBuffer::local_capture(CAP, TARGET)
        .network_stats()
        .is_none());
}

// --- the drift servo ----------------------------------------------------------------------------

/// Pop indices at which a counter advanced.
fn advances(prev: &mut u64, now: u64, pop: usize, at: &mut Vec<usize>) {
    if now > *prev {
        assert_eq!(now, *prev + 1, "one correction per pop at most");
        at.push(pop);
    }
    *prev = now;
}

#[test]
fn a_high_fill_is_walked_down_by_single_frame_drops_at_most_1ms_per_s() {
    let t0 = Instant::now();
    let mut jb = leg();
    // Primed at the target, then a mid-stream burst leaves 400 frames above it (a re-prime never
    // trims), then a steady exact feed of one block per pop.
    jb.push_at(&mono(vec![100; TARGET]), t0);
    jb.pop_block_at(BLOCK, t0);
    jb.push_at(&mono(vec![100; BLOCK + 400]), t0);
    let mut drops = 0u64;
    let mut at = Vec::new();
    for pop in 0..(40 * 48_000 / BLOCK) {
        let b = jb.pop_block_at(BLOCK, t0);
        assert_eq!(b[0].len(), BLOCK, "every pop is exactly one block");
        assert!(!is_silent(&b), "the servo never starves the leg");
        let s = jb.network_stats().unwrap();
        advances(&mut drops, s.servo_drops, pop, &mut at);
        assert_eq!(s.servo_repeats, 0);
        jb.push_at(&mono(vec![100; BLOCK]), t0);
    }
    assert!(drops > 0, "a high fill must be corrected");
    let min_gap_pops = SERVO_MIN_SPACING_FRAMES.div_ceil(BLOCK);
    assert!(
        at.windows(2).all(|w| w[1] - w[0] >= min_gap_pops),
        "at most one dropped frame per {SERVO_MIN_SPACING_FRAMES} output frames (<= 1 ms/s)"
    );
    let s = jb.network_stats().unwrap();
    assert!(
        s.depth_frames <= TARGET + SERVO_DEADBAND_FRAMES,
        "the mean fill is back in the band: {s:?}"
    );
    assert!(s.depth_frames + SERVO_DEADBAND_FRAMES >= TARGET, "{s:?}");
    assert!(
        s.servo_drops <= 400,
        "never more than the excess: no overshoot {s:?}"
    );
    assert_eq!(jb.underruns, 0);
    assert_eq!(jb.overruns, 0);
}

/// The proportional servo's equilibrium distance from the target for a sender `ppm` off the hub:
/// the band plus `SERVO_GAIN_DIV` frames per corrected frame per second, plus slack.
fn equilibrium_offset(ppm: f64) -> usize {
    let per_s = (ppm.abs() * 1e-6 * 48_000.0).ceil() as usize;
    SERVO_DEADBAND_FRAMES + SERVO_GAIN_DIV * per_s + 16
}

/// A count-level feed at `ppm` against the exact pop clock: `(ran_dry, drops, repeats, final)`.
fn ppm_feed(ppm: f64, secs: usize) -> (u64, u64, u64, NetworkFill) {
    let mut f = NetworkFill::new(TARGET, CAP);
    let per_pop = BLOCK as f64 * (1.0 + ppm * 1e-6);
    let (mut fill, mut owed, mut ran_dry) = (TARGET, 0.0f64, 0u64);
    for _ in 0..(secs * 48_000 / BLOCK) {
        match f.plan_pop(fill, BLOCK) {
            PopPlan::Silent { ran_dry: d } => ran_dry += u64::from(d),
            PopPlan::Audio { skip, take } => fill -= skip + take,
        }
        owed += per_pop;
        let arrive = owed.floor();
        owed -= arrive;
        fill += arrive as usize;
        if let Some(keep) = f.overrun_keep(fill) {
            fill = keep;
        }
    }
    let s = f.stats();
    (ran_dry, s.servo_drops, s.servo_repeats, f)
}

#[test]
fn a_slow_sender_is_absorbed_by_single_frame_repeats_never_an_underrun() {
    // -500 ppm for 10 minutes: 0.128 frames short per pop, 14 400 frames over the run.
    let (ran_dry, drops, repeats, f) = ppm_feed(-500.0, 600);
    assert_eq!(
        ran_dry, 0,
        "a slow sender inside the servo's range never runs dry"
    );
    assert_eq!(drops, 0);
    let expected = 500e-6 * 48_000.0 * 600.0;
    let tolerance = TARGET as f64;
    assert!(
        (repeats as f64 - expected).abs() < tolerance,
        "repeats {repeats} ≈ the drift {expected}"
    );
    let s = f.stats();
    assert!(
        s.depth_frames + equilibrium_offset(-500.0) >= TARGET,
        "{s:?}"
    );
}

#[test]
fn a_fast_sender_is_absorbed_by_single_frame_drops_never_an_overrun() {
    // +540 ppm: the cam1 headset ADC measured against the hub (issue 1345, 48 026 samples/s).
    let (ran_dry, drops, repeats, f) = ppm_feed(540.0, 600);
    assert_eq!(ran_dry, 0);
    assert_eq!(repeats, 0);
    let expected = 540e-6 * 48_000.0 * 600.0;
    let tolerance = TARGET as f64;
    assert!(
        (drops as f64 - expected).abs() < tolerance,
        "drops {drops} ≈ the drift {expected}"
    );
    let s = f.stats();
    assert!(
        s.depth_frames <= TARGET + equilibrium_offset(540.0),
        "{s:?}"
    );
}

/// A steady exact feed of `pattern` frames per packet (repeating), one packet every
/// `pattern[i] / 48 kHz`, starting `phase` frames into the first block.
fn steady_feed_corrections(pattern: &[usize], phase: usize, secs: usize, settle_s: usize) -> u64 {
    let mut f = NetworkFill::new(TARGET, CAP);
    let mut fill = 0usize;
    // Time in frames at 48 kHz: packets complete at cumulative frame counts, pops every BLOCK.
    let mut next_pkt_at = phase + pattern[0];
    let mut k = 0usize;
    let mut late_corrections = 0u64;
    let pops = secs * 48_000 / BLOCK;
    for pop in 1..=pops {
        let t = pop * BLOCK;
        while next_pkt_at <= t {
            fill += pattern[k % pattern.len()];
            k += 1;
            next_pkt_at += pattern[k % pattern.len()];
            if let Some(keep) = f.overrun_keep(fill) {
                fill = keep;
            }
        }
        let before = f.stats();
        match f.plan_pop(fill, BLOCK) {
            PopPlan::Silent { ran_dry } => assert!(!ran_dry, "a steady feed never runs dry"),
            PopPlan::Audio { skip, take } => fill -= skip + take,
        }
        let after = f.stats();
        let corrected =
            (after.servo_drops + after.servo_repeats) - (before.servo_drops + before.servo_repeats);
        if t >= settle_s * 48_000 {
            late_corrections += corrected;
        }
    }
    late_corrections
}

#[test]
fn the_servo_leaves_a_steady_exact_feed_alone_once_settled() {
    // The FOH feed (103 frames at 96 kHz = 51/52 at 48 kHz), a cambox (128) and a 256-frame
    // sender, at many start phases: after the first 30 s the servo makes no correction at all.
    for pattern in [&[51usize, 52][..], &[128], &[256]] {
        for phase in (0..BLOCK).step_by(16) {
            assert_eq!(
                steady_feed_corrections(pattern, phase, 120, 30),
                0,
                "packets {pattern:?}, phase {phase}"
            );
        }
    }
}

// --- the click-free single-frame splice ---------------------------------------------------------

#[test]
fn stretch_block_keeps_both_ends_and_spreads_one_frame() {
    assert_eq!(stretch_block(&ramp(0, BLOCK), BLOCK), ramp(0, BLOCK));
    // A drop: 257 frames into 256. Both ends kept, exactly one step of 2, every other step 1.
    let out = stretch_block(&ramp(0, BLOCK + 1), BLOCK);
    assert_eq!(out.len(), BLOCK);
    assert_eq!((out[0], out[BLOCK - 1]), (0, BLOCK as i16));
    let steps: Vec<i16> = out.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(steps.iter().filter(|&&d| d == 2).count(), 1);
    assert!(steps.iter().all(|&d| d == 1 || d == 2));
    // A repeat: 255 frames into 256. Exactly one step of 0.
    let out = stretch_block(&ramp(0, BLOCK - 1), BLOCK);
    assert_eq!((out[0], out[BLOCK - 1]), (0, (BLOCK - 2) as i16));
    let steps: Vec<i16> = out.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(steps.iter().filter(|&&d| d == 0).count(), 1);
    assert!(steps.iter().all(|&d| d == 0 || d == 1));
    // Degenerate inputs never panic.
    assert_eq!(stretch_block(&[], 4), vec![0; 4]);
    assert_eq!(stretch_block(&[9], 3), vec![9; 3]);
    assert!(stretch_block(&[1, 2], 0).is_empty());
}

fn max_step(x: &[i16]) -> i32 {
    x.windows(2)
        .map(|w| (i32::from(w[1]) - i32::from(w[0])).abs())
        .max()
        .unwrap_or(0)
}

#[test]
fn a_servo_correction_does_not_click_on_a_loud_tone() {
    // A 10 kHz tone near full scale: the hardest case for a one-sample splice. The spread
    // correction raises the largest sample-to-sample step by well under 1 %, where a plain
    // one-sample drop at the worst point would add a jump of the tone's whole per-sample swing.
    let tone: Vec<i16> = (0..BLOCK + 1)
        .map(|i| {
            (30_000.0 * (2.0 * std::f64::consts::PI * 10_000.0 * i as f64 / 48_000.0).sin()) as i16
        })
        .collect();
    let base = max_step(&tone);
    let bound = f64::from(base) * 1.01 + 1.0;
    for (n_in, label) in [(BLOCK + 1, "drop"), (BLOCK - 1, "repeat")] {
        let out = stretch_block(&tone[..n_in], BLOCK);
        let step = max_step(&out);
        assert!(
            f64::from(step) <= bound,
            "{label}: max step {step} vs the tone's own {base}"
        );
    }
    // The bound discriminates: cutting one sample out (the naive drop) breaks it at the worst point.
    let naive_worst = (1..BLOCK)
        .map(|r| {
            let mut naive = tone.clone();
            naive.remove(r);
            max_step(&naive)
        })
        .max()
        .unwrap_or(0);
    assert!(
        f64::from(naive_worst) > bound,
        "a naive one-sample drop would click: {naive_worst} vs {bound}"
    );
}
