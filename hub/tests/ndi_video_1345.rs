//! The Interkom MJPEG picture leg (issue 1345 M3c): an NDI low-bandwidth receiver, decimated to
//! ~10 fps, JPEG-encoded, served as `multipart/x-mixed-replace` at `/interkom.mjpeg` (+ a single
//! `/interkom.jpg`). These tests pin the PURE pieces WITHOUT libndi and WITHOUT a running server —
//! the same Tier-0 doctrine bkshading uses for its preview pipeline (CI is the first compile, but
//! the framing bytes, the stale predicate, the decimator, the BGRX→RGB conversion, the shared slot,
//! and the `[video]` config defaults + validation are all pinned here).

use intercom_hub::http;
use intercom_hub::matrix::Matrix;
use intercom_hub::ndi_video::convert::bgra_to_rgb;
use intercom_hub::ndi_video::decimate::Decimator;
use intercom_hub::ndi_video::VideoState;

// --- 1. The multipart MJPEG part framing (exact bytes) ---------------------------------------

#[test]
fn mjpeg_part_frames_the_jpeg_exactly() {
    // A tiny fake JPEG (8 bytes): SOI + "jpeg" + EOI.
    let jpeg: &[u8] = b"\xff\xd8jpeg\xff\xd9";
    let part = http::mjpeg_part(jpeg);
    let expected =
        b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 8\r\n\r\n\xff\xd8jpeg\xff\xd9\r\n";
    assert_eq!(part, expected.to_vec());
}

#[test]
fn mjpeg_part_content_length_matches_payload() {
    let jpeg = [0u8; 1234];
    let part = http::mjpeg_part(&jpeg);
    let text = String::from_utf8_lossy(&part);
    assert!(text.starts_with("--frame\r\n"), "boundary opens the part");
    assert!(
        text.contains("Content-Type: image/jpeg\r\n"),
        "jpeg content-type"
    );
    assert!(
        text.contains("Content-Length: 1234\r\n"),
        "content-length == payload len"
    );
    // The part ends with the payload followed by a trailing CRLF.
    assert!(part.ends_with(b"\r\n"), "trailing CRLF closes the part");
    // header + payload + trailing CRLF.
    let header = b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 1234\r\n\r\n";
    assert_eq!(part.len(), header.len() + 1234 + 2);
}

// --- 2. The stale predicate (the >5 s stream-ends contract with the PWA) ----------------------

#[test]
fn frame_is_stale_boundaries() {
    // age == max is NOT stale; age > max IS stale.
    assert!(!http::frame_is_stale(1000, 6000, 5000), "age 5000 == max");
    assert!(http::frame_is_stale(1000, 6001, 5000), "age 5001 > max");
    assert!(!http::frame_is_stale(1000, 1000, 5000), "age 0");
    // A backwards clock (now < updated) saturates to age 0 → never stale.
    assert!(!http::frame_is_stale(1000, 500, 5000), "clock went back");
}

// --- 3. The decimator holds output at <= fps on a fake monotonic clock ------------------------

#[test]
fn decimator_holds_target_fps_on_a_fake_monotonic_clock() {
    // 10 fps → a 100 ms minimum interval.
    let mut d = Decimator::new(10.0);
    assert_eq!(d.min_interval_ms(), 100);
    // Feed a frame every 10 ms across a 1000 ms window (t = 0,10,…,1000 = 101 frames).
    let mut emitted = 0usize;
    for t in (0..=1000).step_by(10) {
        if d.should_emit(t) {
            emitted += 1;
        }
    }
    // At 10 fps over 1 s the decimator emits at t=0,100,…,1000 = 11 frames — never more than
    // one per 100 ms window, so a ~60 fps input can never overwhelm the 10 fps picture.
    assert!(emitted <= 11, "decimated to <= 11 in 1 s, got {emitted}");
    assert!(emitted >= 10, "not over-thinning, got {emitted}");
}

// --- 4. BGRX → RGB conversion on a 2×2 fixture ------------------------------------------------

#[test]
fn bgrx_to_rgb_on_a_2x2_fixture() {
    // 2×2 px, 4 bytes each, little-endian B, G, R, X. Distinct colours per pixel.
    // px0 = (R=10,G=20,B=30) -> bytes 30,20,10,255
    // px1 = (R=40,G=50,B=60) -> bytes 60,50,40,255
    // px2 = (R=70,G=80,B=90) -> bytes 90,80,70,0   (BGRX: X ignored)
    // px3 = (R=100,G=110,B=120) -> bytes 120,110,100,255
    let bgrx: Vec<u8> = vec![
        30, 20, 10, 255, //
        60, 50, 40, 255, //
        90, 80, 70, 0, //
        120, 110, 100, 255,
    ];
    let rgb = bgra_to_rgb(&bgrx, 2, 2);
    assert_eq!(
        rgb,
        vec![
            10, 20, 30, //
            40, 50, 60, //
            70, 80, 90, //
            100, 110, 120,
        ]
    );
}

