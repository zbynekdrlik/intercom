//! The hub's shared windowed-sinc FIR machinery (issue 1345): Kaiser low-pass taps + a stateful
//! integer decimator.
//!
//! Two resamplers use it: the PCMU leg's 48 kHz -> 8 kHz anti-alias decimator
//! ([`crate::mulaw::Decimator48kTo8k`]) and the VBAN ingress 96/192 kHz -> 48 kHz decimator
//! ([`crate::vban_rate`]). Pure, std-only, so it verifies under Tier-0 with a `rustc --test` replica.

/// The modified Bessel function of the first kind, order 0 (the Kaiser window's kernel), by its
/// power series — converges in well under 40 terms for the betas used here.
pub fn bessel_i0(x: f64) -> f64 {
    let half = x / 2.0;
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..64 {
        let f = half / k as f64;
        term *= f * f;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// `n` Kaiser-windowed-sinc low-pass taps with a -6 dB cut-off at `cutoff` (cycles per INPUT
/// sample) and Kaiser shape `beta`, normalised to exactly unity DC gain (so a constant input maps to
/// that constant). An odd `n` gives a linear-phase filter with an integer group delay of
/// `(n - 1) / 2` samples. `n <= 1` is the identity filter.
pub fn kaiser_lowpass_taps(n: usize, cutoff: f64, beta: f64) -> Vec<f64> {
    if n <= 1 {
        return vec![1.0];
    }
    let span = (n - 1) as f64;
    let i0_beta = bessel_i0(beta);
    let mut taps: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - span / 2.0;
            let sinc = if t == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f64::consts::PI * cutoff * t).sin() / (std::f64::consts::PI * t)
            };
            let r = 2.0 * i as f64 / span - 1.0;
            let window = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
            sinc * window
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    for t in &mut taps {
        *t /= sum;
    }
    taps
}

/// A STATEFUL integer decimator: an FIR low-pass evaluated only at every `factor`-th input sample.
///
/// Its circular history and its `factor`:1 phase carry across calls, so feeding a stream in chunks
/// of any size produces exactly the output of one pass over the whole stream — no restart transient
/// at a chunk boundary.
#[derive(Debug, Clone)]
pub struct FirDecimator {
    taps: Vec<f64>,
    /// Circular history of the last `taps.len()` input samples.
    history: Vec<f64>,
    /// Where the NEXT input sample is written (== the oldest sample once written).
    pos: usize,
    /// Input samples consumed since the last output (`0..factor`).
    phase: usize,
    factor: usize,
}

impl FirDecimator {
    /// A decimator starting from silence. An empty `taps` is the identity filter; a `factor` of 0 is
    /// treated as 1.
    pub fn new(taps: Vec<f64>, factor: usize) -> Self {
        Self::primed(taps, factor, 0)
    }

    /// A decimator whose history is pre-filled with `x0` — the steady state of a constant `x0` input,
    /// so a DC block maps to that constant from its very first output.
    pub fn primed(taps: Vec<f64>, factor: usize, x0: i16) -> Self {
        let taps = if taps.is_empty() { vec![1.0] } else { taps };
        FirDecimator {
            history: vec![x0 as f64; taps.len()],
            taps,
            pos: 0,
            phase: 0,
            factor: factor.max(1),
        }
    }

    /// The decimation ratio (inputs per output).
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Restart from silence.
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.pos = 0;
        self.phase = 0;
    }

    /// Feed any number of input samples; returns the output samples completed by them (one per
    /// `factor` inputs, counted across calls), rounded and clamped to i16.
    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let len = self.history.len();
        let mut out = Vec::with_capacity((self.phase + input.len()) / self.factor);
        for &x in input {
            self.history[self.pos] = x as f64;
            self.pos = (self.pos + 1) % len;
            self.phase += 1;
            if self.phase == self.factor {
                self.phase = 0;
                // Oldest sample first: history[pos..] then history[..pos], aligned with taps[0..].
                let (newer, older) = self.history.split_at(self.pos);
                let acc: f64 = self
                    .taps
                    .iter()
                    .zip(older.iter().chain(newer.iter()))
                    .map(|(t, s)| t * s)
                    .sum();
                out.push(acc.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            }
        }
        out
    }
}
