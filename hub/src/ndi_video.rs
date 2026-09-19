//! The Interkom picture leg (issue 1345 M3c): an NDI low-bandwidth receiver → decimate → JPEG →
//! the shared [`VideoState`] slot the HTTP layer serves as MJPEG at `/interkom.mjpeg`.
//!
//! The NDI recv FFI + the cross-platform runtime discovery are COPIED VERBATIM from the bkshading
//! preview (`bkshading/service/src/preview/*.rs`) — this hub does NOT depend on the bkshading crate
//! (that would pull its whole tree); the copied modules carry a `keep in sync` header. The layering
//! mirrors bkshading so the pixel/decision logic is pure and CI-testable WITHOUT libndi:
//! - pure core (default features, always compiled + tested): [`frame`], [`pattern`], [`decimate`],
//!   [`encode`], [`convert`], [`ndi_paths`] (cross-platform runtime discovery), [`shared_runtime`]
//!   (process-shared load-once keep-alive), [`slot`] (the shared frame slot + the `/api/state` facet)
//! - runtime glue: [`source`] (trait + stub), [`worker`] (one OS thread)
//! - `#[cfg(feature = "ndi")]` [`ndi_source`]: the real libndi receiver at bandwidth LOWEST + the
//!   BGRX/BGRA colour format (mirrors the appliance `src/ndi.rs`), a RUNTIME dynamic load so the
//!   DEFAULT build still compiles on CI with no libndi; a missing runtime warns + backs off + retries
//!   forever (fail-loud, non-crashing, no stub image).
//!
//! Feature `ndi` is DEFAULT ON (the bkshading service model): the picture leg is a needed feature, so
//! it is on by default, never a forgettable toggle. The `--no-default-features` build (CI keeps it
//! proven) swaps the real receiver for the stub test-pattern source so the libndi-free path can't
//! bit-rot.

pub mod convert;
pub mod decimate;
pub mod encode;
pub mod frame;
pub mod ndi_paths;
pub mod pattern;
pub mod shared_runtime;
pub mod slot;
pub mod source;
pub mod worker;

#[cfg(feature = "ndi")]
pub mod ndi_source;

pub use slot::{VideoState, VideoStats};

use std::sync::Arc;

use crate::matrix::VideoConfig;

/// Start the Interkom picture capture worker for `cfg` and return the shared [`VideoState`] the HTTP
/// layer reads. The caller only calls this when the `[video]` table is present AND `enabled`.
pub fn start(cfg: &VideoConfig) -> Arc<VideoState> {
    let state = Arc::new(VideoState::new(&cfg.ndi_source_name).with_target_fps(cfg.fps));
    worker::spawn_video(
        cfg.ndi_source_name.clone(),
        cfg.fps as f64,
        cfg.jpeg_quality,
        state.clone(),
        source::build_default_source,
    );
    state
}
