//! issue 1345 (24.9.2026 production fix): the hub honours the VBAN sample rate.
//!
//! The FOH desk sends `fohabl-strih` at 96 kHz (VBAN rate index 4, 103 frames x 2 ch per packet,
//! about 934 packets/s). The hub used to ignore the header rate and push those samples straight into
//! its 48 kHz jitter ring, so the ring overran on about 60 % of packets and what survived played at
//! the wrong rate: that was the corrupted strih program audio and the corrupted cans. Pinned here:
//!
//! * a 96 kHz and a 192 kHz stream come out as the SAME tone at 48 kHz (correct pitch + level);
//! * a tone that would alias (30 kHz at 96 kHz folds to 18 kHz) is rejected >= 60 dB;
//! * decimating the real 103-frame packets one by one equals one pass over the whole stream;
//! * a 48 kHz stream passes through byte-identical;
//! * any other rate is rejected loudly (counted, never mis-played) and a rate change resets the
//!   filter state;
//! * the decoder carries the header rate and `/api/state` shows it per participant.

use intercom_hub::vban_rate::{
    decimation_factor, decimator, VbanRateConverter, VbanRateStats, SUPPORTED_DECIMATIONS,
};

const HUB_RATE: u32 = 48_000;

fn tone(freq_hz: f64, amp: f64, rate: u32, n: usize) -> Vec<i16> {
    (0..n)
        .map(|i| {
            (amp * (2.0 * std::f64::consts::PI * freq_hz * i as f64 / rate as f64).sin()).round()
                as i16
        })
        .collect()
}

fn rms(x: &[i16]) -> f64 {
    (x.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

/// Least-squares fit of `a*sin + b*cos` at `freq_hz` (sampled at `rate`) over `x`: returns the fitted
/// amplitude and the RMS of what the sinusoid does NOT explain.
fn fit_tone(x: &[i16], freq_hz: f64, rate: u32) -> (f64, f64) {
    let w = 2.0 * std::f64::consts::PI * freq_hz / rate as f64;
    let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (i, &v) in x.iter().enumerate() {
        let (s, c) = (w * i as f64).sin_cos();
        ss += s * s;
        cc += c * c;
        sc += s * c;
        xs += v as f64 * s;
        xc += v as f64 * c;
    }
    let det = ss * cc - sc * sc;
    let a = (xs * cc - xc * sc) / det;
    let b = (xc * ss - xs * sc) / det;
    let resid: f64 = x
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let (s, c) = (w * i as f64).sin_cos();
            let e = v as f64 - (a * s + b * c);
            e * e
        })
        .sum::<f64>()
        / x.len().max(1) as f64;
    ((a * a + b * b).sqrt(), resid.sqrt())
}

/// Feed a 1 s mono tone at `in_rate` through a fresh decimator of `factor`; skip the filter start-up.
fn decimate_1s(freq_hz: f64, in_rate: u32, factor: usize) -> Vec<i16> {
    let x = tone(freq_hz, 16_000.0, in_rate, in_rate as usize);
    let y = decimator(factor).process(&x);
    assert_eq!(
        y.len(),
        in_rate as usize / factor,
        "one output per {factor} inputs"
    );
    y[400..].to_vec()
}

#[test]
fn the_supported_ratios_are_exactly_1_2_4() {
    assert_eq!(SUPPORTED_DECIMATIONS, [1, 2, 4]);
    assert_eq!(decimation_factor(48_000, HUB_RATE), Some(1));
    assert_eq!(decimation_factor(96_000, HUB_RATE), Some(2));
    assert_eq!(decimation_factor(192_000, HUB_RATE), Some(4));
    for other in [44_100, 88_200, 24_000, 32_000, 384_000, 0] {
        assert_eq!(decimation_factor(other, HUB_RATE), None, "{other} Hz");
    }
    assert_eq!(
        decimation_factor(48_000, 0),
        None,
        "a zero hub rate never divides"
    );
}

