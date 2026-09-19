//! The checked-in strih-lx matrix must LOAD through the same `Matrix::from_toml` the daemon runs
//! (issue 1345 M1, review F2). The Python converter pytest pins byte-for-byte parity + parses the
//! TOML with `tomllib`, but it does NOT exercise the Rust role/adapter/channel-range/self-route
//! rules — so a future converter/XML change could produce a syntactically-valid TOML the daemon
//! REJECTS at rig startup. This closes that Tier-0 gap: CI loads the real deployed matrix here.

use intercom_hub::matrix::{Matrix, ADAPTER_VBAN};

const DEPLOYED_TOML: &str = include_str!("../../intercom.strih-lx.toml");

#[test]
fn deployed_strih_lx_matrix_loads_and_has_the_expected_shape() {
    let m = Matrix::from_toml(DEPLOYED_TOML)
        .expect("the deployed intercom.strih-lx.toml must load through Matrix::from_toml");

    assert_eq!(m.participants.len(), 15, "expected 15 participants");
    assert_eq!(m.points.len(), 216, "expected the 216-point VB-Matrix grid");

    // 10 VBAN inputs (cam1..7 + fohabl/lv1/mbc), 7 active VBAN outputs (cam1..7).
    let vban_in = m
        .participants
        .iter()
        .filter(|p| p.adapter == ADAPTER_VBAN && p.in_stream.is_some())
        .count();
    let vban_out = m.vban_outputs().len();
    assert_eq!(vban_in, 10, "expected 10 VBAN inputs");
    assert_eq!(vban_out, 7, "expected 7 active VBAN outputs");

    // The mix-minus invariant is upheld structurally (from_toml would have refused a self-route).
    assert!(
        m.points.iter().all(|pt| pt.src != pt.dst),
        "no self-route point may survive load"
    );

    // Spot-check the −8 dB fohabl→cutters program ref survived into the resolved matrix.
    let fohabl = m.id_of("fohabl").expect("fohabl participant");
    let cutters = m.id_of("cutters").expect("cutters participant");
    let cams: Vec<usize> = (1..=7)
        .filter_map(|i| m.id_of(&format!("cam{i}")))
        .collect();
    assert_eq!(cams.len(), 7);
    let fohabl_cutters: Vec<_> = m
        .points
        .iter()
        .filter(|pt| pt.src == fohabl && pt.dst == cutters && pt.in_ch == 1)
        .collect();
    assert!(
        !fohabl_cutters.is_empty(),
        "fohabl in1 must route to the cutters"
    );
    assert!(
        fohabl_cutters
            .iter()
            .all(|pt| (pt.gain_db - (-8.0)).abs() < 1e-6),
        "the fohabl program ref must stay at -8 dB"
    );
    // A program reference must NEVER reach a cambox output.
    assert!(
        !m.points
            .iter()
            .any(|pt| pt.src == fohabl && cams.contains(&pt.dst)),
        "program references must never reach a cambox output"
    );

    // issue 1345 M3a: the deployed matrix carries the Janus audiobridge edge — the phones
    // participant is the single `janus` participant, and the `[janus]` table parsed with defaults.
    let phones = m
        .janus_participant()
        .expect("a janus participant (phones) is present");
    assert_eq!(m.participants[phones].name, "phones");
    assert_eq!(m.participants[phones].role, "phones");
    let j = m.janus.as_ref().expect("the [janus] table is present");
    assert_eq!(j.room, 1000);
    assert_eq!(j.rtp_bind, "0.0.0.0:6990");
    assert_eq!(
        j.room_secret_file.as_deref(),
        Some("/etc/intercom-hub/janus-room.secret")
    );
}
