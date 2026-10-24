//! Issue 1461 (lane 1, design comment 6376769274): the resampler at the hub's clock-domain
//! crossings ([`intercom_hub::asrc`]) and the fixed-target leg it drives
//! ([`intercom_hub::vban_jitter::NetworkFill`]).
//!
//! Part 1 pins the resampler itself: phase 0 at ratio 1 is the identity, every phase has DC gain
//! 1, a sine read at any ratio inside +-500 ppm lands on the continuous sine, and rendering in
//! blocks equals rendering in one pass (the fractional read phase carries across blocks).
//!
//! Part 2 is the two-clock bench on frame counts: a sender `ppm` off the hub's clock delivers
//! packets (optionally jittered, or in the FOH burst pattern), the hub pops 256-frame blocks on
//! its exact grid, for hours. After the settle no pop may be silent, no frame may be skipped or
//! trimmed, the depth stays inside +-1/4 of the target and the target never moves. The audio
//! itself (THD+N of a sine through the real buffer) is graded in `tests/asrc_leg_1461.rs`.

use std::collections::VecDeque;

use intercom_hub::asrc::{
    least_squares_slope, render, render_interleaved, ReadPoint, ASRC_HISTORY, ASRC_LOOKAHEAD,
    ASRC_MAX_PPM, ASRC_PHASES,
};
use intercom_hub::vban_jitter::{
    target_frames_from_ms, vban_cap_blocks, NetworkFill, PopPlan, CARD_CAPTURE_TARGET_FRAMES,
    VBAN_PROGRAM_TARGET_BLOCKS, VBAN_TARGET_BLOCKS,
};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;

fn sine(t: f64, hz: f64, amp: f64) -> f64 {
    amp * (2.0 * std::f64::consts::PI * hz * t / RATE).sin()
}

/// A deterministic test signal (a fixed LCG, never zero-mean structure the filter could hide).
fn noise(n: usize) -> Vec<i16> {
    let mut x = 0x2545_f491_u32;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 16) as i16
        })
        .collect()
}

// ---- Part 1: the resampler ----------------------------------------------------------------

#[test]
fn unity_ratio_at_phase_zero_is_a_delayed_identity() {
    let input = noise(4096);
    let read = ReadPoint::at_prime(1.0);
    let n = 2048;
    assert!(read.frames_needed(n) <= input.len());
    let mut out = vec![0i16; n];
    render(read, &input, &mut out);
    assert_eq!(&out[..], &input[ASRC_HISTORY..ASRC_HISTORY + n]);
}

#[test]
fn every_phase_has_dc_gain_one() {
    let input = vec![12_345i16; 256];
    for p in 0..=ASRC_PHASES * 4 {
        let frac = p as f64 / (ASRC_PHASES * 4) as f64;
        for ratio in [1.0 - ASRC_MAX_PPM * 1e-6, 1.0, 1.0 + ASRC_MAX_PPM * 1e-6] {
            let read = ReadPoint {
                pos: ASRC_HISTORY as f64 + frac.min(0.999_999),
                ratio,
            };
            let mut out = vec![0i16; 64];
            render(read, &input, &mut out);
            assert!(
                out.iter().all(|&v| v == 12_345),
                "frac {frac} ratio {ratio}"
            );
        }
    }
}

/// The RMS error of `out` against the continuous sine read at `read`'s positions, in dB of the
/// sine's own RMS.
fn error_db(read: ReadPoint, out: &[i16], hz: f64, amp: f64) -> f64 {
    let (mut e2, mut s2) = (0.0, 0.0);
    for (k, &v) in out.iter().enumerate() {
        let ideal = sine(read.pos + k as f64 * read.ratio, hz, amp);
        e2 += (f64::from(v) - ideal).powi(2);
        s2 += ideal * ideal;
    }
    10.0 * (e2 / s2).log10()
}

