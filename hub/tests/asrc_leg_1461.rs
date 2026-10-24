//! Issue 1461 (lane 1, design comment 6376769274): a VBAN leg crosses its sender's clock into the
//! hub's mix clock with a resampler, never by inserting or dropping a frame.
//!
//! Graded black-box through the real [`JitterBuffer`] a VBAN leg runs (`vban_leg`, `push_at`,
//! `pop_block_at`): a cambox sends a 1 kHz sine (-6 dBFS, mono, 128-frame packets) on a clock
//! `ppm` off the hub's; the hub pops 256-frame blocks on its own exact grid. After the settle,
//! every 50 ms window of the leg's output is fitted with a sine of free frequency, phase and
//! offset; the residual against that fit is the window's THD+N. A single-frame drop or repeat
//! (the issue-1401 servo, even spread over a block) is a phase step inside its window, about
//! -30 dB; a resampler that follows the sender's rate leaves only the i16 rounding (~ -95 dB).

use std::time::{Duration, Instant};

use intercom_hub::vban_io::{DecodedAudio, JitterBuffer};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;
const PACKET: usize = 128;
const TONE_HZ: f64 = 1000.0;
const AMP: f64 = 16_384.0;
/// The THD+N window: 50 ms.
const WINDOW: usize = 2_400;
/// The bar the issue sets for every window after the settle.
const THD_N_MAX_DB: f64 = -80.0;

/// The sender's sample `n` of the tone (its own clock).
fn tone(n: u64) -> i16 {
    let phase = 2.0 * std::f64::consts::PI * TONE_HZ * n as f64 / RATE;
    (AMP * phase.sin()).round() as i16
}

/// Run one mono cambox leg at `ppm` for `settle_s + measure_s` seconds; return the output samples
/// of the measured part.
fn run_leg(ppm: f64, settle_s: f64, measure_s: f64) -> Vec<i16> {
    run_leg_with(ppm, None, settle_s, measure_s)
}

/// [`run_leg`] with an optional clock step: from `step.0` seconds on, the sender runs `step.1` ppm
/// off the hub.
fn run_leg_with(ppm: f64, step: Option<(f64, f64)>, settle_s: f64, measure_s: f64) -> Vec<i16> {
    let t0 = Instant::now();
    let at = |secs: f64| t0 + Duration::from_secs_f64(secs);
    let mut jb = JitterBuffer::vban_leg(9 * BLOCK, 3 * BLOCK);
    let rate_at = |t: f64| {
        let p = match step {
            Some((from, p)) if t >= from => p,
            _ => ppm,
        };
        RATE * (1.0 + p * 1e-6)
    };
    // The sender's phase against the hub grid: never on a pop.
    let offset = 0.000_37;
    let total = settle_s + measure_s;
    let mut next_packet = 0u64;
    // The time the next packet is complete, on the sender's (possibly stepped) clock.
    let mut t_ready = PACKET as f64 / rate_at(0.0);
    let mut out = Vec::new();
    let pops = (total * RATE / BLOCK as f64) as u64;
    for pop in 0..pops {
        let t_pop = pop as f64 * BLOCK as f64 / RATE;
        // Every packet the sender finished before this pop.
        loop {
            let first = next_packet * PACKET as u64;
            let t_sent = offset + t_ready;
            if t_sent > t_pop {
                break;
            }
            t_ready += PACKET as f64 / rate_at(t_ready);
            let samples: Vec<i16> = (first..first + PACKET as u64).map(tone).collect();
            jb.push_at(
                &DecodedAudio::from_planar("cam1".into(), vec![samples]),
                at(t_sent),
            );
            next_packet += 1;
        }
        let block = jb.pop_block_at(BLOCK, at(t_pop));
        if t_pop >= settle_s {
            out.extend_from_slice(&block[0]);
        }
    }
    out
}

/// Least squares of `x` on `a cos(w n) + b sin(w n) + c` (n centred on the window): the residual
/// power and the fitted tone's power.
fn fit_at(x: &[f64], w: f64) -> (f64, f64) {
    let n = x.len();
    let half = (n as f64 - 1.0) / 2.0;
    let (cw, sw) = (w.cos(), w.sin());
    // The phasor at the first sample, then rotated sample by sample.
    let (mut c, mut s) = ((-w * half).cos(), (-w * half).sin());
    let (mut scc, mut sss, mut scs, mut sc, mut ss) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let (mut sxc, mut sxs, mut sx) = (0.0, 0.0, 0.0);
    let mut cs = Vec::with_capacity(n);
    for &v in x {
        scc += c * c;
        sss += s * s;
        scs += c * s;
        sc += c;
        ss += s;
        sxc += v * c;
        sxs += v * s;
        sx += v;
        cs.push((c, s));
        let nc = c * cw - s * sw;
        s = s * cw + c * sw;
        c = nc;
    }
    let m = [[scc, scs, sc], [scs, sss, ss], [sc, ss, n as f64]];
    let rhs = [sxc, sxs, sx];
    let [a, b, k] = solve3(m, rhs);
    let mut resid = 0.0;
    let mut sig = 0.0;
    for (&v, &(c, s)) in x.iter().zip(&cs) {
        let tone = a * c + b * s;
        let e = v - tone - k;
        resid += e * e;
        sig += tone * tone;
    }
    (resid, sig)
}

