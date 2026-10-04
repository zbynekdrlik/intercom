//! Issue 1401: the VBAN legs' jitter buffer is observable. `/api/state` carries each VBAN leg's
//! target, measured depth and servo corrections; the periodic status line names the leg with the
//! most underruns and overruns (the live line only said `underruns=220`, summed over every
//! participant, and never showed overruns at all); and the hub really gives every VBAN participant
//! the target-fill buffer.

use std::path::PathBuf;

use intercom_hub::matrix::Matrix;
use intercom_hub::state::{HubState, JitterFacet, RuntimeStats};

fn matrix() -> Matrix {
    Matrix::from_toml(
        r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2

[[participant]]
name = "cutters"
role = "cutters"
adapter = "none"
in_channels = 2
out_channels = 4
"#,
    )
    .unwrap()
}

fn facet(servo_drops: u64, servo_repeats: u64) -> JitterFacet {
    JitterFacet {
        target_frames: 768,
        depth_frames: 790,
        depth_min_frames: 610,
        servo_drops,
        servo_repeats,
        primed: true,
    }
}

#[test]
fn a_vban_leg_reports_its_jitter_facet_and_the_others_do_not() {
    let stats = vec![
        RuntimeStats {
            jitter: Some(facet(12, 3)),
            ..Default::default()
        },
        // Even if a facet were attached to a non-VBAN participant, it is not rendered.
        RuntimeStats {
            jitter: Some(facet(1, 1)),
            ..Default::default()
        },
    ];
    let v = serde_json::to_value(HubState::snapshot(&matrix(), "v", &stats)).unwrap();
    let j = &v["participants"][0]["jitter"];
    assert_eq!(j["target_frames"], 768);
    assert_eq!(j["depth_frames"], 790);
    assert_eq!(j["depth_min_frames"], 610);
    assert_eq!(j["servo_drops"], 12);
    assert_eq!(j["servo_repeats"], 3);
    assert_eq!(j["primed"].as_bool(), Some(true));
    assert!(v["participants"][1].get("jitter").is_none(), "{v}");
}

#[test]
fn the_status_line_names_the_worst_leg_for_underruns_and_overruns() {
    let stats = vec![
        RuntimeStats {
            underruns: 3,
            overruns: 2,
            ..Default::default()
        },
        RuntimeStats {
            underruns: 5,
            ..Default::default()
        },
    ];
    let line = HubState::snapshot(&matrix(), "v", &stats).status_line();
    assert!(line.contains("underruns=8(cutters)"), "got: {line}");
    assert!(line.contains("overruns=2(cam1)"), "got: {line}");
}

#[test]
fn a_quiet_status_line_names_no_leg_and_no_servo() {
    let stats = vec![RuntimeStats::default(), RuntimeStats::default()];
    let line = HubState::snapshot(&matrix(), "v", &stats).status_line();
    assert!(line.contains("underruns=0 overruns=0"), "got: {line}");
    assert!(!line.contains("servo="), "got: {line}");
}

#[test]
fn the_status_line_shows_the_servo_corrections_when_there_are_any() {
    let stats = vec![
        RuntimeStats {
            jitter: Some(facet(12, 3)),
            ..Default::default()
        },
        RuntimeStats::default(),
    ];
    let line = HubState::snapshot(&matrix(), "v", &stats).status_line();
    assert!(line.contains("servo=12/3"), "got: {line}");
}

#[test]
fn the_hub_gives_every_vban_leg_the_target_fill_buffer() {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&p).expect("read main.rs");
    let f = src
        .split("fn input_buffers(")
        .nth(1)
        .expect("input_buffers exists");
    let body = &f[..f.find("\n}\n").expect("fn end")];
    assert!(
        body.contains("ADAPTER_VBAN") && body.contains("JitterBuffer::vban_leg("),
        "input_buffers must give every VBAN participant the VBAN-leg buffer"
    );
    assert!(
        body.contains("VBAN_TARGET_BLOCKS"),
        "the VBAN leg's target is the shared vban_jitter constant"
    );
    assert!(
        src.contains(".jitter = b.network_stats()"),
        "the block loop publishes each VBAN leg's fill numbers to /api/state"
    );
}