#[test]
fn a_sine_read_at_any_ratio_lands_on_the_continuous_sine() {
    let amp = 16_384.0;
    for hz in [100.0, 1_000.0, 5_000.0] {
        let input: Vec<i16> = (0..12_000)
            .map(|n| sine(n as f64, hz, amp).round() as i16)
            .collect();
        for ppm in [-500.0, -137.0, -50.0, 0.0, 50.0, 137.0, 500.0] {
            for start in [0.0, 0.25, 0.37, 0.999] {
                let read = ReadPoint {
                    pos: ASRC_HISTORY as f64 + start,
                    ratio: 1.0 + ppm * 1e-6,
                };
                let mut out = vec![0i16; 9_600];
                render(read, &input, &mut out);
                let db = error_db(read, &out, hz, amp);
                assert!(
                    db < -85.0,
                    "{hz} Hz at {ppm:+} ppm from +{start}: {db:.1} dB"
                );
            }
        }
    }
}

#[test]
fn rendering_block_by_block_equals_one_pass() {
    let input = noise(40_000);
    let read0 = ReadPoint {
        pos: ASRC_HISTORY as f64 + 0.41,
        ratio: 1.0 + 321e-6,
    };
    let blocks = 100;
    let mut whole = vec![0i16; blocks * BLOCK];
    render(read0, &input, &mut whole);
    // Block by block, dropping what each block consumed, exactly as a leg's queue does.
    let mut queue: VecDeque<i16> = input.iter().copied().collect();
    let mut read = read0;
    let mut pieces = Vec::new();
    for _ in 0..blocks {
        assert!(read.frames_needed(BLOCK) <= queue.len());
        let mut block = vec![0i16; BLOCK];
        render(read, queue.make_contiguous(), &mut block);
        pieces.extend_from_slice(&block);
        let (next, drop) = read.advanced(BLOCK);
        queue.drain(0..drop);
        read = next;
        assert!(
            (ASRC_HISTORY as f64..ASRC_HISTORY as f64 + 1.0).contains(&read.pos),
            "the read point keeps exactly its history: {}",
            read.pos
        );
    }
    assert_eq!(pieces, whole);
}

#[test]
fn interleaved_rendering_equals_per_channel_rendering() {
    let (a, b) = (
        noise(8_000),
        noise(8_000).iter().map(|&v| v / 3).collect::<Vec<_>>(),
    );
    let inter: Vec<i16> = a.iter().zip(&b).flat_map(|(&x, &y)| [x, y]).collect();
    let read = ReadPoint {
        pos: ASRC_HISTORY as f64 + 0.73,
        ratio: 1.0 - 444e-6,
    };
    let n = 4_096;
    let (mut oa, mut ob, mut oi) = (vec![0i16; n], vec![0i16; n], vec![0i16; 2 * n]);
    render(read, &a, &mut oa);
    render(read, &b, &mut ob);
    render_interleaved(read, &inter, 2, &mut oi);
    for k in 0..n {
        assert_eq!((oi[2 * k], oi[2 * k + 1]), (oa[k], ob[k]), "frame {k}");
    }
}

#[test]
fn frames_needed_covers_the_look_ahead_of_the_last_frame() {
    let read = ReadPoint {
        pos: ASRC_HISTORY as f64 + 0.5,
        ratio: 1.0 + 500e-6,
    };
    let last = read.pos + 255.0 * read.ratio;
    assert_eq!(
        read.frames_needed(BLOCK),
        last.floor() as usize + ASRC_LOOKAHEAD + 1
    );
    assert_eq!(read.frames_needed(0), 0);
    let input = noise(read.frames_needed(BLOCK));
    let mut out = vec![0i16; BLOCK];
    render(read, &input, &mut out);
    assert!(
        out.iter().any(|&v| v != 0),
        "every frame of the block rendered"
    );
}

#[test]
fn the_rate_fit_is_the_least_squares_slope() {
    let pts: VecDeque<(f64, f64)> = (0..64)
        .map(|i| {
            let t = i as f64 * 48_000.0;
            (
                t,
                7.0 + t * (1.0 + 123e-6) + if i % 2 == 0 { 3.0 } else { -3.0 },
            )
        })
        .collect();
    let slope = least_squares_slope(&pts).unwrap();
    // The alternating +-3 frames bias the slope by a deterministic 0.09 ppm.
    assert!(((slope - 1.0) * 1e6 - 123.0).abs() < 0.2, "{slope}");
    assert!(least_squares_slope(&VecDeque::from(vec![(1.0, 2.0)])).is_none());
}

