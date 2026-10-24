//! The hub's clock-domain crossing (issue 1461, design comment 6376769274): a variable-ratio
//! polyphase windowed-sinc resampler and the rate controller that drives it.
//!
//! Every input the hub mixes runs on a clock of its own: a cambox headset ADC, the FOH desk, the
//! cutters' USB card crystal. The mix runs on `CLOCK_MONOTONIC` (dantesync rate-disciplined) on
//! an exact 256-frame grid. Issue 1401 crossed that boundary by dropping or repeating single
//! frames; the owner's genlock rule (issue 1461) is that a crossing is a RESAMPLER and the
//! latency is fixed from the start, the same model the OBS genlock ASRC and the obs-vban receive
//! timeline use (ROZHODNUTÉ 6343922049).
//!
//! - [`ReadPoint`] is the resampler's state at frame-count level: the fractional read position
//!   inside a leg's queued input (it keeps [`ASRC_HISTORY`] frames behind it and reads
//!   [`ASRC_LOOKAHEAD`] ahead) and the ratio (input frames per output frame, `1 + e`, clamped to
//!   +-[`ASRC_MAX_PPM`]). [`ReadPoint::frames_needed`] and [`ReadPoint::advanced`] are pure
//!   arithmetic, so the hours-long benches run on frame counts alone.
//! - [`render`] / [`render_interleaved`] turn a read point and the queued samples into the output:
//!   a Kaiser-windowed sinc from [`crate::fir::kaiser_lowpass_taps`], [`ASRC_TAPS`] taps x
//!   [`ASRC_PHASES`] phases, linear interpolation between phases. Each phase row sums to exactly 1
//!   (DC gain 1 at every phase); the cut-off is the input Nyquist, so phase 0 is the identity
//!   (unity ratio = delayed identity, bit for bit). One shared table, nothing allocated per block.
//! - [`RateControl`] decides the ratio, the obs-vban receive model: the RATE is the least-squares
//!   slope of the leg's arrived frames against each packet's arrival time on the hub's clock,
//!   over a [`RATE_WINDOW_SECONDS`] window of 1 s bins (the arrivals' drift against the hub's
//!   clock IS the clock offset; a lost packet only shifts the series, and the slope's bias then
//!   gives those frames back), followed at [`RATE_FOLLOW_PPM_PER_S`]; a phase term starts only
//!   outside a dead band of +-1/4 of the target and then brings the depth back to +-1/8.
//!   Nothing else moves the depth: after the prime it is never trimmed or padded again.
//!
//! Pure and std-only, so it verifies with a plain `rustc --test` replica under Tier-0 (issue 557).

use std::collections::VecDeque;
use std::sync::OnceLock;

use crate::fir::kaiser_lowpass_taps;

/// Taps of the interpolation filter (input frames it reads per output frame).
pub const ASRC_TAPS: usize = 32;

/// Phases of the polyphase table; the read position between two of them is linearly interpolated.
pub const ASRC_PHASES: usize = 128;

/// Input frames the read point keeps behind it for the filter.
pub const ASRC_HISTORY: usize = ASRC_TAPS / 2 - 1;

/// Input frames the filter reads ahead of the read point.
pub const ASRC_LOOKAHEAD: usize = ASRC_TAPS / 2;

/// The ratio is clamped to `1 +- ASRC_MAX_PPM * 1e-6`. 1000 ppm, the range the issue-1401 servo
/// covered (977 ppm): the cam1 headset ADC measured +540 ppm (`capture 48026 samp/s`, 19.9.2026),
/// so the design's +-500 would have left it ~1.9 frames/s of uncorrected drift and a counted resync
/// (a ~32 ms cut of the talkback) every ~13 min (issue 1461 review round 1). The filter is the same
/// at either clamp.
pub const ASRC_MAX_PPM: f64 = 1000.0;

