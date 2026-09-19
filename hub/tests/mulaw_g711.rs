//! G.711 µ-law codec + 48 kHz↔8 kHz resample tests (issue 1345 M3a).
//!
//! The PCMU Janus leg's audio path is pure + std-only, so it is pinned here against the ITU-T G.711
//! reference vectors and the resample invariants. Locally verified RED→GREEN with a rustc `--test`
//! replica (Tier-0 #557 bans a local cargo compile); this is the in-tree CI form.

use intercom_hub::mulaw::{
    downsample_48k_to_8k, mono_to_stereo, stereo_to_mono, ulaw_decode, ulaw_decode_block,
    ulaw_encode, ulaw_encode_block, upsample_8k_to_48k,
};

#[test]
fn ulaw_encode_reference_vectors() {
    // The canonical G.711 vectors: silence encodes to 0xFF, +full-scale to 0x80, -full-scale to 0x00.
    assert_eq!(ulaw_encode(0), 0xFF);
    assert_eq!(ulaw_encode(32124), 0x80);
    assert_eq!(ulaw_encode(-32124), 0x00);
}

#[test]
fn ulaw_decode_reference_vectors() {
    assert_eq!(ulaw_decode(0xFF), 0);
    assert_eq!(ulaw_decode(0x80), 32124);
    assert_eq!(ulaw_decode(0x00), -32124);
}

#[test]
fn ulaw_codec_is_stable_on_every_decoded_level() {
    // µ-law has 255 distinct levels (0x7F and 0xFF both decode to 0), so encode(decode(u)) is NOT the
    // identity at the negative-zero code 0x7F. The honest stability property: re-encoding then
    // re-decoding a decoded level reproduces that level.
    for u in 0u16..=255 {
        let code = u as u8;
        let level = ulaw_decode(code);
        assert_eq!(
            ulaw_decode(ulaw_encode(level)),
            level,
            "codec must be stable on level {level} (code {code})"
        );
    }
    // The one alias, made explicit.
    assert_eq!(ulaw_decode(0x7F), 0);
    assert_eq!(ulaw_decode(0xFF), 0);
    assert_eq!(ulaw_encode(0), 0xFF);
}

#[test]
fn ulaw_encode_is_monotonic_and_within_quantisation_step() {
    let mut prev = i32::MIN;
    for x in (-32768i32..=32767).step_by(37) {
        let rt = ulaw_decode(ulaw_encode(x as i16)) as i32;
        assert!(
            rt >= prev,
            "decode(encode(x)) must be non-decreasing at x={x}"
        );
        prev = rt;
        let err = (rt - x).abs();
        assert!(
            err <= (x.abs() >> 4) + 512,
            "x={x} err={err} exceeds the local µ-law quantisation step"
        );
    }
}

#[test]
fn ulaw_block_roundtrip_matches_sample_ops() {
    let pcm: Vec<i16> = (-4000..4000).step_by(211).collect();
    let bytes = ulaw_encode_block(&pcm);
    assert_eq!(bytes.len(), pcm.len());
    let back = ulaw_decode_block(&bytes);
    assert_eq!(back.len(), pcm.len());
    for (i, &b) in bytes.iter().enumerate() {
        assert_eq!(b, ulaw_encode(pcm[i]));
        assert_eq!(back[i], ulaw_decode(b));
    }
}

#[test]
fn downsample_length_and_dc_preservation() {
    let input: Vec<i16> = vec![1000; 48];
    let out = downsample_48k_to_8k(&input);
    assert_eq!(out.len(), 8, "48 samples in -> 8 out (6:1 floor)");
    assert!(
        out.iter().all(|&s| s == 1000),
        "a DC input maps to that constant"
    );
    assert_eq!(
        downsample_48k_to_8k(&vec![0i16; 47]).len(),
        7,
        "floor: 47 -> 7"
    );
    assert!(downsample_48k_to_8k(&[]).is_empty());
}

#[test]
fn upsample_length_and_dc_preservation() {
    let input: Vec<i16> = vec![-500; 8];
    let out = upsample_8k_to_48k(&input);
    assert_eq!(out.len(), 48, "8 samples in -> 48 out (1:6)");
    assert!(
        out.iter().all(|&s| s == -500),
        "a DC input maps to that constant"
    );
    assert!(upsample_8k_to_48k(&[]).is_empty());
}

#[test]
fn resample_roundtrip_preserves_dc_and_length() {
    // 960 samples (20 ms @ 48 kHz) of DC down to 160 @ 8 kHz and back to 960 @ 48 kHz.
    let block: Vec<i16> = vec![777; 960];
    let down = downsample_48k_to_8k(&block);
    assert_eq!(down.len(), 160);
    let up = upsample_8k_to_48k(&down);
    assert_eq!(up.len(), 960);
    assert!(
        up.iter().all(|&s| s == 777),
        "DC survives the down/up round-trip"
    );
}

#[test]
fn stereo_mono_conversions() {
    // Down-mix averages L/R; up-mix duplicates.
    let interleaved = [100i16, 300, -50, -150];
    assert_eq!(stereo_to_mono(&interleaved), vec![200, -100]);
    assert_eq!(mono_to_stereo(&[7, -8]), vec![7, 7, -8, -8]);
    // A trailing odd sample is dropped from the down-mix (a malformed block never panics).
    assert_eq!(stereo_to_mono(&[10, 20, 99]), vec![15]);
}
