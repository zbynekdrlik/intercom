//! Issue 1401: the VBAN legs' jitter buffer is observable. `/api/state` carries each VBAN leg's
//! target, measured depth, servo corrections and stalls; the periodic status line names the leg
//! with the most underruns and overruns (the live line only said `underruns=220`, summed over every
//! participant, and never showed overruns at all) and the program feeds' stalls; and the hub really
//! gives every VBAN participant the target-fill buffer. Since design 5980775411 the block loop runs
//! missed ticks late (`caught_up_ticks`) and gives up only the part beyond four blocks
//! (`lost_ticks`, `lost=N` on the line). Since issue 1461 the facet is the resampler leg's (VBAN
//! legs and the cutters' card capture): the fixed target, the depth, `rate_ppm`,
//! `asrc_phase_frames` and `resyncs`, never a servo or an adaptive-target field; the status line
//! shows the legs' resyncs and one `asrc=` group per leg that has received audio.

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

fn facet(rate_ppm: f64, resyncs: u64, stalls: u64) -> JitterFacet {
    JitterFacet {
        target_frames: 768,
        depth_frames: 790,
        depth_min_frames: 610,
        rate_ppm,
        rate_fit_ppm: Some(rate_ppm),
        asrc_phase_frames: 22.0,
        resyncs,
        stalls,
        primed: true,
    }
}

