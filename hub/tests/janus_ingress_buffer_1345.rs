//! Issue 1345 live (24.9.2026, after the capture-buffer fix): the `phones` (Janus) leg arrives as
//! 960-frame RTP chunks (20 ms PCMU upsampled to 48 kHz), bursty from the Janus mixer timer, into
//! the default 8-block (2048-frame) VBAN ring -> ~60 overruns/s = the phones' voice is chopped
//! before it reaches the operator and the camboxes. The Janus ingress must use the same
//! target-fill ring as the local capture input.
use std::path::PathBuf;

#[test]
fn janus_participants_get_the_target_fill_ring() {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&p).expect("read main.rs");
    let f = src
        .split("fn input_buffers(")
        .nth(1)
        .expect("input_buffers exists");
    let body = &f[..f.find("\n}\n").expect("fn end")];
    assert!(
        body.contains("ADAPTER_JANUS"),
        "input_buffers must give the Janus (phones) ingress the local-capture target-fill ring"
    );
}
