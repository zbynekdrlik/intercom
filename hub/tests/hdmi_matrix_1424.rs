//! issue 1424 part 3: the `alsa_hdmi` adapter in the routing matrix, and the leg on `/api/state` and
//! the status line. CI-only (serde + toml); the pure half is `hdmi_audio_1424.rs`.

use intercom_hub::drm_output_audio_match::{MATCH, NO_MONITOR};
use intercom_hub::hdmi_audio::{HdmiLegStats, HdmiResolution};
use intercom_hub::local_audio::LocalAudioFacet;
use intercom_hub::matrix::{Matrix, ADAPTER_ALSA_HDMI};
use intercom_hub::state::{HubState, RuntimeStats};

const HEAD: &str = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "lv1"
role = "program_ref"
adapter = "vban"
host = "lv1.lan"
in_stream = "strih"
in_channels = 2
out_channels = 0
"#;

const VESTIBULE: &str = r#"
[[participant]]
name = "vestibule"
role = "program_monitor"
adapter = "alsa_hdmi"
hdmi_connector = "HDMI-1"
in_channels = 0
out_channels = 2

[[point]]
src = "lv1"
in_ch = 1
dst = "vestibule"
out_ch = 1

[[point]]
src = "lv1"
in_ch = 2
dst = "vestibule"
out_ch = 2
"#;

fn load(extra: &str) -> anyhow::Result<Matrix> {
    Matrix::from_toml(&format!("{HEAD}{extra}"))
}

#[test]
fn an_alsa_hdmi_participant_loads_and_names_its_connector() {
    let m = load(VESTIBULE).expect("the vestibule loads");
    let id = m.id_of("vestibule").unwrap();
    assert_eq!(m.participants[id].adapter, ADAPTER_ALSA_HDMI);
    assert_eq!(m.alsa_hdmi_outputs(), [(id, "HDMI-1".to_string(), 2)]);
    // it is not a PipeWire sink and not a VBAN output
    assert!(m.local_outputs().is_empty());
    assert!(m.vban_outputs().is_empty());
}

#[test]
fn an_alsa_hdmi_participant_without_a_connector_is_refused() {
    let no_conn = VESTIBULE.replace("hdmi_connector = \"HDMI-1\"\n", "");
    let err = load(&no_conn).unwrap_err().to_string();
    assert!(err.contains("hdmi_connector"), "got: {err}");
    let spaced = VESTIBULE.replace("\"HDMI-1\"", "\"HDMI 1\"");
    assert!(
        load(&spaced).is_err(),
        "a connector name never holds whitespace"
    );
}

#[test]
fn an_alsa_hdmi_participant_is_an_output_only() {
    let with_input = VESTIBULE.replace("in_channels = 0", "in_channels = 2");
    let err = load(&with_input).unwrap_err().to_string();
    assert!(err.contains("in_channels"), "got: {err}");
    let silent = VESTIBULE
        .replace("out_channels = 2", "out_channels = 0")
        .replace(
            "[[point]]\nsrc = \"lv1\"\nin_ch = 1\ndst = \"vestibule\"\nout_ch = 1\n",
            "",
        )
        .replace(
            "[[point]]\nsrc = \"lv1\"\nin_ch = 2\ndst = \"vestibule\"\nout_ch = 2\n",
            "",
        );
    assert!(load(&silent).is_err(), "an HDMI leg with nothing to play");
}

