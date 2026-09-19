//! G.711 µ-law codec + the 48 kHz ↔ 8 kHz resample step (issue 1345 M3a).
//!
//! The Janus audiobridge's plain-RTP participant leg carries **PCMU** (G.711 µ-law, 8 kHz mono, PT
//! 0). The hub mixes at 48 kHz stereo PCM16, so the `janus_rtp` adapter down-samples the phones'
//! N-1 mix to 8 kHz mono + µ-law-encodes it for the RTP send, and µ-law-decodes + up-samples the
//! received room mix back to 48 kHz for the engine's jitter buffer.
//!
//! Pure, std-only (no crate): the whole thing is the ITU-T G.711 reference table + a one-pole
//! low-pass, so it verifies RED→GREEN with a rustc `--test` replica under Tier-0 (issue 557 bans a
//! local cargo compile). µ-law is telephone-band (≈3.4 kHz) talkback speech — the design's stability
//! trade; an Opus upgrade of this leg is the documented next step if quality is short.

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

/// A one-pole low-pass smoothing coefficient (`alpha` in `y += alpha*(x-y)`). ~0.35 keeps the
/// pass-band well below the 4 kHz PCMU band while passing DC exactly (unity gain at DC). Speech
/// talkback does not need a sharp anti-alias — the design's telephone-band trade.
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

/// Down-sample a 48 kHz mono block to 8 kHz: one-pole low-pass, then keep every 6th sample. Output
/// length is `input.len() / 6` (floor). DC is preserved (a constant input maps to that constant).
pub fn downsample_48k_to_8k(input: &[i16]) -> Vec<i16> {
    let filtered = one_pole_lowpass(input);
    let mut out = Vec::with_capacity(filtered.len() / RESAMPLE_RATIO);
    // Take one output per full group of RESAMPLE_RATIO input samples (indices 5, 11, 17, …).
    for i in 0..(filtered.len() / RESAMPLE_RATIO) {
        out.push(filtered[i * RESAMPLE_RATIO + (RESAMPLE_RATIO - 1)]);
    }
    out
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