// ---- The fixed target -----------------------------------------------------------------------

#[test]
fn target_ms_rounds_up_to_whole_hub_blocks() {
    let f = |ms| target_frames_from_ms(ms, 48_000, BLOCK);
    assert_eq!(f(16.0), Some(768));
    assert_eq!(f(37.33), Some(1792));
    assert_eq!(f(37.3), Some(1792));
    assert_eq!(f(42.67), Some(CARD_CAPTURE_TARGET_FRAMES));
    assert_eq!(f(5.0), Some(256));
    assert_eq!(f(5.4), Some(512));
    assert_eq!(f(0.0), None);
    assert_eq!(f(-1.0), None);
    assert_eq!(f(250.0), None);
    assert_eq!(f(f64::NAN), None);
    assert_eq!(VBAN_TARGET_BLOCKS * BLOCK, 768);
    assert_eq!(VBAN_PROGRAM_TARGET_BLOCKS * BLOCK, 1792);
}

// ---- Part 2: the two-clock bench on frame counts --------------------------------------------

/// A tiny deterministic PRNG (xorshift) for the jitter.
struct Rng(u64);
impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.unit().max(1e-12), self.unit());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

#[derive(Clone, Copy)]
enum Pattern {
    /// Evenly paced packets of this many frames (a cambox: 128).
    Even(usize),
    /// The FOH sender: 52-frame packets released every 6 ms, held for `gap_ms` every 402 ms.
    Burst { gap_ms: f64 },
}

struct Scenario {
    target: usize,
    cap: usize,
    /// The sender's clock against the hub's, ppm; `step` = (at s, new ppm).
    ppm: f64,
    step: Option<(f64, f64)>,
    jitter_sd_ms: f64,
    pattern: Pattern,
    secs: f64,
    settle_s: f64,
    /// A lost hub tick every this many seconds (0 = never).
    lost_tick_every_s: f64,
    /// A span (from, to s) the depth bound leaves out: the follow after a clock step. Underruns,
    /// skips and resyncs are still counted there.
    depth_transient: Option<(f64, f64)>,
    seed: u64,
}

#[derive(Debug, Default)]
struct Outcome {
    silent_after_settle: u64,
    skips_after_settle: u64,
    resyncs: u64,
    depth_min: f64,
    depth_max: f64,
    target_moved: bool,
    final_rate_ppm: f64,
    /// The worst |depth - target| of a 1 s mean after the settle.
    worst_mean_err: f64,
}

/// The FOH burst sender's release instant for a packet ready at `ts`: it releases its queue every
/// 6 ms, except once every 402 ms it holds it for `gap_ms` (the measured long gap).
fn burst_release(ts: f64, gap_ms: f64) -> f64 {
    let period = 0.402;
    let p = (ts / period).floor();
    let within = ts - p * period;
    let (hold_from, hold_to) = (0.1, 0.1 + gap_ms * 1e-3);
    if (hold_from..hold_to).contains(&within) {
        p * period + hold_to
    } else {
        p * period + (within / 0.006).ceil() * 0.006
    }
}

