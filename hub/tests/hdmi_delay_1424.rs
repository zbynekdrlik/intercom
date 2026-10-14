//! issue 1424: the vestibule (strih-pp HDMI-1) heard the lv1 program ~170-210 ms BEFORE the picture
//! (measured 14.10.2026: the camera -> strih render p50 117 ms + the HDMI-1 output 171 ms, against
//! the hub's ~75-80 ms audio path). The `alsa_hdmi` leg therefore carries a fixed lip-sync delay,
//! `hdmi_delay_ms`, applied BEFORE the aplay pipe so the pipe servo's depth is untouched.
//! Std-only: the pure delay line.

use intercom_hub::hdmi_leg::SampleDelay;

fn block(v: i16, frames: usize, ch: usize) -> Vec<i16> {
    vec![v; frames * ch]
}

#[test]
fn zero_delay_passes_every_block_unchanged() {
    let mut d = SampleDelay::new(0, 2);
    let b: Vec<i16> = (0..512).map(|i| i as i16).collect();
    assert_eq!(d.process(&b), b);
}

#[test]
fn the_delay_shifts_the_audio_by_exactly_the_configured_frames() {
    // 48 kHz, 180 ms = 8640 frames, 256-frame blocks, stereo
    let frames = 8640;
    let mut d = SampleDelay::new(frames, 2);
    let mut out = Vec::new();
    for n in 0..60 {
        out.extend(d.process(&block(n as i16 + 1, 256, 2)));
    }
    // the first 8640 frames are silence, then block 1 starts
    assert!(out[..frames * 2].iter().all(|&s| s == 0));
    assert_eq!(out[frames * 2], 1);
    assert_eq!(out.len(), 60 * 256 * 2, "one block in, one block out");
    // block k (1-based) begins exactly `frames` frames after its input position
    for k in 1..=20usize {
        let at = (k - 1) * 256 * 2 + frames * 2;
        assert_eq!(out[at], k as i16, "block {k}");
    }
}

#[test]
fn a_reset_refills_the_line_with_silence() {
    let mut d = SampleDelay::new(300, 2);
    for _ in 0..10 {
        d.process(&block(7, 256, 2));
    }
    d.reset();
    let out = d.process(&block(9, 256, 2));
    assert!(
        out.iter().all(|&s| s == 0),
        "after a reset the old audio never plays"
    );
}

#[test]
fn a_block_of_another_size_keeps_the_delay() {
    let mut d = SampleDelay::new(100, 2);
    let a = d.process(&block(1, 30, 2));
    assert!(a.iter().all(|&s| s == 0));
    let b = d.process(&block(2, 256, 2));
    // 100 frames of delay: 70 silent frames, then the 30 frames of block 1, then block 2
    assert!(b[..70 * 2].iter().all(|&s| s == 0));
    assert!(b[70 * 2..100 * 2].iter().all(|&s| s == 1));
    assert!(b[100 * 2..].iter().all(|&s| s == 2));
}
