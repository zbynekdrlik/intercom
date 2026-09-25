//! issue 1345 (24.9.2026 production fix): the local PipeWire EGRESS to the operator's headphones,
//! and the capture child's argv.
//!
//! On Linux the `cutters` participant was capture-only: the hub read the MiniFuse mics but played
//! NOTHING back, so the strih operator heard no cameraman and no phone. A pipewire participant that
//! is not the `program_out`, has output channels and a `pipewire_target` now gets its own
//! `pw-cat --playback` sink fed from its OWN N-1 output bus. The MiniFuse playback node is a
//! pro-audio node (`AUX0..AUX5`), so the sink carries an explicit channel map. Without the map,
//! 4 channels default to FL/FR/RL/RR and never land on the AUX ports.

use intercom_hub::local_audio::{
    pw_cat_playback_argv_with_map, pw_cat_record_argv, PW_GRAPH_BURST_FRAMES,
};
use intercom_hub::matrix::Matrix;

const MINIFUSE_OUT: &str = "alsa_output.usb-ARTURIA_MiniFuse_4-00.pro-output-0";

fn toml_with_cutters(cutters_extra: &str, cutters_out: usize) -> String {
    format!(
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
name = "fohabl"
role = "program_ref"
adapter = "vban"
host = "10.77.8.8"
in_stream = "fohabl-strih"
in_channels = 2
out_channels = 0

[[participant]]
name = "cutters"
role = "cutters"
adapter = "pipewire"
pipewire_source = "alsa_input.usb-ARTURIA_MiniFuse_4-00.pro-input-0"
{cutters_extra}
in_channels = 2
out_channels = {cutters_out}

[[participant]]
name = "program_out"
role = "program_out"
adapter = "pipewire"
pipewire_target = "strih-program"
source_streams = ["fohabl-strih"]
in_channels = 0
out_channels = 2

[[point]]
src = "cam1"
in_ch = 1
dst = "cutters"
out_ch = 1

[[point]]
src = "cutters"
in_ch = 1
dst = "cam1"
out_ch = 1
gain_db = 12.0

[[point]]
src = "fohabl"
in_ch = 1
dst = "program_out"
out_ch = 1
"#
    )
}

#[test]
fn a_capture_participant_with_a_target_is_also_a_local_output() {
    let t = toml_with_cutters(
        &format!(
            "pipewire_target = \"{MINIFUSE_OUT}\"\npipewire_channel_map = \"AUX0,AUX1,AUX2,AUX3\""
        ),
        4,
    );
    let m = Matrix::from_toml(&t).expect("source + target on one participant is valid");
    let cutters = m.id_of("cutters").unwrap();

    let outs = m.local_outputs();
    assert_eq!(
        outs.len(),
        1,
        "only the cutters (the program_out is its own sink)"
    );
    let (pid, target, chans, map) = &outs[0];
    assert_eq!(*pid, cutters);
    assert_eq!(target, MINIFUSE_OUT);
    assert_eq!(*chans, 4);
    assert_eq!(map.as_deref(), Some("AUX0,AUX1,AUX2,AUX3"));

    // The same participant is still the talkback capture input.
    let ins = m.local_inputs();
    assert_eq!(ins.len(), 1);
    assert_eq!(ins[0].0, cutters);
    // The program_out is untouched.
    assert_eq!(m.program_out().unwrap().1, "strih-program");
    // The talkback gain survives into the resolved point.
    let p = m.points.iter().find(|p| p.src == cutters).unwrap();
    assert!((p.gain_linear - 3.981).abs() < 1e-2, "+12 dB = x3.98");
}

#[test]
fn a_capture_only_participant_has_no_local_output() {
    let m = Matrix::from_toml(&toml_with_cutters("", 4)).unwrap();
    assert!(m.local_outputs().is_empty());
}

#[test]
fn a_channel_map_must_name_exactly_the_output_channels() {
    let t = toml_with_cutters(
        &format!("pipewire_target = \"{MINIFUSE_OUT}\"\npipewire_channel_map = \"AUX0,AUX1\""),
        4,
    );
    let err = Matrix::from_toml(&t).unwrap_err().to_string();
    assert!(err.contains("pipewire_channel_map"), "got: {err}");
}

#[test]
fn an_empty_target_is_refused() {
    let t = toml_with_cutters("pipewire_target = \"\"", 4);
    let err = Matrix::from_toml(&t).unwrap_err().to_string();
    assert!(err.contains("pipewire_target"), "got: {err}");
}

#[test]
fn the_playback_argv_carries_the_aux_channel_map_only_when_given() {
    let a = pw_cat_playback_argv_with_map(MINIFUSE_OUT, 48000, 4, Some("AUX0,AUX1,AUX2,AUX3"));
    let pos = |k: &str| a.iter().position(|x| x == k).unwrap();
    assert_eq!(a[pos("--channels") + 1], "4");
    assert_eq!(a[pos("--channel-map") + 1], "AUX0,AUX1,AUX2,AUX3");
    assert_eq!(a[pos("--target") + 1], MINIFUSE_OUT);
    assert_eq!(a.last().unwrap(), "-");

    // The program sink keeps its issue-1344 argv exactly (no map).
    let p = pw_cat_playback_argv_with_map("strih-program", 48000, 2, None);
    let expected: Vec<String> = [
        "pw-cat",
        "--playback",
        "--raw",
        "--rate",
        "48000",
        "--channels",
        "2",
        "--format",
        "s16",
        "--target",
        "strih-program",
        "-",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(p, expected);
}

#[test]
fn the_record_child_asks_for_the_1024_frame_graph_quantum() {
    // pw-cat takes the latency as direct SAMPLES at `--rate` (or a time unit): `--latency 1024` with
    // `--rate 48000` sets `node.latency = "1024/48000"`. The literal `1024/48000` is REJECTED by
    // pw-cat 1.6.2 ("bad latency value ... (bad unit)", live-verified on strih-lx).
    //
    // 24.9.2026 (owner accepted 25.9): the capture child asked `--latency 256` and pulled the whole
    // graph down to quantum 256 while the MiniFuse playback ran period 1024 -- the cameraman sounded
    // robotic in the operator headphones. The request must be the MiniFuse graph quantum itself.
    let a = pw_cat_record_argv("alsa_input.minifuse", 48000, 2);
    let pos = a.iter().position(|x| x == "--latency").expect("--latency");
    assert_eq!(a[pos + 1], "1024");
    assert_eq!(a[pos + 1], PW_GRAPH_BURST_FRAMES.to_string());
    let rate = a.iter().position(|x| x == "--rate").unwrap();
    assert_eq!(a[rate + 1], "48000");
    assert_eq!(a.last().unwrap(), "-");
}