/// The arrival times of every packet (seconds on the hub clock) and their frame counts.
fn packets(sc: &Scenario) -> Vec<(f64, usize)> {
    let mut rng = Rng(sc.seed | 1);
    let mut out = Vec::new();
    let rate_at = |t: f64| {
        let ppm = match sc.step {
            Some((at, p)) if t >= at => p,
            _ => sc.ppm,
        };
        RATE * (1.0 + ppm * 1e-6)
    };
    let frames = match sc.pattern {
        Pattern::Even(n) => n,
        Pattern::Burst { .. } => 52,
    };
    let mut ts = 0.0f64;
    while ts < sc.secs {
        // The sender's clock: a packet is ready once its frames are captured.
        ts += frames as f64 / rate_at(ts);
        let jitter = sc.jitter_sd_ms * 1e-3 * rng.gauss().abs();
        let at = match sc.pattern {
            Pattern::Even(_) => ts + jitter,
            Pattern::Burst { gap_ms } => burst_release(ts, gap_ms) + jitter,
        };
        out.push((at + 0.000_37, frames));
    }
    // FIFO: a packet never overtakes the one before it.
    for i in 1..out.len() {
        if out[i].0 < out[i - 1].0 {
            out[i].0 = out[i - 1].0;
        }
    }
    out
}

fn run(sc: &Scenario) -> Outcome {
    let mut ctl = NetworkFill::new(sc.target, sc.cap);
    let pk = packets(sc);
    let mut fill = 0usize;
    let mut next = 0usize;
    let pops = (sc.secs * RATE / BLOCK as f64) as u64;
    let lost_every = if sc.lost_tick_every_s > 0.0 {
        (sc.lost_tick_every_s * RATE / BLOCK as f64) as u64
    } else {
        0
    };
    let mut o = Outcome {
        depth_min: f64::INFINITY,
        depth_max: f64::NEG_INFINITY,
        ..Default::default()
    };
    let (mut win_sum, mut win_n) = (0.0, 0u32);
    let mut lost_pending = 0u64;
    for pop in 0..pops {
        let t = pop as f64 * BLOCK as f64 / RATE;
        while next < pk.len() && pk[next].0 <= t {
            fill += pk[next].1;
            ctl.note_arrival(pk[next].1, pk[next].0);
            next += 1;
            if let Some(keep) = ctl.overrun_keep(fill) {
                fill = keep;
            }
        }
        if lost_every > 0 && pop > 0 && pop.is_multiple_of(lost_every) {
            // This tick is lost: no pop now; the next cycle gives its block up right before its
            // own pop, as main.rs does (`skip_missed`, then `pop_block`).
            lost_pending += 1;
            continue;
        }
        if lost_pending > 0 {
            fill -= ctl.discard_for_missed_ticks(fill, BLOCK, lost_pending);
            lost_pending = 0;
        }
        let read = ctl.read_point();
        let settled = t >= sc.settle_s;
        let primed_before = ctl.stats().primed;
        let depth = read.depth(fill);
        match ctl.plan_pop(fill, BLOCK) {
            PopPlan::Silent { .. } => {
                if settled {
                    o.silent_after_settle += 1;
                }
            }
            PopPlan::Audio { skip, consume, .. } => {
                if settled && skip > 0 && primed_before {
                    o.skips_after_settle += 1;
                }
                fill -= skip + consume;
                let transient = sc
                    .depth_transient
                    .is_some_and(|(from, to)| (from..to).contains(&t));
                if settled && primed_before && !transient {
                    o.depth_min = o.depth_min.min(depth);
                    o.depth_max = o.depth_max.max(depth);
                    win_sum += depth;
                    win_n += 1;
                    if win_n == 188 {
                        let err = (win_sum / f64::from(win_n) - sc.target as f64).abs();
                        o.worst_mean_err = o.worst_mean_err.max(err);
                        win_sum = 0.0;
                        win_n = 0;
                    }
                }
            }
        }
        o.target_moved |= ctl.target() != sc.target;
    }
    let s = ctl.stats();
    o.resyncs = s.resyncs;
    o.final_rate_ppm = s.rate_ppm;
    o
}

fn cambox(ppm: f64, hours: f64, seed: u64) -> Scenario {
    Scenario {
        target: VBAN_TARGET_BLOCKS * BLOCK,
        cap: vban_cap_blocks(VBAN_TARGET_BLOCKS) * BLOCK,
        ppm,
        step: None,
        jitter_sd_ms: 1.0,
        pattern: Pattern::Even(128),
        secs: hours * 3600.0,
        settle_s: 240.0,
        lost_tick_every_s: 0.0,
        depth_transient: None,
        seed,
    }
}

