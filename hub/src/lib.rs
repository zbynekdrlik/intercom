//! `intercom-hub` — the strih-lx intercom mix-minus (N-1) hub (issue 1345 M1).
//!
//! One tokio daemon on the strih-lx Linux notebook that replaces the Windows VB-Matrix's static
//! N-1 intercom for the VBAN world (the 7 camboxes) FIRST. It loads a declarative routing
//! [`matrix::Matrix`] from `intercom.toml` (GENERATED from the live VB-Matrix XML by
//! `scripts/vbmatrix_to_intercom_toml.py`, never a GUI); runs the [`engine::Engine`] N-1 mixer (for
//! every output channel, the sum of the routed input channels with gain, structurally EXCLUDING a
//! participant's own source — mix-minus); speaks byte-identical VBAN to the camboxes via
//! [`vban_io`] (recv + demux by stream name into per-participant jitter buffers; send `camN` stereo
//! PCM16 back to `camN.lan:6980`); and exposes [`state`] over [`http`] (`/api/state` JSON + a 1 Hz
//! `/ws` push + `/api/version`).
//!
//! M1 scope is the VBAN leg + the engine + observability. The MiniFuse/PipeWire adapter (cutters,
//! speakers, line-3/4, the −8/−10 dB program refs into the cans), Janus + the phone PWA + the
//! Interkom video, and the cut-over are M2/M3/M4 — those participants are declared in the matrix
//! with `adapter = "none"` and are computed by the engine but not yet delivered.

pub mod engine;
pub mod fir;
pub mod http;
pub mod janus_rtp;
pub mod local_audio;
pub mod matrix;
pub mod mulaw;
pub mod ndi_video;
pub mod state;
pub mod vban_io;
pub mod vban_rate;
