//! The checked-in strih-snv matrix must LOAD through the same `Matrix::from_toml` the daemon runs
//! (issue 1345 M1, review F2). The Python converter pytest pins byte-for-byte parity + parses the
//! TOML with `tomllib`, but it does NOT exercise the Rust role/adapter/channel-range/self-route
//! rules — so a future converter/XML change could produce a syntactically-valid TOML the daemon
//! REJECTS at rig startup. This closes that Tier-0 gap: CI loads the real deployed matrix here.

use intercom_hub::janus_rtp::JanusCodec;
use intercom_hub::matrix::{Matrix, ADAPTER_ALSA, ADAPTER_VBAN, PROGRAM_OUT_ROLE};

const DEPLOYED_TOML: &str = include_str!("../../intercom.strih-snv.toml");

#[test]
fn deployed_strih_lx_matrix_loads_and_has_the_expected_shape() {
    let m = Matrix::from_toml(DEPLOYED_TOML)
        .expect("the deployed intercom.strih-snv.toml must load through Matrix::from_toml");

    // issue 1439 phase 2: the VB-Matrix grid minus VASIO8 (the old OBS program capture) and its 4 points.
    assert_eq!(m.participants.len(), 14, "expected 14 participants");
    assert_eq!(
        m.points.len(),
        212,
        "expected the 216-point VB-Matrix grid minus VASIO8's 4"
    );

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
    // Issue 1345 (25.9.2026): the deployed phones leg is Opus with in-band FEC, not PCMU.
    assert_eq!(j.codec, JanusCodec::Opus);

    // issue 1439 phase 2 (owner ROZHODNUTE 22.10.2026): the hub is the intercom only and forwards nothing
    // into the strih OBS, which receives the console's VBAN program itself (the obs-vban source).
    assert!(
        m.program_out().is_none(),
        "the strih-snv hub must declare no program_out sink"
    );
    assert!(
        !m.participants.iter().any(|p| p.role == PROGRAM_OUT_ROLE),
        "no program_out participant"
    );
    assert!(
        m.id_of("program_monitor").is_none(),
        "VASIO8 is not carried into the hub at all"
    );

    // issue 1438: the cutters open the MiniFuse 4 directly through ALSA (card ID M4), no PipeWire
    // capture or playback node is left in the matrix.
    assert!(
        m.local_inputs().is_empty(),
        "no pipewire capture input is left (the cutters record through arecord)"
    );
    let cards = m.alsa_cards();
    assert_eq!(cards.len(), 1, "one card leg (the cutters)");
    let c = &cards[0];
    assert_eq!(m.participants[c.participant].name, "cutters");
    assert_eq!(m.participants[c.participant].adapter, ADAPTER_ALSA);
    assert_eq!(
        c.card, "M4",
        "the MiniFuse 4 by its ALSA card ID, never an index"
    );
    assert_eq!(c.device, 0);
    assert_eq!(c.in_channels, 2, "two cutter mics");
    assert_eq!(c.hw_in_channels, 6, "the MiniFuse captures 6 channels");
    assert_eq!(c.in_map, [0, 1], "the mics are hw inputs 1 and 2");
}

#[test]
fn deployed_matrix_plays_the_cutters_mix_to_the_minifuse_with_talkback_gain() {
    // issue 1345 (24.9.2026): the operator heard nothing because the cutters were capture-only.
    // issue 1438: the cutters' 4 outputs play on the MiniFuse's hw outputs 1-4 of 6 through aplay.
    let m = Matrix::from_toml(DEPLOYED_TOML).expect("the deployed matrix loads");
    let cutters = m.id_of("cutters").expect("cutters");
    assert!(
        m.local_outputs().is_empty(),
        "no pipewire playback egress is left"
    );
    let cards = m.alsa_cards();
    assert_eq!(cards.len(), 1);
    let c = &cards[0];
    assert_eq!(c.participant, cutters);
    assert_eq!(
        c.out_channels, 4,
        "the 4 MiniFuse outputs the VB-Matrix fed"
    );
    assert_eq!(c.hw_out_channels, 6, "the MiniFuse plays 6 channels");
    assert_eq!(c.out_map, [0, 1, 2, 3]);
    assert_eq!(c.period_frames, 256, "one hub block");

    // Owner ruling 24.9.2026: no hub makeup gain on the talkback -- every cutters -> phones / camN
    // point is unity (the level is set on the MiniFuse preamp).
    let talkback: Vec<_> = m.points.iter().filter(|p| p.src == cutters).collect();
    assert!(!talkback.is_empty());
    assert!(talkback.iter().all(|p| p.gain_db.abs() < 1e-6));
}
