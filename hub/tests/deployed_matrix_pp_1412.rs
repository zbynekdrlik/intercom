//! issue 1412: the Poprad matrices must LOAD through the same `Matrix::from_toml` the daemon runs.
//! stream-pp is program-only (lv1's VBAN `stream` -> the `strih-program` sink); strih-pp carries the
//! camera intercom (ROZHODNUTE 13.10.2026). Since issue 1439 reached strih-pp (24.10.2026, design
//! 6385519443) its hub forwards nothing into OBS: lv1 sends `strih-obs` straight into the OBS obs-vban
//! source, so the strih-pp matrix has no `program_out` and no pipewire participant.

use intercom_hub::matrix::{Matrix, ADAPTER_PIPEWIRE, ADAPTER_VBAN, PROGRAM_OUT_ROLE};

const PP_TOML: &str = include_str!("../../intercom.strih-pp.toml");
const STREAM_PP_TOML: &str = include_str!("../../intercom.stream-pp.toml");

/// ROZHODNUTE 13.10.2026 (owner: "prehod to"): the Poprad camera intercom runs on the strih-pp hub,
/// the SNV shape for cam1/4/6/7 + the phones + the cutters. Issue 1439 (24.10.2026): no program path
/// into OBS any more, and nothing on the hub needs PipeWire (issue 1440).
#[test]
fn strih_pp_matrix_loads_as_the_camera_intercom_without_a_program_out() {
    let m = Matrix::from_toml(PP_TOML).expect("the PP matrix must load through Matrix::from_toml");
    assert!(
        m.id_of("program_out").is_none(),
        "no program_out participant"
    );
    assert!(
        m.program_out().is_none(),
        "the hub forwards nothing into OBS"
    );
    assert!(
        m.participants.iter().all(|p| p.adapter != ADAPTER_PIPEWIRE),
        "no pipewire participant on strih-pp"
    );
    // issue 1431: every camera cam1-7 (a joint event brings the SNV cameras here too).
    assert_eq!((m.participants.len(), m.points.len()), (11, 164));
    assert!(m.janus.is_some(), "the phones leg");
    assert_eq!(
        m.video.as_ref().map(|v| v.ndi_source_name.as_str()),
        Some("STRIH-PP (interkom)")
    );
    let mut sent: Vec<String> = m
        .vban_outputs()
        .iter()
        .map(|(id, _, _)| m.participants[*id].name.clone())
        .collect();
    sent.sort();
    assert_eq!(
        sent,
        ["cam1", "cam2", "cam3", "cam4", "cam5", "cam6", "cam7"]
    );
}

/// issue 1412 slice B: the Linux stream box takes its program audio as VBAN `stream` from lv1
/// (owner ROZHODNUTE 11.10.2026).
#[test]
fn stream_pp_matrix_loads_as_a_program_only_hub() {
    assert_program_only(STREAM_PP_TOML, "stream");
}

fn assert_program_only(toml: &str, stream: &str) {
    let m = Matrix::from_toml(toml).expect("the PP matrix must load through Matrix::from_toml");
    assert_eq!(m.participants.len(), 2, "lv1 + program_out only");
    // The program mix is SUMMED through the matrix points; `source_streams` only names the
    // streams. Without the two points the hub received lv1 and wrote silence to the OBS sink
    // (live on strih-pp + stream-pp, 11.10.2026).
    let src_id = m.id_of("lv1").expect("lv1");
    let out_id = m.id_of("program_out").expect("program_out");
    let mut routes: Vec<(usize, usize, usize, usize, f32)> = m
        .points
        .iter()
        .map(|p| (p.src, p.in_ch, p.dst, p.out_ch, p.gain_db))
        .collect();
    routes.sort_by_key(|r| r.1);
    assert_eq!(
        routes,
        [(src_id, 1, out_id, 1, 0.0), (src_id, 2, out_id, 2, 0.0)],
        "exactly lv1 ch1 -> program_out ch1 and ch2 -> ch2 at unity, nothing else"
    );
    assert!(m.janus.is_none(), "no Janus phones leg in PP yet");
    assert!(m.video.is_none(), "no Interkom picture in PP yet");
    assert!(
        m.vban_outputs().is_empty(),
        "the PP hub must never SEND VBAN (no cambox participant)"
    );

    let src = &m.participants[m.id_of("lv1").expect("lv1")];
    assert_eq!(src.adapter, ADAPTER_VBAN);
    assert_eq!(src.role, "program_ref");
    assert_eq!(src.in_stream.as_deref(), Some(stream));
    assert_eq!(src.in_channels, 2);

    let (id, target, streams) = m.program_out().expect("a program_out sink");
    assert_eq!(m.participants[id].role, PROGRAM_OUT_ROLE);
    assert_eq!(m.participants[id].adapter, ADAPTER_PIPEWIRE);
    assert_eq!(target, "strih-program");
    assert_eq!(streams, [stream.to_string()]);
}

