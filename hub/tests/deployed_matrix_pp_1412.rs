//! issue 1412: the Poprad strih's program-only matrix (`intercom/intercom.strih-pp.toml`) must LOAD
//! through the same `Matrix::from_toml` the daemon runs, and carry exactly the program path:
//! lv1's VBAN `strih` -> the `strih-program` PipeWire sink. No cambox, no Janus, no picture.

use intercom_hub::matrix::{Matrix, ADAPTER_PIPEWIRE, ADAPTER_VBAN, PROGRAM_OUT_ROLE};

const PP_TOML: &str = include_str!("../../intercom.strih-pp.toml");
const STREAM_PP_TOML: &str = include_str!("../../intercom.stream-pp.toml");

#[test]
fn strih_pp_matrix_loads_as_a_program_only_hub() {
    assert_program_only(PP_TOML, "strih");
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
    assert!(m.points.is_empty(), "no intercom routing points");
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
