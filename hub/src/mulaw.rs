//! G.711 µ-law codec + the 48 kHz ↔ 8 kHz resample step (issue 1345 M3a).
//!
//! The Janus audiobridge's plain-RTP participant leg carries **PCMU** (G.711 µ-law, 8 kHz mono, PT
//! 0). The hub mixes at 48 kHz stereo PCM16, so the `janus_rtp` adapter down-samples the phones'
//! N-1 mix to 8 kHz mono + µ-law-encodes it for the RTP send, and µ-law-decodes + up-samples the
//! received room mix back to 48 kHz for the engine's jitter buffer.
//!
//! Pure, std-only (no crate): the whole thing is the ITU-T G.711 reference table + a windowed-sinc
//! low-pass, so it verifies RED→GREEN with a rustc `--test` replica under Tier-0 (issue 557 bans a
//! local cargo compile). µ-law is telephone-band (≈3.4 kHz) talkback speech — the design's stability
//! trade; an Opus upgrade of this leg is the documented next step if quality is short.

use crate::fir::{kaiser_lowpass_taps, FirDecimator};

/// The µ-law bias added to the magnitude before segment extraction (ITU-T G.711 / the Sun reference).
const ULAW_BIAS: i32 = 0x84; // 132

/// Encode one 16-bit linear PCM sample to a G.711 µ-law byte (the SpanDSP/Sun reference: sign +
/// 3-bit segment + 4-bit mantissa, complemented). Matches the canonical vectors `0 → 0xFF`,
/// `+32124 → 0x80`, `-32124 → 0x00`.
pub fn ulaw_encode(sample: i16) -> u8 {
    // Sign-magnitude, biased. Compute in i32 so `BIAS - i16::MIN` never overflows.
    let (mut mag, mask) = if sample < 0 {
        (ULAW_BIAS - sample as i32, 0x7F)
    } else {
        (ULAW_BIAS + sample as i32, 0xFF)
    };
    // The segment is the position of the highest set bit of the biased magnitude (with the low byte
    // forced set) minus 7. `mag | 0xFF` never has the top byte cleared, so `top_bit` is well defined.
    let seg = top_bit((mag | 0xFF) as u32) as i32 - 7;
    let byte = if seg >= 8 {
        // Overflow (near full scale) saturates to the top code.
        0x7F ^ mask
    } else {
        mag >>= seg + 3;
        ((seg << 4) | (mag & 0x0F)) ^ mask
    };
    byte as u8
}

/// Decode one G.711 µ-law byte back to a 16-bit linear PCM sample (the segment midpoint). Matches
/// `0xFF → 0`, `0x80 → 32124`, `0x00 → -32124`.
pub fn ulaw_decode(byte: u8) -> i16 {
    let u = !byte; // undo the µ-law complement
    let mantissa = (u & 0x0F) as i32;
    let exponent = ((u & 0x70) >> 4) as i32;
    let t = ((mantissa << 3) + ULAW_BIAS) << exponent;
    let linear = if u & 0x80 != 0 {
        ULAW_BIAS - t
    } else {
        t - ULAW_BIAS
    };
    linear as i16
}

/// Position (0-based) of the most-significant set bit of a non-zero `x`. `top_bit(0xFF) == 7`,
/// `top_bit(0x4000) == 14`.
#[inline]
fn top_bit(x: u32) -> u32 {
    // x is always >= 0xFF here (never 0), so 31 - leading_zeros is exact.
    31 - x.leading_zeros()
}

/// Encode an interleaved-nothing mono 16-bit block as µ-law bytes (one byte per sample).
pub fn ulaw_encode_block(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().map(|&s| ulaw_encode(s)).collect()
}

/// Decode a µ-law byte block back to mono 16-bit PCM (one sample per byte).
pub fn ulaw_decode_block(bytes: &[u8]) -> Vec<i16> {
    bytes.iter().map(|&b| ulaw_decode(b)).collect()
}

/// The 6:1 ratio between the hub's 48 kHz mix rate and the PCMU 8 kHz RTP rate.
pub const RESAMPLE_RATIO: usize = 6;

/// The hub's mix rate, the rate the anti-alias FIR is designed at.
const MIX_RATE_HZ: f64 = 48_000.0;

/// Number of taps of the 48 kHz -> 8 kHz anti-alias FIR. Odd, so the filter is linear-phase with an
/// integer group delay of `(N - 1) / 2` = 120 samples (2.5 ms at 48 kHz — negligible for talkback).
pub const DECIMATOR_TAPS: usize = 241;

/// The -6 dB cut-off of the anti-alias low-pass. With [`DECIMATOR_TAPS`] and the Kaiser window below,
/// the transition band is about 0.72 kHz wide: the pass band stays flat to about 3.3 kHz (the
/// telephone band PCMU carries), and everything above about 4.1 kHz — every frequency that would
/// fold back into 0-4 kHz after the 6:1 decimation — is at least 60 dB down.
const DECIMATOR_CUTOFF_HZ: f64 = 3_700.0;

/// The Kaiser window shape parameter for a ~60 dB stop band (`0.1102 * (60 - 8.7)`).
const DECIMATOR_KAISER_BETA: f64 = 5.653;

