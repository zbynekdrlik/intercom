//! The THD+N grader of the issue-1461 crossing tests (`asrc_leg_1461.rs`, `egress_asrc_1461.rs`):
//! every 50 ms window of a leg's output is fitted with a sine of free frequency, phase and offset;
//! the residual against that fit is the window's THD+N. A single-frame drop or repeat (even spread
//! over a block) is a phase step inside its window, about -30 dB; a resampler that follows the
//! clocks leaves only the i16 rounding (~ -95 dB). Std-only, mounted by `#[path]`.

#![allow(dead_code)]

/// The THD+N window: 50 ms at 48 kHz.
pub const WINDOW: usize = 2_400;

/// The bar issue 1461 sets for every window after the settle.
pub const THD_N_MAX_DB: f64 = -80.0;

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
pub fn thd_n_db(x: &[f64], w0: f64) -> f64 {
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

/// Every whole 50 ms window's THD+N for a tone near `w0` rad/sample: (the worst window, the
/// windows over [`THD_N_MAX_DB`], the windows).
pub fn grade_windows(out: &[i16], w0: f64) -> (f64, usize, usize) {
    let mut worst = f64::NEG_INFINITY;
    let mut over = 0;
    let mut windows = 0;
    for chunk in out.as_chunks::<WINDOW>().0 {
        let x: Vec<f64> = chunk.iter().map(|&v| f64::from(v)).collect();
        let db = thd_n_db(&x, w0);
        worst = worst.max(db);
        over += usize::from(db > THD_N_MAX_DB);
        windows += 1;
    }
    (worst, over, windows)
}