/// The Kaiser shape of the prototype. The cut-off is the input Nyquist (24 kHz) and the 32 taps
/// leave a transition of about 19-29 kHz, so the ~100 dB stop band starts near 29 kHz (images of
/// program content up to ~19 kHz). A fractional read lands within -97 dB of the continuous signal
/// up to 10 kHz (-88 dB at 16 kHz); beta 8 left -82 dB at 1 kHz.
pub const ASRC_KAISER_BETA: f64 = 10.0;

/// The rate fit's window in 1 s bins.
pub const RATE_WINDOW_SECONDS: usize = 64;

/// The fewest 1 s bins the rate fit is used from (after a start or a stall).
pub const RATE_MIN_FIT_SECONDS: usize = 4;

/// How fast the ratio follows the fitted rate once it has reached it: ppm per second.
pub const RATE_FOLLOW_PPM_PER_S: f64 = 5.0;

/// How fast the ratio moves until it first reaches its goal (the acquisition after a hub start):
/// ppm per second. A pitch ramp of 250 ppm/s is inaudible; at the follow speed a 500 ppm sender
/// would walk its leg ~1400 frames past the target before the rate caught up. With the first fit
/// after [`RATE_MIN_FIT_SECONDS`] a 500 ppm sender's start leaves about 150 frames of offset,
/// inside a cambox's dead band.
pub const RATE_ACQUIRE_PPM_PER_S: f64 = 250.0;

/// The phase term's time constant: a depth `excess` frames outside the dead band asks for
/// `excess / (PHASE_TIME_CONSTANT_S * rate)` of ratio, so it is taken out in about this long.
pub const PHASE_TIME_CONSTANT_S: f64 = 30.0;

/// The phase term never asks for more than this, ppm.
pub const PHASE_MAX_PPM: f64 = 200.0;

/// The polyphase table: row `p` (0..=ASRC_PHASES) holds the [`ASRC_TAPS`] coefficients for a read
/// position `p / ASRC_PHASES` past an input frame, coefficient `m` multiplying the input frame
/// `m - ASRC_HISTORY` frames from it. Each row sums to 1.
fn table() -> &'static [f64] {
    static TABLE: OnceLock<Vec<f64>> = OnceLock::new();
    TABLE.get_or_init(|| {
        // The prototype at ASRC_PHASES x the input rate; its sinc's zeros fall on the input frames.
        let proto = kaiser_lowpass_taps(
            ASRC_TAPS * ASRC_PHASES + 1,
            0.5 / ASRC_PHASES as f64,
            ASRC_KAISER_BETA,
        );
        let mut t = Vec::with_capacity((ASRC_PHASES + 1) * ASRC_TAPS);
        for p in 0..=ASRC_PHASES {
            // Input frame offset (m - HISTORY) sits at prototype time p/P + HISTORY - m frames
            // from the centre (index TAPS * PHASES / 2).
            let row: Vec<f64> = (0..ASRC_TAPS)
                .map(|m| proto[p + (ASRC_TAPS - 1 - m) * ASRC_PHASES])
                .collect();
            let sum: f64 = row.iter().sum();
            t.extend(row.iter().map(|c| c / sum));
        }
        t
    })
}

/// Build the polyphase table now. The first render would otherwise build it (4097 Kaiser taps,
/// ~0.4 ms) on the real-time hub-mix thread; every leg constructor calls this at start.
pub fn warm() {
    let _ = table();
}

/// The coefficients for a read position `frac` (0..1) past an input frame: the two neighbouring
/// phase rows, linearly interpolated.
fn coefficients(frac: f64, out: &mut [f64; ASRC_TAPS]) {
    let t = table();
    let x = frac.clamp(0.0, 1.0) * ASRC_PHASES as f64;
    let p = (x.floor() as usize).min(ASRC_PHASES - 1);
    let a = x - p as f64;
    let (r0, r1) = (
        &t[p * ASRC_TAPS..(p + 1) * ASRC_TAPS],
        &t[(p + 1) * ASRC_TAPS..(p + 2) * ASRC_TAPS],
    );
    for (m, c) in out.iter_mut().enumerate() {
        *c = r0[m] + a * (r1[m] - r0[m]);
    }
}