#[test]
fn a_1_khz_tone_at_96_khz_comes_out_as_the_same_1_khz_tone_at_48_khz() {
    let y = decimate_1s(1_000.0, 96_000, 2);
    let (amp, resid) = fit_tone(&y, 1_000.0, HUB_RATE);
    // Right pitch at the hub rate (the old bug played it an octave off) and the right level.
    assert!(
        (20.0 * (amp / 16_000.0).log10()).abs() <= 0.1,
        "1 kHz must pass within 0.1 dB, got amplitude {amp:.1}"
    );
    assert!(
        20.0 * (resid / amp).log10() <= -60.0,
        "the output must be a clean 1 kHz sine, residual {resid:.2} vs amplitude {amp:.1}"
    );
}

#[test]
fn a_1_khz_tone_at_192_khz_comes_out_as_the_same_1_khz_tone_at_48_khz() {
    let y = decimate_1s(1_000.0, 192_000, 4);
    let (amp, resid) = fit_tone(&y, 1_000.0, HUB_RATE);
    assert!(
        (20.0 * (amp / 16_000.0).log10()).abs() <= 0.1,
        "amp {amp:.1}"
    );
    assert!(20.0 * (resid / amp).log10() <= -60.0, "residual {resid:.2}");
}

#[test]
fn the_audio_pass_band_is_flat_to_20_khz() {
    for f in [100.0, 5_000.0, 15_000.0, 20_000.0] {
        for (rate, factor) in [(96_000, 2), (192_000, 4)] {
            let y = decimate_1s(f, rate, factor);
            let (amp, _) = fit_tone(&y, f, HUB_RATE);
            let g = 20.0 * (amp / 16_000.0).log10();
            assert!(
                g.abs() <= 0.5,
                "{f} Hz at {rate} must pass within 0.5 dB, got {g:.2}"
            );
        }
    }
}

#[test]
fn a_30_khz_tone_at_96_khz_is_rejected_at_least_60_db() {
    // 30 kHz folds to 18 kHz at 48 kHz — squarely audible if the filter lets it through.
    let x = tone(30_000.0, 16_000.0, 96_000, 96_000);
    let y = decimate_1s(30_000.0, 96_000, 2);
    let g = 20.0 * (rms(&y) / rms(&x)).log10();
    assert!(
        g <= -60.0,
        "30 kHz must come out >= 60 dB down, got {g:.1} dB"
    );
}

#[test]
fn every_tone_that_would_alias_is_rejected_at_least_60_db() {
    for (rate, factor, freqs) in [
        (96_000, 2, vec![25_000.0, 30_000.0, 40_000.0, 47_000.0]),
        (
            192_000,
            4,
            vec![25_000.0, 30_000.0, 50_000.0, 70_000.0, 95_000.0],
        ),
    ] {
        for f in freqs {
            let x = tone(f, 16_000.0, rate, rate as usize);
            let y = decimate_1s(f, rate, factor);
            let g = 20.0 * (rms(&y) / rms(&x)).log10();
            assert!(
                g <= -60.0,
                "{f} Hz at {rate} must be >= 60 dB down, got {g:.1} dB"
            );
        }
    }
}

#[test]
fn decimating_103_frame_packets_equals_one_pass_over_the_whole_stream() {
    // The live fohabl-strih shape: 96 kHz, 103 frames x 2 channels per packet. 103 is odd, so the
    // 2:1 phase straddles every packet boundary — the filter history AND the phase must carry.
    let left = tone(1_000.0, 12_000.0, 96_000, 103 * 200);
    let right = tone(7_000.0, 9_000.0, 96_000, 103 * 200);
    let whole_l = decimator(2).process(&left);
    let whole_r = decimator(2).process(&right);

    let mut conv = VbanRateConverter::new(HUB_RATE);
    let (mut got_l, mut got_r) = (Vec::new(), Vec::new());
    for (cl, cr) in left.chunks(103).zip(right.chunks(103)) {
        let step = conv.process(96_000, vec![cl.to_vec(), cr.to_vec()]);
        let out = step.channels.expect("96 kHz is supported");
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].len(),
            out[1].len(),
            "both channels decimate in lock-step"
        );
        got_l.extend_from_slice(&out[0]);
        got_r.extend_from_slice(&out[1]);
    }
    assert_eq!(
        got_l, whole_l,
        "packetised left must equal the one-shot pass bit for bit"
    );
    assert_eq!(
        got_r, whole_r,
        "packetised right must equal the one-shot pass bit for bit"
    );
    assert_eq!(got_l.len(), 103 * 200 / 2);
    assert_eq!(conv.sample_rate(), Some(96_000));
    assert_eq!(conv.rate_rejects(), 0);
}