fn solve3(m: [[f64; 3]; 3], r: [f64; 3]) -> [f64; 3] {
    let det = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det(m);
    let mut out = [0.0; 3];
    for (col, o) in out.iter_mut().enumerate() {
        let mut mc = m;
        for row in 0..3 {
            mc[row][col] = r[row];
        }
        *o = det(mc) / d;
    }
    out
}

/// THD+N of one window in dB: the best sine of free frequency (within +-0.1 % of `w0`), phase and
/// offset, by a golden-section search on the frequency.
fn thd_n_db(x: &[f64], w0: f64) -> f64 {
    let g = (5f64.sqrt() - 1.0) / 2.0;
    let (mut lo, mut hi) = (w0 * (1.0 - 1e-3), w0 * (1.0 + 1e-3));
    let mut x1 = hi - g * (hi - lo);
    let mut x2 = lo + g * (hi - lo);
    let mut f1 = fit_at(x, x1).0;
    let mut f2 = fit_at(x, x2).0;
    for _ in 0..60 {
        if f1 < f2 {
            hi = x2;
            x2 = x1;
            f2 = f1;
            x1 = hi - g * (hi - lo);
            f1 = fit_at(x, x1).0;
        } else {
            lo = x1;
            x1 = x2;
            f1 = f2;
            x2 = lo + g * (hi - lo);
            f2 = fit_at(x, x2).0;
        }
    }
    let (resid, sig) = fit_at(x, (lo + hi) / 2.0);
    10.0 * (resid.max(1e-30) / sig.max(1e-30)).log10()
}

/// Every 50 ms window's THD+N: (the worst window, the windows over the bar, the windows).
fn grade(out: &[i16], ppm: f64) -> (f64, usize, usize) {
    let w0 = 2.0 * std::f64::consts::PI * TONE_HZ * (1.0 + ppm * 1e-6) / RATE;
    let mut worst = f64::NEG_INFINITY;
    let mut over = 0;
    let mut windows = 0;
    for chunk in out.chunks_exact(WINDOW) {
        let x: Vec<f64> = chunk.iter().map(|&v| f64::from(v)).collect();
        let db = thd_n_db(&x, w0);
        worst = worst.max(db);
        over += usize::from(db > THD_N_MAX_DB);
        windows += 1;
    }
    (worst, over, windows)
}

fn assert_clean(ppm: f64) {
    let out = run_leg(ppm, 30.0, 30.0);
    let (worst, over, windows) = grade(&out, ppm);
    assert!(windows >= 590, "{windows} windows measured");
    assert_eq!(
        over, 0,
        "at {ppm:+} ppm {over} of {windows} 50 ms windows over {THD_N_MAX_DB} dB THD+N (worst {worst:.1} dB): a frame was inserted or dropped"
    );
    assert!(worst < THD_N_MAX_DB, "worst window {worst:.1} dB");
}

#[test]
fn a_fast_cambox_is_resampled_never_a_frame_dropped() {
    assert_clean(50.0);
}

#[test]
fn a_slow_cambox_is_resampled_never_a_frame_repeated() {
    assert_clean(-50.0);
}

#[test]
fn a_cambox_at_500ppm_is_resampled_never_a_frame_inserted_or_dropped() {
    assert_clean(500.0);
    assert_clean(-500.0);
}

#[test]
fn a_100ppm_clock_step_is_followed_without_a_click() {
    // The sender's clock steps from 0 to +100 ppm 40 s in; the 60 s after it (the 64 s fit
    // catching up, the ratio following at 5 ppm/s) are graded window by window.
    let out = run_leg_with(0.0, Some((40.0, 100.0)), 40.0, 60.0);
    let (worst, over, windows) = grade(&out, 100.0);
    assert!(windows >= 1190, "{windows} windows measured");
    assert_eq!(
        over, 0,
        "{over} of {windows} windows over {THD_N_MAX_DB} dB after the step (worst {worst:.1} dB)"
    );
}

#[test]
fn the_grader_sees_a_single_dropped_frame() {
    // The grader's own control: one frame cut out of a clean tone reads far over the bar.
    let mut x: Vec<i16> = (0..WINDOW as u64 + 1).map(tone).collect();
    x.remove(WINDOW / 2);
    let (worst, over, _) = grade(&x[..WINDOW], 0.0);
    assert_eq!(
        over, 1,
        "a dropped frame must fail its window ({worst:.1} dB)"
    );
    let clean: Vec<i16> = (0..WINDOW as u64).map(tone).collect();
    let (worst, over, _) = grade(&clean, 0.0);
    assert_eq!(over, 0, "a clean tone passes ({worst:.1} dB)");
}