#[test]
fn two_participants_on_one_connector_are_refused() {
    let second = VESTIBULE
        .replace("name = \"vestibule\"", "name = \"foyer\"")
        .replace("dst = \"vestibule\"", "dst = \"foyer\"");
    let err = load(&format!("{VESTIBULE}{second}"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("HDMI-1"), "got: {err}");
}

#[test]
fn hdmi_connector_is_refused_on_any_other_adapter() {
    let on_vban = HEAD.replace(
        "in_stream = \"strih\"",
        "in_stream = \"strih\"\nhdmi_connector = \"HDMI-1\"",
    );
    assert!(Matrix::from_toml(&on_vban).is_err());
}

/// issue 1424 (14.10.2026): the vestibule heard the program ~170-210 ms before the picture, so the
/// leg carries a fixed lip-sync delay. Absent = 0 (the pre-delay behaviour).
#[test]
fn hdmi_delay_ms_is_read_validated_and_only_for_alsa_hdmi() {
    let m = load(VESTIBULE).unwrap();
    let id = m.id_of("vestibule").unwrap();
    assert_eq!(m.alsa_hdmi_delay_ms(id), 0, "absent = no delay");
    let delayed = VESTIBULE.replace(
        "hdmi_connector = \"HDMI-1\"\n",
        "hdmi_connector = \"HDMI-1\"\nhdmi_delay_ms = 180\n",
    );
    let m = load(&delayed).expect("a delayed vestibule loads");
    let id = m.id_of("vestibule").unwrap();
    assert_eq!(m.alsa_hdmi_delay_ms(id), 180);
    let too_long = delayed.replace("hdmi_delay_ms = 180", "hdmi_delay_ms = 1001");
    let err = load(&too_long).unwrap_err().to_string();
    assert!(err.contains("hdmi_delay_ms"), "got: {err}");
    let on_vban = HEAD.replace(
        "in_stream = \"strih\"",
        "in_stream = \"strih\"\nhdmi_delay_ms = 50",
    );
    let err = Matrix::from_toml(&on_vban).unwrap_err().to_string();
    assert!(err.contains("hdmi_delay_ms"), "got: {err}");
}

#[test]
fn the_leg_shows_its_resolved_hw_on_api_state_and_the_status_line() {
    let m = load(VESTIBULE).unwrap();
    let id = m.id_of("vestibule").unwrap();
    let hdmi = HdmiLegStats::default();
    hdmi.record_resolution(&HdmiResolution {
        result: MATCH,
        pcm: Some((0, 7)),
    });
    hdmi.set_running(true);
    hdmi.record_xrun();
    let mut stats = vec![RuntimeStats::default(); m.participants.len()];
    stats[id].hdmi = Some(hdmi.snapshot());
    stats[id].local_audio = Some(LocalAudioFacet {
        tx_blocks: 100,
        pipe_depth_frames: 640,
        pipe_setpoint_frames: 640,
        ..Default::default()
    });
    let hs = HubState::snapshot(&m, "v", &stats);
    let v = serde_json::to_value(&hs).unwrap();
    let p = &v["participants"][id];
    assert_eq!(p["hdmi"]["connector"], "HDMI-1");
    assert_eq!(p["hdmi"]["hw"], "hw:0,7");
    assert_eq!(p["hdmi"]["resolved"], true);
    assert_eq!(p["hdmi"]["running"], true);
    assert_eq!(p["hdmi"]["xruns"], 1);
    assert_eq!(p["local_audio"]["pipe_depth_frames"], 640);
    // the other participants carry no hdmi facet
    assert!(v["participants"][m.id_of("lv1").unwrap()]
        .get("hdmi")
        .is_none());
    let line = hs.status_line();
    assert!(
        line.contains(" hdmi=vestibule:hw:0,7:depth=640:xruns=1"),
        "got: {line}"
    );
    assert!(line.starts_with("intercom-hub: status participants=2 underruns=0 overruns=0"));
}

#[test]
fn an_unresolved_leg_says_why_it_is_silent() {
    let m = load(VESTIBULE).unwrap();
    let id = m.id_of("vestibule").unwrap();
    let hdmi = HdmiLegStats::default();
    hdmi.record_resolution(&HdmiResolution {
        result: NO_MONITOR,
        pcm: None,
    });
    let mut stats = vec![RuntimeStats::default(); m.participants.len()];
    stats[id].hdmi = Some(hdmi.snapshot());
    let hs = HubState::snapshot(&m, "v", &stats);
    let v = serde_json::to_value(&hs).unwrap();
    let p = &v["participants"][id];
    assert_eq!(p["hdmi"]["resolved"], false);
    assert!(p["hdmi"]["hw"].is_null());
    assert!(p["hdmi"]["reason"].as_str().unwrap().contains("no ELD"));
    assert!(hs.status_line().contains(" hdmi=vestibule:silent"));
}

#[test]
fn a_hub_without_an_hdmi_leg_keeps_its_status_line() {
    let m = Matrix::from_toml(HEAD).unwrap();
    let hs = HubState::snapshot(&m, "v", &[RuntimeStats::default()]);
    assert!(!hs.status_line().contains("hdmi="));
}
