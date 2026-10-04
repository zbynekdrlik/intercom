//! Issue 1401: the VBAN legs' jitter buffer is observable. `/api/state` carries each VBAN leg's
//! target, measured depth, servo corrections and stalls; the periodic status line names the leg
//! with the most underruns and overruns (the live line only said `underruns=220`, summed over every
//! participant, and never showed overruns at all) and the program feeds' stalls; and the hub really
//! gives every VBAN participant the target-fill buffer. Since design 5980775411 the block loop runs
//! missed ticks late (`caught_up_ticks`) and gives up only the part beyond four blocks
//! (`lost_ticks`, `lost=N` on the line); a program feed's facet also shows the largest gap of the
//! last 10 min its adaptive target follows.

use std::path::PathBuf;

use intercom_hub::inputs::input_buffers;
use intercom_hub::matrix::{Matrix, ADAPTER_JANUS, ADAPTER_VBAN, PROGRAM_OUT_ROLE};
use intercom_hub::state::{HubState, JitterFacet, RuntimeStats};
use intercom_hub::vban_io::BufferKind;
use intercom_hub::vban_jitter::NetworkFillStats;

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

[[participant]]
name = "fohabl"
role = "program_ref"
adapter = "vban"
host = "10.77.7.30"
in_stream = "fohabl-strih"
in_channels = 2
out_channels = 0
"#,
    )
    .unwrap()
}

fn facet(servo_drops: u64, servo_repeats: u64, stalls: u64) -> JitterFacet {
    JitterFacet {
        target_frames: 768,
        depth_frames: 790,
        depth_min_frames: 610,
        servo_drops,
        servo_repeats,
        stalls,
        primed: true,
        max_gap_ms_10min: None,
    }
}