// --- 5. The shared slot publishes only on a NEW frame -----------------------------------------

#[test]
fn video_slot_publishes_only_on_a_new_frame() {
    let state = VideoState::new("STRIH-LX (interkom)");
    // Nothing published yet.
    assert_eq!(state.frame_counter(), 0);
    assert!(state.latest_frame().is_none());
    assert!(state.latest().is_none());

    // First frame → counter advances to 1, the frame is readable.
    state.publish(vec![1, 2, 3], 100);
    assert_eq!(state.frame_counter(), 1);
    assert_eq!(state.latest_frame().as_deref(), Some(&[1u8, 2, 3].to_vec()));
    // Reading again WITHOUT a publish does not advance the counter (only a new frame does).
    let (_f1, c1) = state.latest().unwrap();
    let (_f2, c2) = state.latest().unwrap();
    assert_eq!(c1, 1);
    assert_eq!(c2, 1);

    // A second publish advances the counter and swaps the frame.
    state.publish(vec![9, 9], 200);
    assert_eq!(state.frame_counter(), 2);
    assert_eq!(state.latest_frame().as_deref(), Some(&[9u8, 9].to_vec()));
}

#[test]
fn video_state_snapshot_reports_the_facet() {
    let state = VideoState::new("CAM1 (usb)");
    // Not connected, no frame yet.
    let s0 = state.snapshot(1000);
    assert_eq!(s0.source, "CAM1 (usb)");
    assert!(!s0.connected);
    assert_eq!(s0.frames, 0);
    assert!(s0.last_frame_age_ms.is_none());
    assert!(s0.last_error.is_none());

    state.set_connected(true);
    state.publish(vec![0u8; 10], 1000);
    state.record_fps(9.8);
    let s1 = state.snapshot(1500);
    assert!(s1.connected);
    assert_eq!(s1.frames, 1);
    assert_eq!(s1.last_frame_age_ms, Some(500));
    assert!((s1.fps_actual - 9.8).abs() < 1e-3);

    state.set_connected(false);
    state.set_error(Some(
        "NDI source 'CAM1 (usb)' not found within 5s".to_string(),
    ));
    let s2 = state.snapshot(2000);
    assert!(!s2.connected);
    assert_eq!(
        s2.last_error.as_deref(),
        Some("NDI source 'CAM1 (usb)' not found within 5s")
    );
}

// --- 6. The `[video]` config table: defaults + validation via Matrix::from_toml ----------------

fn toml_with_video(extra: &str) -> String {
    format!(
        r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256
{extra}

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2
"#
    )
}

#[test]
fn video_table_defaults_when_empty() {
    let m = Matrix::from_toml(&toml_with_video("\n[video]")).unwrap();
    let v = m.video.expect("[video] present");
    assert_eq!(v.ndi_source_name, "STRIH-LX (interkom)");
    assert_eq!(v.fps, 10);
    assert_eq!(v.jpeg_quality, 70);
    assert!(v.enabled);
}

#[test]
fn video_table_absent_is_none() {
    let m = Matrix::from_toml(&toml_with_video("")).unwrap();
    assert!(m.video.is_none());
}

#[test]
fn video_table_explicit_values_parse() {
    let extra = r#"
[video]
ndi_source_name = "CAM1 (usb)"
fps = 12
jpeg_quality = 55
enabled = false
"#;
    let m = Matrix::from_toml(&toml_with_video(extra)).unwrap();
    let v = m.video.expect("[video] present");
    assert_eq!(v.ndi_source_name, "CAM1 (usb)");
    assert_eq!(v.fps, 12);
    assert_eq!(v.jpeg_quality, 55);
    assert!(!v.enabled);
}

#[test]
fn video_fps_out_of_range_is_refused() {
    let low = Matrix::from_toml(&toml_with_video("\n[video]\nfps = 0"))
        .unwrap_err()
        .to_string();
    assert!(low.contains("fps"), "got: {low}");
    let high = Matrix::from_toml(&toml_with_video("\n[video]\nfps = 31"))
        .unwrap_err()
        .to_string();
    assert!(high.contains("fps"), "got: {high}");
}

#[test]
fn video_jpeg_quality_out_of_range_is_refused() {
    let low = Matrix::from_toml(&toml_with_video("\n[video]\njpeg_quality = 29"))
        .unwrap_err()
        .to_string();
    assert!(low.contains("jpeg_quality"), "got: {low}");
    let high = Matrix::from_toml(&toml_with_video("\n[video]\njpeg_quality = 96"))
        .unwrap_err()
        .to_string();
    assert!(high.contains("jpeg_quality"), "got: {high}");
}