/// The Kaiser-windowed-sinc low-pass taps for the 6:1 anti-alias decimation, normalised to exactly
/// unity DC gain (so a constant input maps to that constant). Built by the shared [`crate::fir`]
/// machinery the VBAN rate converter uses too.
fn anti_alias_taps() -> Vec<f64> {
    kaiser_lowpass_taps(
        DECIMATOR_TAPS,
        DECIMATOR_CUTOFF_HZ / MIX_RATE_HZ, // cycles per input sample
        DECIMATOR_KAISER_BETA,
    )
}

/// The STATEFUL 48 kHz -> 8 kHz down-sampler for the PCMU leg: a windowed-sinc FIR low-pass
/// (~3.4 kHz pass band, >= 60 dB stop band from ~4.1 kHz) evaluated only at every 6th input sample.
///
/// Its history and its 6:1 phase carry across calls, so feeding a stream in 20 ms chunks (the Janus
/// leg's 960-sample packets) produces exactly the output of one pass over the whole stream — no
/// restart transient every chunk. Replaces the old one-pole + keep-every-6th, which let 4-24 kHz
/// alias back into the speech band (a 6 kHz tone came out only -6 dB) and restarted every chunk.
#[derive(Debug, Clone)]
pub struct Decimator48kTo8k {
    inner: FirDecimator,
}

impl Default for Decimator48kTo8k {
    fn default() -> Self {
        Self::new()
    }
}

impl Decimator48kTo8k {
    /// A decimator starting from silence.
    pub fn new() -> Self {
        Self::primed(0)
    }

    /// A decimator whose history is pre-filled with `x0` — the steady state of a constant `x0`
    /// input, so a DC block maps to that constant from its very first output.
    fn primed(x0: i16) -> Self {
        Decimator48kTo8k {
            inner: FirDecimator::primed(anti_alias_taps(), RESAMPLE_RATIO, x0),
        }
    }

    /// Restart from silence (a new RTP session).
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    /// Feed any number of 48 kHz mono samples; returns the 8 kHz samples completed by them (one per
    /// 6 inputs, counted across calls).
    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        self.inner.process(input)
    }
}

/// A one-pole low-pass smoothing coefficient (`alpha` in `y += alpha*(x-y)`) for the 8 kHz -> 48 kHz
/// UP-sampling side only (smoothing the zero-order hold). The DOWN-sampling side is the FIR
/// [`Decimator48kTo8k`].
const LP_ALPHA_NUM: i64 = 35;
const LP_ALPHA_DEN: i64 = 100;

/// One-pole low-pass, DC-preserving: the state starts AT the first sample so a constant input maps
/// to a constant output from the first sample (no start-up ramp), which is what makes the
/// DC-preservation invariant exact. Returns the filtered stream (same length as the input).
fn one_pole_lowpass(input: &[i16]) -> Vec<i16> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut y: i64 = input[0] as i64 * LP_ALPHA_DEN; // fixed-point state scaled by the denominator
    let mut out = Vec::with_capacity(input.len());
    for &x in input {
        // y += alpha*(x - y): keep y scaled by LP_ALPHA_DEN to avoid per-sample rounding drift.
        let x_scaled = x as i64 * LP_ALPHA_DEN;
        y += LP_ALPHA_NUM * (x_scaled - y) / LP_ALPHA_DEN;
        out.push((y / LP_ALPHA_DEN).clamp(i16::MIN as i64, i16::MAX as i64) as i16);
    }
    out
}

/// Down-sample ONE self-contained 48 kHz mono block to 8 kHz through the FIR anti-alias filter
/// ([`Decimator48kTo8k`], history primed with the block's first sample). Output length is
/// `input.len() / 6` (floor). DC is preserved (a constant input maps to that constant). A STREAM (the
/// Janus leg) must use one long-lived [`Decimator48kTo8k`] instead, so the filter state carries
/// across chunks.
pub fn downsample_48k_to_8k(input: &[i16]) -> Vec<i16> {
    match input.first() {
        Some(&x0) => Decimator48kTo8k::primed(x0).process(input),
        None => Vec::new(),
    }
}

/// Up-sample an 8 kHz mono block to 48 kHz: zero-order hold (each sample repeated 6×) then the same
/// one-pole smoothing. Output length is `input.len() * 6`. DC is preserved.
pub fn upsample_8k_to_48k(input: &[i16]) -> Vec<i16> {
    let mut held = Vec::with_capacity(input.len() * RESAMPLE_RATIO);
    for &s in input {
        for _ in 0..RESAMPLE_RATIO {
            held.push(s);
        }
    }
    one_pole_lowpass(&held)
}

/// Down-mix an interleaved stereo PCM16 block (L,R,L,R,…) to mono by averaging each L/R pair. A
/// trailing odd sample (a malformed block) is DROPPED, so the function never panics.
pub fn stereo_to_mono(interleaved: &[i16]) -> Vec<i16> {
    let mut out = Vec::with_capacity(interleaved.len() / 2);
    let mut i = 0;
    while i + 1 < interleaved.len() {
        out.push(((interleaved[i] as i32 + interleaved[i + 1] as i32) / 2) as i16);
        i += 2;
    }
    out
}

/// Up-mix a mono PCM16 block to interleaved stereo (each sample duplicated to L and R).
pub fn mono_to_stereo(mono: &[i16]) -> Vec<i16> {
    let mut out = Vec::with_capacity(mono.len() * 2);
    for &s in mono {
        out.push(s);
        out.push(s);
    }
    out
}