/// The resampler's state in frames: where the next output frame reads in a leg's queued input
/// (`pos`, counted from the queue's oldest frame, at least [`ASRC_HISTORY`]) and how many input
/// frames one output frame advances it (`ratio`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReadPoint {
    pub pos: f64,
    pub ratio: f64,
}

impl ReadPoint {
    /// A read point right after a prime: [`ASRC_HISTORY`] frames of history, on an input frame.
    pub fn at_prime(ratio: f64) -> Self {
        ReadPoint {
            pos: ASRC_HISTORY as f64,
            ratio,
        }
    }

    /// The input frames the queue must hold (history included) to render `out_frames` frames.
    pub fn frames_needed(&self, out_frames: usize) -> usize {
        if out_frames == 0 {
            return 0;
        }
        let last = self.pos + (out_frames - 1) as f64 * self.ratio;
        last.floor() as usize + ASRC_LOOKAHEAD + 1
    }

    /// The input frames queued ahead of the read point, the leg's depth.
    pub fn depth(&self, queued: usize) -> f64 {
        queued as f64 - self.pos
    }

    /// The read point after `out_frames` output frames, and how many of the oldest input frames
    /// the queue can then drop (the read point keeps its [`ASRC_HISTORY`]).
    pub fn advanced(&self, out_frames: usize) -> (ReadPoint, usize) {
        let end = self.pos + out_frames as f64 * self.ratio;
        let drop = (end.floor() as usize).saturating_sub(ASRC_HISTORY);
        (
            ReadPoint {
                pos: end - drop as f64,
                ratio: self.ratio,
            },
            drop,
        )
    }
}

/// Run `f(frame, base, coefficients)` for every output frame of `out_frames` from `read`: `base`
/// is the first input frame the coefficients multiply.
fn for_each_frame(
    read: ReadPoint,
    out_frames: usize,
    mut f: impl FnMut(usize, usize, &[f64; ASRC_TAPS]),
) {
    let mut c = [0.0; ASRC_TAPS];
    for k in 0..out_frames {
        let t = read.pos + k as f64 * read.ratio;
        let i = t.floor();
        coefficients(t - i, &mut c);
        f(k, (i as usize).saturating_sub(ASRC_HISTORY), &c);
    }
}

/// One output sample: the 32 coefficients over the 32 input frames from `base`, or `None` when the
/// taps run past the input. Eight partial sums, so the compiler can keep them in vector lanes (a
/// single running sum is a serial chain it may not reorder).
fn dot(c: &[f64; ASRC_TAPS], input: &[i16], base: usize) -> Option<i16> {
    let x: &[i16; ASRC_TAPS] = input.get(base..base + ASRC_TAPS)?.try_into().ok()?;
    let mut acc = [0.0f64; 8];
    for (cs, xs) in c.chunks(8).zip(x.chunks(8)) {
        for ((a, &c), &x) in acc.iter_mut().zip(cs).zip(xs) {
            *a += c * f64::from(x);
        }
    }
    Some(to_i16(acc.iter().sum()))
}