#[test]
fn a_48_khz_stream_passes_through_byte_identical() {
    let mut conv = VbanRateConverter::new(HUB_RATE);
    let a: Vec<i16> = (0..256).map(|i| (i * 97 - 12_000) as i16).collect();
    let b: Vec<i16> = (0..256).map(|i| (i * -53 + 7_000) as i16).collect();
    let first = conv.process(48_000, vec![a.clone(), b.clone()]);
    assert!(
        first.rate_changed,
        "the first packet reports the stream's rate once"
    );
    assert_eq!(first.channels, Some(vec![a.clone(), b.clone()]));
    let second = conv.process(48_000, vec![b.clone(), a.clone()]);
    assert!(!second.rate_changed, "a steady rate is not a transition");
    assert_eq!(second.channels, Some(vec![b, a]));
    assert_eq!(conv.sample_rate(), Some(48_000));
    assert_eq!(conv.rate_rejects(), 0);
}

#[test]
fn an_unsupported_rate_is_rejected_counted_and_reported_once() {
    let mut conv = VbanRateConverter::new(HUB_RATE);
    let pkt = vec![vec![1000i16; 256]; 2];
    let first = conv.process(44_100, pkt.clone());
    assert!(
        first.channels.is_none(),
        "44.1 kHz must never be played at 48 kHz"
    );
    assert!(
        first.rate_changed,
        "the first reject is the one transition the caller warns on"
    );
    for _ in 0..9 {
        let step = conv.process(44_100, pkt.clone());
        assert!(step.channels.is_none());
        assert!(
            !step.rate_changed,
            "no warn per packet — only per transition"
        );
    }
    assert_eq!(conv.rate_rejects(), 10, "every rejected packet is counted");
    assert_eq!(
        conv.sample_rate(),
        Some(44_100),
        "the offending rate stays visible"
    );

    // The source switches to a supported rate: audio flows again, one transition, count kept.
    let back = conv.process(48_000, pkt.clone());
    assert!(back.rate_changed);
    assert_eq!(back.channels, Some(pkt));
    assert_eq!(conv.rate_rejects(), 10);
}

#[test]
fn a_rate_change_resets_the_filter_state() {
    let mut conv = VbanRateConverter::new(HUB_RATE);
    // Leave history + an odd phase in the 2:1 filter.
    for c in tone(3_000.0, 15_000.0, 96_000, 103 * 7).chunks(103) {
        let _ = conv.process(96_000, vec![c.to_vec()]);
    }
    // Switch to 192 kHz: the output must be exactly a FRESH 4:1 filter's.
    let x = tone(2_000.0, 11_000.0, 192_000, 1_000);
    let step = conv.process(192_000, vec![x.clone()]);
    assert!(step.rate_changed);
    assert_eq!(step.channels, Some(vec![decimator(4).process(&x)]));
    // And back to 96 kHz: a fresh 2:1 filter again, not the old history.
    let z = tone(500.0, 8_000.0, 96_000, 103);
    let step = conv.process(96_000, vec![z.clone()]);
    assert!(step.rate_changed);
    assert_eq!(step.channels, Some(vec![decimator(2).process(&z)]));
}