#[test]
fn a_resampler_leg_reports_its_jitter_facet_and_the_others_do_not() {
    let stats = vec![
        RuntimeStats {
            jitter: Some(facet(12.5, 3, 2)),
            ..Default::default()
        },
        // The daemon fills the facet only from a resampler leg's buffer: none here.
        RuntimeStats::default(),
    ];
    let v = serde_json::to_value(HubState::snapshot(&matrix(), "v", &stats)).unwrap();
    let j = &v["participants"][0]["jitter"];
    assert_eq!(j["target_frames"], 768);
    assert_eq!(j["depth_frames"], 790);
    assert_eq!(j["depth_min_frames"], 610);
    assert_eq!(j["rate_ppm"], 12.5);
    assert_eq!(j["rate_fit_ppm"], 12.5);
    assert_eq!(j["asrc_phase_frames"], 22.0);
    assert_eq!(j["resyncs"], 3);
    assert_eq!(j["stalls"], 2);
    assert_eq!(j["primed"].as_bool(), Some(true));
    // Issue 1461: no single-frame servo and no adaptive target are left to report.
    for gone in [
        "servo_drops",
        "servo_repeats",
        "setpoint_frames",
        "servo_walk_steps",
        "max_gap_ms_10min",
    ] {
        assert!(j.get(gone).is_none(), "{gone}: {j}");
    }
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
fn a_quiet_status_line_names_no_leg_and_no_resync_or_stalls() {
    let stats = vec![RuntimeStats::default(), RuntimeStats::default()];
    let line = HubState::snapshot(&matrix(), "v", &stats).status_line();
    assert!(line.contains("underruns=0 overruns=0"), "got: {line}");
    assert!(!line.contains("servo="), "got: {line}");
    assert!(!line.contains("resyncs="), "got: {line}");
    assert!(!line.contains("asrc="), "got: {line}");
    assert!(!line.contains("stalls="), "got: {line}");
}

#[test]
fn the_status_line_shows_each_live_legs_resampler_and_the_resyncs() {
    // Issue 1461: per leg that has received audio, where its clock sits against the hub's
    // (`rate_ppm`), how far its depth is off the fixed target (`asrc_phase_frames`), its underruns
    // and resyncs; the resyncs also lead as a total naming the worst leg. A leg that never received
    // a packet (fohabl here) shows no group, nor one that is not playing now.
    let stats = vec![
        RuntimeStats {
            rx_packets: 100,
            underruns: 1,
            jitter: Some(facet(12.5, 2, 0)),
            ..Default::default()
        },
        RuntimeStats::default(),
        RuntimeStats {
            jitter: Some(facet(-0.5, 0, 0)),
            ..Default::default()
        },
    ];
    let line = HubState::snapshot(&matrix(), "v", &stats).status_line();
    // A leg that is not playing now (re-priming, muted) shows no group either.
    let mut quiet = stats.clone();
    quiet[0].jitter = quiet[0].jitter.map(|j| JitterFacet { primed: false, ..j });
    let quiet_line = HubState::snapshot(&matrix(), "v", &quiet).status_line();
    assert!(!quiet_line.contains("asrc="), "got: {quiet_line}");
    assert!(
        line.contains(" asrc=cam1:rate_ppm=+12.5:asrc_phase_frames=+22.0:underruns=1:resyncs=2"),
        "got: {line}"
    );
    assert!(line.contains(" resyncs=2(cam1)"), "got: {line}");
    assert!(!line.contains("asrc=fohabl"), "got: {line}");
    assert!(!line.contains("servo="), "got: {line}");
    assert!(
        line.starts_with("intercom-hub: status participants=3 underruns=1(cam1) overruns=0"),
        "the totals still lead: {line}"
    );
}

#[test]
fn the_status_line_shows_program_feed_stalls_but_not_cambox_mutes() {
    // cam1 stalled 5 times (it stops sending on every mute); the FOH feed stalled twice (a real
    // outage). Only the program feed's stalls are a fault worth a line.
    let stats = vec![
        RuntimeStats {
            jitter: Some(facet(0.0, 0, 5)),
            ..Default::default()
        },
        RuntimeStats::default(),
        RuntimeStats {
            jitter: Some(facet(0.0, 0, 2)),
            ..Default::default()
        },
    ];
    let line = HubState::snapshot(&matrix(), "v", &stats).status_line();
    assert!(line.contains("stalls=2(fohabl)"), "got: {line}");
    let mutes_only = vec![RuntimeStats {
        jitter: Some(facet(0.0, 0, 5)),
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

/// Issue 1401, design 6265012279: the hub's memory lock is on `/api/state` (verify-strih item 16
/// FAILs a running hub that reads false); the pure snapshot has no process and omits it.
#[test]
fn the_memory_lock_shows_on_api_state_once_the_daemon_sets_it() {
    let stats = vec![RuntimeStats::default(), RuntimeStats::default()];
    let mut hs = HubState::snapshot(&matrix(), "v", &stats);
    let v = serde_json::to_value(&hs).unwrap();
    assert!(
        v.get("memory_locked").is_none(),
        "the pure snapshot has no process: {v}"
    );
    hs.memory_locked = Some(true);
    let v = serde_json::to_value(&hs).unwrap();
    assert_eq!(v["memory_locked"], true);
    hs.memory_locked = Some(false);
    let v = serde_json::to_value(&hs).unwrap();
    assert_eq!(v["memory_locked"], false, "a refused lock reads false: {v}");
}

#[test]
fn the_facet_takes_the_controllers_numbers() {
    let f = JitterFacet::from(NetworkFillStats {
        target_frames: 1792,
        depth_frames: 1800,
        depth_min_frames: 1500,
        rate_ppm: 0.4,
        rate_fit_ppm: None,
        asrc_phase_frames: 8.0,
        resyncs: 1,
        stalls: 2,
        primed: true,
    });
    assert_eq!(
        (f.target_frames, f.depth_frames, f.depth_min_frames),
        (1792, 1800, 1500)
    );
    assert_eq!((f.rate_ppm, f.asrc_phase_frames), (0.4, 8.0));
    assert_eq!(f.rate_fit_ppm, None, "no fit yet");
    assert_eq!((f.resyncs, f.stalls, f.primed), (1, 2, true));
}

#[test]
fn the_deployed_matrix_gives_every_crossing_the_resampler_leg() {
    // The checked-in strih-snv routing, through the real loader: every VBAN participant (the
    // camboxes and the program feeds) gets the VBAN resampler leg and the cutters' card capture the
    // same leg (issue 1461); the Janus phones keep the local-capture ring; the program sink and
    // the `none` slots the plain one.
    let m = Matrix::from_toml(include_str!("../../intercom.strih-snv.toml")).unwrap();
    let buffers = input_buffers(&m);
    assert_eq!(buffers.len(), m.participants.len());
    // The local captures: a pipewire capture, or since issue 1438 a card leg's arecord.
    let local_inputs: Vec<usize> = m
        .local_inputs()
        .into_iter()
        .map(|(id, _, _)| id)
        .chain(
            m.alsa_cards()
                .into_iter()
                .filter(|c| c.in_channels > 0)
                .map(|c| c.participant),
        )
        .collect();
    assert!(
        !local_inputs.is_empty(),
        "the cutters' talkback is a local capture"
    );
    let mut vban_legs = 0;
    for (id, (p, b)) in m.participants.iter().zip(&buffers).enumerate() {
        let want = if p.adapter == ADAPTER_VBAN {
            vban_legs += 1;
            BufferKind::VbanLeg
        } else if p.adapter == ADAPTER_JANUS {
            BufferKind::LocalCapture
        } else if local_inputs.contains(&id) {
            BufferKind::CardCapture
        } else {
            BufferKind::Plain
        };
        assert_eq!(b.kind(), want, "{} ({}, {})", p.name, p.role, p.adapter);
        assert_eq!(
            b.network_stats().is_some(),
            want == BufferKind::VbanLeg || want == BufferKind::CardCapture
        );
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
        "the block loop publishes each resampler leg's fill numbers to /api/state"
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
