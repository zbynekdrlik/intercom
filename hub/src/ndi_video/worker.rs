//! The Interkom picture capture loop (issue 1345 M3c) — the runtime glue that drives a
//! [`VideoSource`] into the shared [`VideoState`] slot.
//!
//! ONE OS thread (not a tokio task: the NDI capture is a blocking FFI call, so it stays off the async
//! runtime — same as the bkshading preview worker). The loop is: build the source → capture →
//! decimate → JPEG-encode → publish, forever; on a source error it logs ONE warn per transition, backs
//! off (1 → 10 s) and rebuilds. Any failure (runtime missing, source not found, capture timeout) is
//! fail-loud + non-crashing + never a stub image (with `--features ndi`, the default). The pure
//! decision logic it composes (decimate, encode) is unit-tested separately.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::ndi_video::decimate::Decimator;
use crate::ndi_video::encode::encode_jpeg;
use crate::ndi_video::slot::VideoState;
use crate::ndi_video::source::VideoSource;

/// How long one capture call blocks waiting for a frame before looping.
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(1000);
/// Reconnect backoff bounds after a source build failure / a source that ended (1 s → 10 s).
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// The rolling window over which `fps_actual` is measured.
const FPS_WINDOW: Duration = Duration::from_secs(1);

/// A source builder: NDI-source-name → a boxed [`VideoSource`] (or an error to retry). A fn pointer so
/// a test can inject a fake source; production passes [`crate::ndi_video::source::build_default_source`].
pub type SourceBuilder = fn(&str) -> anyhow::Result<Box<dyn VideoSource>>;

/// Wall-clock milliseconds (for the slot's `updated_ms` staleness).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Spawn the Interkom picture capture loop on its own OS thread. `fps` thins the source to the target
/// picture rate; `jpeg_quality` (0–100) is the encode quality; `build` selects the source (stub by
/// default, real NDI with `--features ndi`) and is retried on failure.
pub fn spawn_video(
    source_name: String,
    fps: f64,
    jpeg_quality: u8,
    state: Arc<VideoState>,
    build: SourceBuilder,
) {
    let log_name = source_name.clone();
    let spawned = thread::Builder::new()
        .name("interkom-video".to_string())
        .spawn(move || run_forever(&source_name, fps, jpeg_quality, &state, build));
    if let Err(e) = spawned {
        // A thread that cannot even start is logged, not panicked — the picture failing must never
        // take the hub down.
        tracing::error!(source = %log_name, error = %e, "failed to spawn interkom-video thread");
    }
}

fn run_forever(
    source_name: &str,
    fps: f64,
    jpeg_quality: u8,
    state: &Arc<VideoState>,
    build: SourceBuilder,
) {
    let mut backoff = BACKOFF_MIN;
    loop {
        match build(source_name) {
            Ok(mut src) => {
                tracing::info!(source = %source_name, "interkom-video source connected");
                state.set_connected(true);
                state.set_error(None);
                let produced = run_source(src.as_mut(), fps, jpeg_quality, state);
                state.set_connected(false);
                tracing::warn!(source = %source_name, "interkom-video source ended; reconnecting");
                if produced {
                    backoff = BACKOFF_MIN; // a working run resets the backoff
                }
            }
            Err(e) => {
                state.set_connected(false);
                state.set_error(Some(e.to_string()));
                tracing::warn!(source = %source_name, error = %e, "interkom-video source build failed; retrying");
            }
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Drive one connected source until it errors. Returns whether at least one frame was published (so a
/// working run can reset the reconnect backoff). Decimation runs on a MONOTONIC `Instant` (immune to a
/// wall-clock / NTP backward step); the slot's `updated_ms` stays wall-clock for staleness.
fn run_source(
    src: &mut dyn VideoSource,
    fps: f64,
    jpeg_quality: u8,
    state: &Arc<VideoState>,
) -> bool {
    let mut decim = Decimator::new(fps);
    let started = Instant::now();
    let mut window_start = Instant::now();
    let mut window_count: u32 = 0;
    let mut produced = false;
    loop {
        match src.next_frame(CAPTURE_TIMEOUT) {
            Ok(Some(frame)) => {
                if !decim.should_emit(started.elapsed().as_millis() as u64) {
                    continue; // thinned to the target fps
                }
                match encode_jpeg(&frame, jpeg_quality) {
                    Ok(jpeg) => {
                        state.publish(jpeg, now_ms());
                        produced = true;
                        window_count += 1;
                        let elapsed = window_start.elapsed();
                        if elapsed >= FPS_WINDOW {
                            state.record_fps(window_count as f32 / elapsed.as_secs_f32());
                            window_start = Instant::now();
                            window_count = 0;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "interkom-video jpeg encode failed");
                    }
                }
            }
            Ok(None) => { /* timeout — keep waiting for the next frame */ }
            Err(e) => {
                state.set_error(Some(e.to_string()));
                tracing::warn!(error = %e, "interkom-video capture error; rebuilding source");
                return produced;
            }
        }
    }
}