fn assert_held(sc: &Scenario, o: &Outcome) {
    let band = sc.target as f64 / 4.0;
    assert_eq!(
        o.silent_after_settle, 0,
        "no underrun after the settle: {o:?}"
    );
    assert_eq!(
        o.skips_after_settle, 0,
        "no frame skipped after the settle: {o:?}"
    );
    assert_eq!(o.resyncs, 0, "no resync: {o:?}");
    assert!(!o.target_moved, "the target never moves");
    assert!(
        o.worst_mean_err <= band,
        "every 1 s mean depth within +-{band} of the target: {o:?}"
    );
}

#[test]
fn a_cambox_at_plus_and_minus_50ppm_holds_its_depth_for_two_hours() {
    for (ppm, seed) in [(50.0, 1), (-50.0, 2)] {
        let sc = cambox(ppm, 2.0, seed);
        let o = run(&sc);
        assert_held(&sc, &o);
        assert!(
            (o.final_rate_ppm - ppm).abs() < 3.0,
            "the ratio follows the sender: {o:?}"
        );
    }
}

#[test]
fn a_cambox_at_plus_and_minus_500ppm_holds_its_depth_for_two_hours() {
    for (ppm, seed) in [(500.0, 3), (-500.0, 4)] {
        let sc = cambox(ppm, 2.0, seed);
        let o = run(&sc);
        assert_held(&sc, &o);
        assert!(
            (o.final_rate_ppm - ppm).abs() < 3.0,
            "the ratio follows the sender: {o:?}"
        );
    }
}

#[test]
fn a_100ppm_clock_step_is_followed_without_a_fault() {
    // The 64 s fit lags a step: while it catches up the depth walks about 150 frames (3 ms) and
    // the phase term brings it back. No frame is skipped, no block silent, no resync, at any time;
    // the depth bound applies again 3 min after the step.
    let mut sc = cambox(0.0, 1.0, 5);
    sc.step = Some((1800.0, 100.0));
    sc.depth_transient = Some((1800.0, 1980.0));
    let o = run(&sc);
    assert_held(&sc, &o);
    assert!((o.final_rate_ppm - 100.0).abs() < 3.0, "{o:?}");
    let mut whole = cambox(0.0, 1.0, 5);
    whole.step = Some((1800.0, 100.0));
    let w = run(&whole);
    assert!(
        w.worst_mean_err <= whole.target as f64 / 2.0,
        "even during the follow the depth stays within half the target: {w:?}"
    );
}

#[test]
fn the_program_feed_rides_the_foh_burst_pattern_at_its_fixed_target() {
    let target = VBAN_PROGRAM_TARGET_BLOCKS * BLOCK;
    let sc = Scenario {
        target,
        cap: vban_cap_blocks(VBAN_PROGRAM_TARGET_BLOCKS) * BLOCK,
        ppm: -20.0,
        step: None,
        jitter_sd_ms: 0.3,
        pattern: Pattern::Burst { gap_ms: 27.6 },
        secs: 3600.0,
        settle_s: 240.0,
        lost_tick_every_s: 0.0,
        depth_transient: None,
        seed: 6,
    };
    let o = run(&sc);
    assert_held(&sc, &o);
}

#[test]
fn a_lost_hub_tick_is_given_up_at_once_and_the_depth_stays() {
    let mut sc = cambox(20.0, 1.0, 7);
    sc.lost_tick_every_s = 30.0;
    let o = run(&sc);
    assert_held(&sc, &o);
}

#[test]
fn a_sender_past_the_clamp_is_counted_never_adapted_to() {
    // 600 ppm is 100 ppm past the clamp: the leg keeps drifting high, and the cap answers it with
    // counted resyncs back to the SAME target, never a moved target.
    let sc = cambox(600.0, 1.0, 8);
    let o = run(&sc);
    assert!(o.resyncs > 0, "{o:?}");
    assert!(!o.target_moved);
    assert!((o.final_rate_ppm - ASRC_MAX_PPM).abs() < 0.2, "{o:?}");
}
