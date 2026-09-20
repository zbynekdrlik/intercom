//! The local PipeWire program-audio matrix rules (issue 1344): the `program_out` sink + the talkback
//! capture input the hub gains to replace the last VB-Matrix function on the strih-lx notebook.
//!
//! These exercise the Rust `Matrix::from_toml` validation the daemon runs (the Python converter's
//! byte-parity test does not check the role/adapter rules), plus the `program_out()` / `local_inputs()`
//! accessors the daemon wires the PipeWire bridges from.

use intercom_hub::matrix::{Matrix, ADAPTER_PIPEWIRE, PROGRAM_OUT_ROLE};

/// A matrix with a valid program_out sink (fed by two VBAN program refs) + a talkback capture input.
const VALID: &str = r#"
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
host = "fohabl.lan"
in_stream = "fohabl-strih"
in_channels = 2
out_channels = 0

[[participant]]
name = "lv1"
role = "program_ref"
adapter = "vban"
host = "lv1.lan"
in_stream = "lv1-strih"
in_channels = 2
out_channels = 0

[[participant]]
name = "program_out"
role = "program_out"
adapter = "pipewire"
pipewire_target = "strih-program"
source_streams = ["fohabl-strih", "lv1-strih"]
in_channels = 0
out_channels = 2

[[participant]]
name = "cutters"
role = "cutters"
adapter = "pipewire"
pipewire_source = "alsa_input.usb-Arturia_MiniFuse_4-00.pro-input-0"
in_channels = 2
out_channels = 0

[[point]]
src = "fohabl"
in_ch = 1
dst = "program_out"
out_ch = 1
[[point]]
src = "fohabl"
in_ch = 2
dst = "program_out"
out_ch = 2
[[point]]
src = "lv1"
in_ch = 1
dst = "program_out"
out_ch = 1
[[point]]
src = "lv1"
in_ch = 2
dst = "program_out"
out_ch = 2
[[point]]
src = "cutters"
in_ch = 1
dst = "cam1"
out_ch = 1
"#;

fn without(section: &str) -> String {
    // Helper to build a variant by string replacement in tests below.
    VALID.replace(section, "")
}

#[test]
fn valid_program_out_and_talkback_load_and_are_discoverable() {
    let m = Matrix::from_toml(VALID).expect("the valid program-audio matrix must load");

    // The program_out accessor returns the single sink with its target + source streams.
    let (pid, target, streams) = m.program_out().expect("a program_out is present");
    assert_eq!(m.participants[pid].name, "program_out");
    assert_eq!(m.participants[pid].adapter, ADAPTER_PIPEWIRE);
    assert_eq!(m.participants[pid].role, PROGRAM_OUT_ROLE);
    assert_eq!(target, "strih-program");
    assert_eq!(
        streams,
        vec!["fohabl-strih".to_string(), "lv1-strih".to_string()]
    );

    // The talkback capture input is discoverable with its capture node + channel count.
    let inputs = m.local_inputs();
    assert_eq!(inputs.len(), 1, "one talkback capture input");
    let (cid, node, chans) = &inputs[0];
    assert_eq!(m.participants[*cid].name, "cutters");
    assert_eq!(node, "alsa_input.usb-Arturia_MiniFuse_4-00.pro-input-0");
    assert_eq!(*chans, 2);
}

#[test]
fn a_matrix_without_a_program_out_still_loads_with_none() {
    // The other rig matrices (the engine/state tests) declare no program_out — that must stay valid.
    let toml = r#"
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
"#;
    let m = Matrix::from_toml(toml).unwrap();
    assert!(m.program_out().is_none());
    assert!(m.local_inputs().is_empty());
}

#[test]
fn two_program_out_participants_are_refused() {
    let bad = VALID.replace(
        r#"[[participant]]
name = "cutters""#,
        r#"[[participant]]
name = "program_out2"
role = "program_out"
adapter = "pipewire"
pipewire_target = "strih-program-2"
source_streams = ["fohabl-strih"]
in_channels = 0
out_channels = 2

[[participant]]
name = "cutters""#,
    );
    let err = Matrix::from_toml(&bad).unwrap_err().to_string();
    assert!(err.contains("at most one 'program_out'"), "got: {err}");
}

#[test]
fn program_out_source_colliding_with_a_cambox_stream_is_refused() {
    let bad = VALID.replace(
        r#"source_streams = ["fohabl-strih", "lv1-strih"]"#,
        r#"source_streams = ["cam1", "fohabl-strih"]"#,
    );
    let err = Matrix::from_toml(&bad).unwrap_err().to_string();
    assert!(err.contains("collides with a cambox stream"), "got: {err}");
}

#[test]
fn program_out_source_that_is_not_a_received_vban_stream_is_refused() {
    let bad = VALID.replace(
        r#"source_streams = ["fohabl-strih", "lv1-strih"]"#,
        r#"source_streams = ["fohabl-strih", "ghost-strih"]"#,
    );
    let err = Matrix::from_toml(&bad).unwrap_err().to_string();
    assert!(err.contains("not a received VBAN in_stream"), "got: {err}");
}

#[test]
fn program_out_without_a_target_is_refused() {
    let bad = without("pipewire_target = \"strih-program\"\n");
    let err = Matrix::from_toml(&bad).unwrap_err().to_string();
    assert!(err.contains("non-empty pipewire_target"), "got: {err}");
}

#[test]
fn program_out_with_empty_source_streams_is_refused() {
    let bad = VALID.replace(
        r#"source_streams = ["fohabl-strih", "lv1-strih"]"#,
        "source_streams = []",
    );
    let err = Matrix::from_toml(&bad).unwrap_err().to_string();
    assert!(err.contains("at least one source_stream"), "got: {err}");
}

#[test]
fn a_pipewire_capture_input_without_a_source_node_is_refused() {
    let bad = without("pipewire_source = \"alsa_input.usb-Arturia_MiniFuse_4-00.pro-input-0\"\n");
    let err = Matrix::from_toml(&bad).unwrap_err().to_string();
    assert!(err.contains("needs a pipewire_source"), "got: {err}");
}

#[test]
fn program_out_role_on_a_non_pipewire_adapter_is_refused() {
    let bad = VALID.replace(
        r#"name = "program_out"
role = "program_out"
adapter = "pipewire""#,
        r#"name = "program_out"
role = "program_out"
adapter = "none""#,
    );
    let err = Matrix::from_toml(&bad).unwrap_err().to_string();
    assert!(
        err.contains("requires the 'pipewire' adapter"),
        "got: {err}"
    );
}
