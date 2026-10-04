//! Issue 1345 live (24.9.2026, after the capture-buffer fix): the `phones` (Janus) leg arrives as
//! 960-frame RTP chunks (20 ms PCMU upsampled to 48 kHz), bursty from the Janus mixer timer, into
//! the default 8-block (2048-frame) VBAN ring -> ~60 overruns/s = the phones' voice is chopped
//! before it reaches the operator and the camboxes. The Janus ingress must use the same
//! target-fill ring as the local capture input.
//!
//! Issue 1401 moved the buffer choice out of the daemon's `main` into `inputs::input_buffers`, so
//! this is checked on the real deployed matrix instead of on the text of `main.rs`.

use intercom_hub::inputs::input_buffers;
use intercom_hub::matrix::{Matrix, ADAPTER_JANUS};
use intercom_hub::vban_io::BufferKind;

#[test]
fn janus_participants_get_the_target_fill_ring() {
    let m = Matrix::from_toml(include_str!("../../intercom.strih-lx.toml")).unwrap();
    let buffers = input_buffers(&m);
    let janus: Vec<usize> = m
        .participants
        .iter()
        .enumerate()
        .filter(|(_, p)| p.adapter == ADAPTER_JANUS)
        .map(|(id, _)| id)
        .collect();
    assert!(!janus.is_empty(), "the deployed matrix has the phones leg");
    for id in janus {
        assert_eq!(
            buffers[id].kind(),
            BufferKind::LocalCapture,
            "input_buffers must give the Janus (phones) ingress the local-capture target-fill ring"
        );
    }
}
