//! copied from bkshading/service/src/preview/ (bandwidth LOWEST, BGRX/BGRA colour) — keep in sync (issue 1345 M3c).
//! The preview source seam — the trait behind which the stub test pattern and the
//! feature-gated real NDI receiver live.
//!
//! The DEFAULT (and the only source built on CI) is [`StubSource`], so the whole preview
//! pipeline compiles, runs and is verified with NO libndi and NO camera. With
//! `--features ndi`, [`build_default_source`] builds the real receiver instead; the rest of
//! the pipeline (decimate → encode → store → HTTP → web UI) is unchanged.

use std::time::Duration;

use crate::ndi_video::frame::RawFrame;
use crate::ndi_video::pattern::test_pattern_rgb;

/// A source of raw preview frames. Implementations run on their own OS thread (the capture
/// may block) and may block up to `timeout` waiting for a frame.
pub trait VideoSource: Send {
    /// Human-readable source name (for logging).
    fn name(&self) -> &str;

    /// Block up to `timeout` for the next frame. `Ok(None)` = no frame yet (timeout); `Err`
    /// = the source failed (the worker backs off and rebuilds it). Never panics.
    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<RawFrame>>;
}

/// Default source: an animated test pattern at a fixed native rate. Gives the pipeline a
/// realistic ~30 fps input for the decimator to thin, so a running service shows a live,
/// visibly-updating preview block even with no NDI feed wired.
pub struct StubSource {
    name: String,
    width: u32,
    height: u32,
    native_fps: u64,
    tick: u64,
}

impl StubSource {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            width: 320,
            height: 180,
            native_fps: 30,
            tick: 0,
        }
    }
}

impl VideoSource for StubSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn next_frame(&mut self, _timeout: Duration) -> anyhow::Result<Option<RawFrame>> {
        // Emit at the native rate so the worker's decimator sees a realistic input to thin.
        let interval = Duration::from_millis(1000 / self.native_fps.max(1));
        std::thread::sleep(interval);
        let rgb = test_pattern_rgb(self.width, self.height, self.tick);
        self.tick = self.tick.wrapping_add(1);
        Ok(Some(RawFrame::new(self.width, self.height, rgb)))
    }
}

/// Build the preview source for `source_name` (an NDI source name from the camera config).
///
/// Default build: always the [`StubSource`]. With `--features ndi`: the real NDI receiver at
/// low bandwidth; a connect failure is propagated so the worker backs off and retries (a
/// cambox NDI feed that comes and goes must self-heal) rather than silently faking a feed.
#[cfg(not(feature = "ndi"))]
pub fn build_default_source(
    _guard: std::sync::Arc<crate::dante_guard::DanteGuard>,
) -> crate::ndi_video::worker::SourceBuilder {
    std::sync::Arc::new(|source_name: &str| {
        Ok(Box::new(StubSource::new(source_name)) as Box<dyn VideoSource>)
    })
}

/// With `--features ndi` the builder skips a source announced at a Dante address (intercom#25:
/// §4, a receiver never uses a `<dante>` address for non-Dante media).
#[cfg(feature = "ndi")]
pub fn build_default_source(
    guard: std::sync::Arc<crate::dante_guard::DanteGuard>,
) -> crate::ndi_video::worker::SourceBuilder {
    std::sync::Arc::new(move |source_name: &str| {
        Ok(
            Box::new(crate::ndi_video::ndi_source::NdiVideoSource::connect(
                source_name,
                &guard,
            )?) as Box<dyn VideoSource>,
        )
    })
}

/// The address of an NDI source's `p_url_address` (`ip:port`, IPv6 in brackets). None when it is
/// empty or not an address.
pub fn ndi_url_ip(url: &str) -> Option<std::net::IpAddr> {
    url.trim()
        .parse::<std::net::SocketAddr>()
        .ok()
        .map(|a| a.ip())
}