/// issue 1424 part 3 (owner 13.10.2026, "mozme to tak spravit"): the PP vestibule TV hears the lv1
/// program straight from the hub on strih-pp's HDMI-1 (the Blackmagic HDMI-to-SDI converter), both
/// channels at unity and nothing else; the connector is named, never a card/device number.
#[test]
fn strih_pp_vestibule_hears_the_lv1_program_on_hdmi_1() {
    use intercom_hub::matrix::ADAPTER_ALSA_HDMI;
    let m = Matrix::from_toml(PP_TOML).expect("the PP matrix must load through Matrix::from_toml");
    let lv1 = m.id_of("lv1").expect("lv1");
    let ves = m.id_of("vestibule").expect("the vestibule participant");
    let p = &m.participants[ves];
    assert_eq!(p.adapter, ADAPTER_ALSA_HDMI);
    assert_eq!(p.role, "program_monitor");
    assert_eq!(p.hdmi_connector.as_deref(), Some("HDMI-1"));
    assert_eq!((p.in_channels, p.out_channels), (0, 2));
    assert_eq!(m.alsa_hdmi_outputs(), [(ves, "HDMI-1".to_string(), 2)]);
    let mut to_ves: Vec<(usize, usize, usize, f32)> = m
        .points
        .iter()
        .filter(|pt| pt.dst == ves)
        .map(|pt| (pt.src, pt.in_ch, pt.out_ch, pt.gain_db))
        .collect();
    to_ves.sort_by_key(|r| r.1);
    assert_eq!(to_ves, [(lv1, 1, 1, 0.0), (lv1, 2, 2, 0.0)]);
    assert!(
        m.points.iter().all(|pt| pt.src != ves),
        "the vestibule is an output only"
    );
}

/// issue 1438: the PP cutters open the Yamaha MG-XU directly through ALSA by its card ID `MGXU`
/// (2 in / 2 out, its hw channels 1 and 2), so a card that leaves USB is the leg DOWN, never a
/// PipeWire fallback into the program sink (22.10.2026).
#[test]
fn strih_pp_cutters_open_the_mg_xu_by_card_id_1438() {
    use intercom_hub::matrix::ADAPTER_ALSA;
    let m = Matrix::from_toml(PP_TOML).expect("the PP matrix must load through Matrix::from_toml");
    let cutters = m.id_of("cutters").expect("cutters");
    assert_eq!(m.participants[cutters].adapter, ADAPTER_ALSA);
    assert!(
        m.local_inputs().is_empty() && m.local_outputs().is_empty(),
        "no pipewire node is left on the cutters"
    );
    let cards = m.alsa_cards();
    assert_eq!(cards.len(), 1);
    let c = &cards[0];
    assert_eq!(
        (c.participant, c.card.as_str(), c.device),
        (cutters, "MGXU", 0)
    );
    assert_eq!((c.in_channels, c.hw_in_channels), (2, 2));
    assert_eq!((c.out_channels, c.hw_out_channels), (2, 2));
    assert_eq!(c.in_map, [0, 1]);
    assert_eq!(c.out_map, [0, 1]);
    // Issue 1439 (24.10.2026): no program sink either; OBS takes lv1's `strih-obs` itself.
    assert!(m.program_out().is_none());
}
