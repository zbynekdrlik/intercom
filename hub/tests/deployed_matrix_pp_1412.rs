//! issue 1412: the Poprad matrices must LOAD through the same `Matrix::from_toml` the daemon runs.
//! stream-pp is program-only (lv1's VBAN `stream` -> the `strih-program` sink); strih-pp carries the
//! camera intercom (ROZHODNUTE 13.10.2026) plus the unchanged program path.

use intercom_hub::matrix::{Matrix, ADAPTER_PIPEWIRE, ADAPTER_VBAN, PROGRAM_OUT_ROLE};

const PP_TOML: &str = include_str!("../../intercom.strih-pp.toml");
const STREAM_PP_TOML: &str = include_str!("../../intercom.stream-pp.toml");

/// ROZHODNUTE 13.10.2026 (owner: "prehod to"): the Poprad camera intercom runs on the strih-pp hub,
/// the SNV shape for cam1/4/6/7 + the phones + the cutters, with the program path unchanged.
#[test]
fn strih_pp_matrix_loads_as_the_camera_intercom_with_the_program_path() {
    let m = Matrix::from_toml(PP_TOML).expect("the PP matrix must load through Matrix::from_toml");
    let src_id = m.id_of("lv1").expect("lv1");
    let out_id = m.id_of("program_out").expect("program_out");
    let mut to_program: Vec<(usize, usize, usize, f32)> = m
        .points
        .iter()
        .filter(|p| p.dst == out_id)
        .map(|p| (p.src, p.in_ch, p.out_ch, p.gain_db))
        .collect();
    to_program.sort_by_key(|r| r.1);
    assert_eq!(to_program, [(src_id, 1, 1, 0.0), (src_id, 2, 2, 0.0)]);
    let (_, target, streams) = m.program_out().expect("a program_out sink");
    assert_eq!(target, "strih-program");
    assert_eq!(streams, ["strih".to_string()]);
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
    assert_eq!(sent, ["cam1", "cam4", "cam6", "cam7"]);
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