#[test]
fn a_channel_count_change_restarts_every_channel_from_silence() {
    let mut conv = VbanRateConverter::new(HUB_RATE);
    let mono = tone(1_000.0, 10_000.0, 96_000, 103);
    let _ = conv.process(96_000, vec![mono.clone()]);
    let stereo = vec![mono.clone(), mono.clone()];
    let step = conv.process(96_000, stereo);
    let fresh = decimator(2).process(&mono);
    assert_eq!(step.channels, Some(vec![fresh.clone(), fresh]));
}

#[test]
fn the_shared_stats_slot_publishes_rate_and_rejects() {
    let stats = VbanRateStats::default();
    assert_eq!(stats.sample_rate(), None, "no packet yet = no rate");
    assert_eq!(stats.rate_rejects(), 0);
    let mut conv = VbanRateConverter::new(HUB_RATE);
    let _ = conv.process(88_200, vec![vec![0i16; 64]]);
    let _ = conv.process(88_200, vec![vec![0i16; 64]]);
    stats.publish(&conv);
    assert_eq!(stats.sample_rate(), Some(88_200));
    assert_eq!(stats.rate_rejects(), 2);
    let _ = conv.process(96_000, vec![vec![0i16; 64]]);
    stats.publish(&conv);
    assert_eq!(stats.sample_rate(), Some(96_000));
    assert_eq!(stats.rate_rejects(), 2);
}

// ---- wiring (needs the whole crate: the VBAN codec + the /api/state snapshot) ----

mod wiring {
    use intercom_hub::matrix::Matrix;
    use intercom_hub::state::{HubState, RuntimeStats};
    use intercom_hub::vban_io::{decode_packet, encode_packet, route_packet, OutBlock};
    use std::collections::HashMap;

    fn packet(name: &str, rate: u32, frames: usize) -> Vec<u8> {
        let interleaved: Vec<i16> = (0..frames * 2).map(|i| i as i16).collect();
        encode_packet(&OutBlock {
            stream_name: name,
            sample_rate: rate,
            channels: 2,
            frame_counter: 0,
            interleaved: &interleaved,
            frames,
        })
        .unwrap()
    }

    #[test]
    fn decode_packet_carries_the_header_rate() {
        let (audio, rate) = decode_packet(&packet("fohabl-strih", 96_000, 103)).unwrap();
        assert_eq!(rate, 96_000);
        assert_eq!(audio.frames, 103);
        assert_eq!(audio.channels.len(), 2);
        let (_, rate) = decode_packet(&packet("cam1", 48_000, 256)).unwrap();
        assert_eq!(rate, 48_000);
    }

    #[test]
    fn route_packet_carries_the_header_rate() {
        let mut known = HashMap::new();
        known.insert("fohabl-strih".to_string(), 7usize);
        let (id, audio, rate) = route_packet(&known, &packet("fohabl-strih", 192_000, 64)).unwrap();
        assert_eq!((id, rate, audio.frames), (7, 192_000, 64));
    }

    #[test]
    fn api_state_shows_each_vban_input_sample_rate_and_rejects() {
        let m = Matrix::from_toml(
            r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "fohabl"
role = "program_ref"
adapter = "vban"
host = "10.77.7.30"
in_stream = "fohabl-strih"
in_channels = 2
out_channels = 0

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2
"#,
        )
        .unwrap();
        let stats = vec![
            RuntimeStats {
                sample_rate: Some(96_000),
                rate_rejects: 3,
                ..Default::default()
            },
            RuntimeStats::default(),
        ];
        let v = serde_json::to_value(HubState::snapshot(&m, "v", &stats)).unwrap();
        assert_eq!(v["participants"][0]["sample_rate"], 96_000);
        assert_eq!(v["participants"][0]["rate_rejects"], 3);
        // A stream that has sent nothing yet has no rate to show.
        assert!(v["participants"][1].get("sample_rate").is_none());
        assert_eq!(v["participants"][1]["rate_rejects"], 0);
    }
}
