//! issue 1345 (24.9.2026 production fix): the PCMU down-conversion's anti-alias filter.
//!
//! The phones hear the strih operator through the Janus PCMU leg: 48 kHz -> 8 kHz, then G.711. The
//! old path was a one-pole low-pass (about -6 dB at 6 kHz) then keep-every-6th, restarted every 20 ms
//! chunk. Everything from 4 to 24 kHz folded back into the telephone band and gave the operator's
//! voice a harsh, garbled timbre. A windowed-sinc FIR low-pass (~3.4 kHz pass band) whose state
//! carries across chunks replaces it. Pinned here: stop-band rejection of a tone that would alias,
//! a flat speech pass band, and bit-exact continuity whatever the chunking.

use intercom_hub::mulaw::{downsample_48k_to_8k, Decimator48kTo8k};

fn tone(freq_hz: f64, amp: f64, n: usize) -> Vec<i16> {
    (0..n)
        .map(|i| (amp * (2.0 * std::f64::consts::PI * freq_hz * i as f64 / 48_000.0).sin()) as i16)
        .collect()
}

fn rms(x: &[i16]) -> f64 {
    (x.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

/// Output-vs-input level in dB of a 1 s tone through the one-shot down-sampler, skipping the
/// filter's start-up outputs.
fn gain_db(freq_hz: f64) -> f64 {
    let x = tone(freq_hz, 16_000.0, 48_000);
    let y = downsample_48k_to_8k(&x);
    assert_eq!(y.len(), 8_000);
    20.0 * (rms(&y[200..]) / rms(&x)).log10()
}

#[test]
fn a_6_khz_tone_is_rejected_at_least_40_db() {
    // 6 kHz folds to 2 kHz at 8 kHz — right in the middle of the speech band.
    let g = gain_db(6_000.0);
    assert!(
        g <= -40.0,
        "6 kHz must come out >= 40 dB down, got {g:.1} dB"
    );
}

#[test]
fn every_tone_that_would_alias_is_rejected_at_least_40_db() {
    for f in [
        4_500.0, 5_000.0, 7_000.0, 9_000.0, 12_000.0, 15_000.0, 20_000.0,
    ] {
        let g = gain_db(f);
        assert!(g <= -40.0, "{f} Hz must be >= 40 dB down, got {g:.1} dB");
    }
}

#[test]
fn the_speech_pass_band_is_flat_within_1_db() {
    for f in [300.0, 1_000.0, 2_000.0, 3_000.0] {
        let g = gain_db(f);
        assert!(
            g.abs() <= 1.0,
            "{f} Hz must pass within 1 dB, got {g:.2} dB"
        );
    }
}

#[test]
fn the_stateful_decimator_is_bit_exact_across_20_ms_chunks() {
    // The Janus leg feeds 960-sample (20 ms) chunks; the filter state must carry across them, so the
    // chunked output equals the one-pass output exactly (no restart transient every 20 ms).
    let a = tone(1_000.0, 9_000.0, 9_600);
    let b = tone(6_000.0, 9_000.0, 9_600);
    let x: Vec<i16> = a.iter().zip(&b).map(|(p, q)| p + q).collect();

    let whole = Decimator48kTo8k::new().process(&x);
    assert_eq!(whole.len(), x.len() / 6);

    for chunk in [960usize, 250, 7] {
        let mut d = Decimator48kTo8k::new();
        let mut chunked = Vec::new();
        for c in x.chunks(chunk) {
            chunked.extend(d.process(c));
        }
        assert_eq!(
            chunked, whole,
            "chunk size {chunk} must match the one-pass output"
        );
    }
}

#[test]
fn reset_restarts_the_filter_from_silence() {
    let x = tone(1_000.0, 9_000.0, 960);
    let mut d = Decimator48kTo8k::new();
    let first = d.process(&x);
    let _ = d.process(&x);
    d.reset();
    assert_eq!(d.process(&x), first, "reset = a fresh decimator");
}
