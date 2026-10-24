//! Issue 1345 live (24.9.2026, after the capture-buffer fix): the `phones` (Janus) leg arrives as
//! 960-frame RTP chunks (20 ms PCMU upsampled to 48 kHz), bursty from the Janus mixer timer, into
//! the default 8-block (2048-frame) VBAN ring -> ~60 overruns/s = the phones' voice is chopped
//! before it reaches the operator and the camboxes. The Janus ingress must use the same
//! target-fill ring as the local capture input.
//!
//! Issue 1401 moved the buffer choice out of the daemon's `main` into `inputs::input_buffers`, so
//! this is checked on the real deployed matrix instead of on the text of `main.rs`. Issue 1461
//! (lane 2) gives that ingress the resampler leg every other foreign-clock input has.

use intercom_hub::inputs::input_buffers;
use intercom_hub::matrix::{Matrix, ADAPTER_JANUS};
use intercom_hub::vban_io::BufferKind;

#[test]
fn janus_participants_get_the_resampler_leg() {
    let m = Matrix::from_toml(include_str!("../../intercom.strih-snv.toml")).unwrap();
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
        // Issue 1461 lane 2: the phones run on the browser's / Janus's clock, a foreign clock like
        // a VBAN sender's, so their ingress is the same resampler leg with a fixed target (the
        // local-capture ring only trimmed).
        assert_eq!(
            buffers[id].kind(),
            BufferKind::JanusCapture,
            "the Janus (phones) ingress is no longer the trim-only local-capture ring"
        );
        let fill = buffers[id]
            .network_stats()
            .expect("the Janus (phones) ingress is a resampler leg");
        assert_eq!(fill.target_frames, 2048, "its fixed target: 42.7 ms");
    }
}