fn to_i16(acc: f64) -> i16 {
    acc.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// Render one channel: `out.len()` frames from `read` over `input` (the leg's queued frames, the
/// oldest first). A frame whose taps run past the input renders silence (the caller checks
/// [`ReadPoint::frames_needed`] first, so this never happens on a planned pop).
pub fn render(read: ReadPoint, input: &[i16], out: &mut [i16]) {
    for_each_frame(read, out.len(), |k, base, c| {
        out[k] = dot(c, input, base).unwrap_or(0);
    });
}

/// Render several planar channels at once: `outs[ch]` from `inputs[ch]`, every output as long as
/// the first. The coefficients are computed once per frame for every channel (the hub's pop path).
pub fn render_planar(read: ReadPoint, inputs: &[&[i16]], outs: &mut [Vec<i16>]) {
    let frames = outs.first().map_or(0, Vec::len);
    for_each_frame(read, frames, |k, base, c| {
        for (input, out) in inputs.iter().zip(outs.iter_mut()) {
            if let Some(slot) = out.get_mut(k) {
                *slot = dot(c, input, base).unwrap_or(0);
            }
        }
    });
}

/// Render `channels` interleaved channels: `out.len() / channels` frames from `read` over the
/// interleaved `input`. The coefficients are computed once per frame for every channel.
pub fn render_interleaved(read: ReadPoint, input: &[i16], channels: usize, out: &mut [i16]) {
    let n = channels.max(1);
    let frames = out.len() / n;
    for_each_frame(read, frames, |k, base, c| {
        for ch in 0..n {
            let mut acc = 0.0;
            let mut ok = true;
            for (m, c) in c.iter().enumerate() {
                match input.get((base + m) * n + ch) {
                    Some(&x) => acc += c * f64::from(x),
                    None => ok = false,
                }
            }
            out[k * n + ch] = if ok { to_i16(acc) } else { 0 };
        }
    });
}

/// The rate controller of one crossing (see the module doc).
///
/// - Every packet's arrival is reported with its time on the hub's clock and the frames arrived
///   so far ([`RateControl::arrival`]). Points are averaged into 1 s bins; the sender's rate is the
///   least-squares slope over the last [`RATE_WINDOW_SECONDS`] bins. Timing each packet by its own
///   arrival (never sampling the arrival count at the pops) keeps the packet granularity out of the
///   fit: a 128-frame packet every half pop would otherwise alias into a ~50 s sawtooth of +-64
///   frames and a +-30 ppm wobble of the fitted rate.
/// - Once per hub block [`RateControl::step`] gets the leg's depth error; every second the goal
///   becomes the fitted rate plus the phase term (outside the dead band only), and the ratio moves
///   toward it at [`RATE_FOLLOW_PPM_PER_S`] ([`RATE_ACQUIRE_PPM_PER_S`] until it first arrives).
#[derive(Debug, Clone)]
pub struct RateControl {
    /// The hub rate in Hz (output frames per second).
    rate: f64,
    ratio: f64,
    /// The ratio has reached its goal once since the hub started.
    acquired: bool,
    /// The ratio the controller moves toward, `None` until the first fit.
    goal: Option<f64>,
    /// The arrival bin being filled: its index (seconds), sums of time (frames) and arrivals.
    bin: Option<u64>,
    bin_t: f64,
    bin_a: f64,
    bin_n: u32,
    /// Completed 1 s bins (mean time, mean arrivals), the oldest first.
    bins: VecDeque<(f64, f64)>,
    fitted: Option<f64>,
    /// The depth-error second being filled (output frames counted, error sum, blocks).
    err_frames: usize,
    err_sum: f64,
    err_n: u32,
    /// The phase term is acting: the depth left the dead band and has not yet come back inside
    /// half of it.
    phasing: bool,
}

impl RateControl {
    /// A controller at ratio 1 for a hub running at `rate` Hz.
    pub fn new(rate: u32) -> Self {
        RateControl {
            rate: f64::from(rate.max(1)),
            ratio: 1.0,
            acquired: false,
            goal: None,
            bin: None,
            bin_t: 0.0,
            bin_a: 0.0,
            bin_n: 0,
            bins: VecDeque::with_capacity(RATE_WINDOW_SECONDS + 1),
            fitted: None,
            err_frames: 0,
            err_sum: 0.0,
            err_n: 0,
            phasing: false,
        }
    }

    /// The ratio now (input frames per output frame).
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// The ratio in ppm off 1.
    pub fn rate_ppm(&self) -> f64 {
        (self.ratio - 1.0) * 1e6
    }

    /// The last fitted sender rate in ppm off the hub's, `None` before the first fit.
    pub fn fitted_ppm(&self) -> Option<f64> {
        self.fitted.map(|r| (r - 1.0) * 1e6)
    }

    /// The stream restarted (a stall, a reshaped sender): its arrival series starts over. The
    /// ratio is kept (the sender's clock did not change) and held until the next fit.
    pub fn restart(&mut self) {
        self.err_frames = 0;
        self.err_sum = 0.0;
        self.err_n = 0;
        self.phasing = false;
        self.bins.clear();
        self.bin = None;
        self.bin_n = 0;
        self.bin_t = 0.0;
        self.bin_a = 0.0;
        self.fitted = None;
        self.goal = None;
    }

    /// A packet arrived at `at_s` seconds on the hub's clock (any fixed origin), and `arrived` is
    /// the leg's input frames received since the last [`RateControl::restart`], this packet
    /// included.
    pub fn arrival(&mut self, at_s: f64, arrived: f64) {
        if !at_s.is_finite() || at_s < 0.0 {
            return;
        }
        let idx = at_s.floor() as u64;
        if self.bin.is_some_and(|b| b != idx) {
            self.close_bin();
        }
        self.bin = Some(idx);
        self.bin_t += at_s * self.rate;
        self.bin_a += arrived;
        self.bin_n += 1;
    }

    fn close_bin(&mut self) {
        if self.bin_n > 0 {
            let n = f64::from(self.bin_n);
            self.bins.push_back((self.bin_t / n, self.bin_a / n));
            if self.bins.len() > RATE_WINDOW_SECONDS {
                self.bins.pop_front();
            }
        }
        self.bin_t = 0.0;
        self.bin_a = 0.0;
        self.bin_n = 0;
        if self.bins.len() >= RATE_MIN_FIT_SECONDS {
            if let Some(slope) = least_squares_slope(&self.bins) {
                self.fitted = Some(slope);
            }
        }
    }

    /// One hub block of `block` output frames: `depth_err` = the leg's depth minus its target,
    /// `band` = the dead band (a quarter of the target). Returns the ratio for this block.
    pub fn step(&mut self, depth_err: f64, band: f64, block: usize) -> f64 {
        self.err_sum += depth_err;
        self.err_n += 1;
        self.err_frames += block;
        if self.err_frames >= self.rate as usize {
            let err = self.err_sum / f64::from(self.err_n);
            self.err_frames = 0;
            self.err_sum = 0.0;
            self.err_n = 0;
            if let Some(slope) = self.fitted {
                // A Schmitt trigger on the depth: the term starts only outside the dead band and
                // then brings the depth back to half of it, so a depth a step pushed out does not
                // park on the band's edge where every packet's jitter pokes it out again.
                if err.abs() > band {
                    self.phasing = true;
                } else if err.abs() < band / 2.0 {
                    self.phasing = false;
                }
                let excess = if self.phasing {
                    (err.abs() - band / 2.0).max(0.0).copysign(err)
                } else {
                    0.0
                };
                let phase = (excess / (PHASE_TIME_CONSTANT_S * self.rate))
                    .clamp(-PHASE_MAX_PPM * 1e-6, PHASE_MAX_PPM * 1e-6);
                let lim = ASRC_MAX_PPM * 1e-6;
                self.goal = Some((slope + phase).clamp(1.0 - lim, 1.0 + lim));
            }
        }
        let per_s = if self.acquired {
            RATE_FOLLOW_PPM_PER_S
        } else {
            RATE_ACQUIRE_PPM_PER_S
        };
        let max_step = per_s * 1e-6 * block as f64 / self.rate;
        if let Some(goal) = self.goal {
            let d = goal - self.ratio;
            if d.abs() <= max_step {
                self.ratio = goal;
                self.acquired = true;
            } else {
                self.ratio += max_step.copysign(d);
            }
        }
        self.ratio
    }
}

/// The least-squares slope of `points` (x, y), centred for precision; `None` for fewer than two
/// distinct x.
pub fn least_squares_slope(points: &VecDeque<(f64, f64)>) -> Option<f64> {
    let n = points.len() as f64;
    if points.len() < 2 {
        return None;
    }
    let (mx, my) = points
        .iter()
        .fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x, sy + y));
    let (mx, my) = (mx / n, my / n);
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for &(x, y) in points {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx) * (x - mx);
    }
    (sxx > 0.0).then_some(sxy / sxx)
}