#[test]
fn a_vban_leg_reports_its_jitter_facet_and_the_others_do_not() {
    let stats = vec![
        RuntimeStats {
            jitter: Some(facet(12, 3, 2)),
            ..Default::default()
        },
        // Even if a facet were attached to a non-VBAN participant, it is not rendered.
        RuntimeStats {
            jitter: Some(facet(1, 1, 1)),
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
    assert_eq!(j["stalls"], 2);
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
fn a_quiet_status_line_names_no_leg_and_no_servo_or_stalls() {
    let stats = vec![RuntimeStats::default(), RuntimeStats::default()];
    let line = HubState::snapshot(&matrix(), "v", &stats).status_line();
    assert!(line.contains("underruns=0 overruns=0"), "got: {line}");
    assert!(!line.contains("servo="), "got: {line}");
    assert!(!line.contains("stalls="), "got: {line}");
}

#[test]
fn the_status_line_shows_the_servo_corrections_when_there_are_any() {
    let stats = vec![
        RuntimeStats {
            jitter: Some(facet(12, 3, 0)),
            ..Default::default()
        },
        RuntimeStats::default(),
    ];
    let line = HubState::snapshot(&matrix(), "v", &stats).status_line();
    assert!(line.contains("servo=12/3"), "got: {line}");
}

#[test]
fn the_status_line_shows_program_feed_stalls_but_not_cambox_mutes() {
    // cam1 stalled 5 times (it stops sending on every mute); the FOH feed stalled twice (a real
    // outage). Only the program feed's stalls are a fault worth a line.
    let stats = vec![
        RuntimeStats {
            jitter: Some(facet(0, 0, 5)),
            ..Default::default()
        },
        RuntimeStats::default(),
        RuntimeStats {
            jitter: Some(facet(0, 0, 2)),
            ..Default::default()
        },
    ];
    let line = HubState::snapshot(&matrix(), "v", &stats).status_line();
    assert!(line.contains("stalls=2(fohabl)"), "got: {line}");
    let mutes_only = vec![RuntimeStats {
        jitter: Some(facet(0, 0, 5)),
        ..Default::default()
    }];
    let line = HubState::snapshot(&matrix(), "v", &mutes_only).status_line();
    assert!(!line.contains("stalls="), "got: {line}");
}

#[test]
fn lost_and_caught_up_hub_ticks_show_on_api_state_and_only_the_lost_on_the_status_line() {
    // A tick run late (caught up) is no loss; a LOST tick is a block lost on EVERY output, the
    // program sink included. The daemon counts both for the whole run; the line names only the
    // loss, after the leading `underruns=`.
    let stats = vec![RuntimeStats::default(), RuntimeStats::default()];
    let mut hs = HubState::snapshot(&matrix(), "v", &stats);
    assert_eq!((hs.caught_up_ticks, hs.lost_ticks), (0, 0));
    hs.caught_up_ticks = 12;
    let line = hs.status_line();
    assert!(
        !line.contains("lost="),
        "a caught-up tick is no loss: {line}"
    );
    assert!(!line.contains("missed="), "{line}");
    hs.lost_ticks = 3;
    let line = hs.status_line();
    assert!(line.contains(" lost=3"), "got: {line}");
    assert!(
        line.starts_with("intercom-hub: status participants=3 underruns=0 overruns=0"),
        "got: {line}"
    );
    let v = serde_json::to_value(&hs).unwrap();
    assert_eq!(v["caught_up_ticks"], 12);
    assert_eq!(v["lost_ticks"], 3);
    assert!(v.get("missed_ticks").is_none(), "{v}");
}

#[test]
fn the_mix_threads_class_shows_on_api_state_once_the_daemon_sets_it() {
    let stats = vec![RuntimeStats::default(), RuntimeStats::default()];
    let mut hs = HubState::snapshot(&matrix(), "v", &stats);
    let v = serde_json::to_value(&hs).unwrap();
    assert!(
        v.get("mix_thread_sched").is_none(),
        "the pure snapshot has no thread: {v}"
    );
    hs.mix_thread_sched = Some("SCHED_FIFO 10".into());
    let v = serde_json::to_value(&hs).unwrap();
    assert_eq!(v["mix_thread_sched"], "SCHED_FIFO 10");
}

#[test]
fn a_program_feeds_facet_shows_its_largest_gap_and_a_camboxes_does_not() {
    let program = JitterFacet::from(NetworkFillStats {
        target_frames: 1792,
        max_gap_us_10min: Some(27_641),
        ..Default::default()
    });
    assert_eq!(program.max_gap_ms_10min, Some(27.6), "0.1 ms resolution");
    assert_eq!(program.target_frames, 1792, "the live target");
    let cambox = JitterFacet::from(NetworkFillStats {
        target_frames: 768,
        ..Default::default()
    });
    assert_eq!(cambox.max_gap_ms_10min, None);
    let stats = vec![
        RuntimeStats {
            jitter: Some(cambox),
            ..Default::default()
        },
        RuntimeStats::default(),
        RuntimeStats {
            jitter: Some(program),
            ..Default::default()
        },
    ];
    let v = serde_json::to_value(HubState::snapshot(&matrix(), "v", &stats)).unwrap();
    assert!(
        v["participants"][0]["jitter"]
            .get("max_gap_ms_10min")
            .is_none(),
        "{v}"
    );
    assert_eq!(v["participants"][2]["jitter"]["max_gap_ms_10min"], 27.6);
    assert_eq!(v["participants"][2]["jitter"]["target_frames"], 1792);
}

#[test]
fn the_deployed_matrix_gives_every_vban_leg_the_vban_buffer() {
    // The checked-in strih-lx routing, through the real loader: every VBAN participant (the
    // camboxes and the program feeds) gets the VBAN-leg buffer; the Janus phones and the local
    // capture keep the local-capture ring; the program sink and the `none` slots the plain one.
    let m = Matrix::from_toml(include_str!("../../intercom.strih-lx.toml")).unwrap();
    let buffers = input_buffers(&m);
    assert_eq!(buffers.len(), m.participants.len());
    let local_inputs: Vec<usize> = m.local_inputs().into_iter().map(|(id, _, _)| id).collect();
    let mut vban_legs = 0;
    for (id, (p, b)) in m.participants.iter().zip(&buffers).enumerate() {
        let want = if p.adapter == ADAPTER_VBAN {
            vban_legs += 1;
            BufferKind::VbanLeg
        } else if p.adapter == ADAPTER_JANUS || local_inputs.contains(&id) {
            BufferKind::LocalCapture
        } else {
            BufferKind::Plain
        };
        assert_eq!(b.kind(), want, "{} ({}, {})", p.name, p.role, p.adapter);
        assert_eq!(b.network_stats().is_some(), want == BufferKind::VbanLeg);
        if p.role == PROGRAM_OUT_ROLE {
            assert_eq!(
                b.kind(),
                BufferKind::Plain,
                "the program sink has no ingress"
            );
        }
    }
    assert!(vban_legs >= 8, "the camboxes + the FOH feed: {vban_legs}");
}

#[test]
fn the_block_loop_catches_up_gives_up_only_the_lost_part_and_publishes_the_fill_numbers() {
    // The daemon's block loop is not unit-testable; anchor the calls that wire the pure pieces
    // (BlockGrid / run_batch / skip_missed / network_stats, all tested on their own) into it.
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&p).expect("read main.rs");
    let batch = src
        .find("run_batch(batch, |lost|")
        .expect("every due tick runs through the catch-up dispatch");
    let skip = src
        .find("b.skip_missed(lost, block_frames)")
        .expect("only the lost part is given up");
    let pop = src.find("b.pop_block(block_frames)").expect("pop");
    assert!(
        batch < skip && skip < pop,
        "inside each cycle, the lost blocks go before the pop"
    );
    assert!(
        !src.contains("missed_ticks("),
        "the grid counts the due ticks itself"
    );
    assert!(
        src.contains(".jitter = b.network_stats()"),
        "the block loop publishes each VBAN leg's fill numbers to /api/state"
    );
    assert!(
        src.contains("snapshot.caught_up_ticks = caught_up_total"),
        "the block loop publishes the run's caught-up ticks"
    );
    assert!(
        src.contains("snapshot.lost_ticks = lost_total"),
        "the block loop publishes the run's lost ticks"
    );
}
