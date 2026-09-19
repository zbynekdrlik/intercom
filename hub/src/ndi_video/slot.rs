//! The shared latest-frame slot + the `/api/state` `video` facet (issue 1345 M3c).
//!
//! One process-wide [`VideoState`] holds the newest JPEG-encoded Interkom picture frame (the design's
//! `Arc<RwLock<Option<Arc<Vec<u8>>>>>` + a wall-clock `updated_ms` + a monotonic frame counter) plus
//! the small set of counters the `/api/state` `video` facet reports. The capture worker `publish`es
//! each new frame here; the HTTP layer (`/interkom.mjpeg`, `/interkom.jpg`) reads the latest frame and
//! the counter without blocking the worker. Every lock is poison-recovered (a panicked worker must
//! never take the HTTP surface down), mirroring the bkshading preview store.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::Serialize;

/// The default picture rate (frames/sec) — the `[video].fps` default, used to derive the MJPEG poll
/// cadence when the config has not overridden it.
pub const DEFAULT_TARGET_FPS: u32 = 10;

/// The serialized `video` facet added to `/api/state` (present only when the hub has a `[video]`
/// config). Every key is always present so a consumer reads a stable shape; `last_frame_age_ms` /
/// `last_error` render as `null` until there is a frame / an error.
#[derive(Debug, Clone, Serialize, Default)]
pub struct VideoStats {
    /// The configured NDI source name the receiver looks for (e.g. `STRIH-LX (interkom)`).
    pub source: String,
    /// Whether the NDI receiver is currently connected to the source.
    pub connected: bool,
    /// The measured emitted frame rate (frames/sec), 0 until the first second of frames.
    pub fps_actual: f32,
    /// Age in ms of the latest frame (`null` = no frame yet).
    pub last_frame_age_ms: Option<u64>,
    /// Total frames published this process.
    pub frames: u64,
    /// The most recent capture/connect error, one per transition (`null` = none / recovered).
    pub last_error: Option<String>,
}

/// The process-shared latest-frame slot + facet counters. Cheap to share behind an `Arc` (the worker
/// thread + the HTTP handlers all hold one).
pub struct VideoState {
    source: String,
    /// The latest JPEG-encoded frame (shared so a reader clones an `Arc`, not the buffer).
    frame: RwLock<Option<Arc<Vec<u8>>>>,
    /// Wall-clock ms when the latest frame was published (0 = never).
    updated_ms: AtomicU64,
    /// Monotonically increasing per published frame — the MJPEG "new frame?" signal.
    frames: AtomicU64,
    /// Whether the NDI receiver is connected (owned by the worker).
    connected: AtomicBool,
    /// fps × 1000 (fixed-point, no float atomic).
    fps_milli: AtomicU64,
    /// Target picture rate (frames/sec), used to size the MJPEG poll interval.
    target_fps: AtomicU32,
    /// The most recent error (poison-recovered lock).
    last_error: Mutex<Option<String>>,
}

impl VideoState {
    /// A fresh slot for `source` with no frame yet and the default target fps.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            frame: RwLock::new(None),
            updated_ms: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            fps_milli: AtomicU64::new(0),
            target_fps: AtomicU32::new(DEFAULT_TARGET_FPS),
            last_error: Mutex::new(None),
        }
    }

    /// Builder: set the target picture rate (frames/sec). A zero/absurd value falls back to the
    /// default so the poll cadence can never divide by zero.
    pub fn with_target_fps(self, fps: u32) -> Self {
        self.set_target_fps(fps);
        self
    }

    /// The configured NDI source name.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Set the target picture rate (frames/sec); a zero value is ignored (keeps the default).
    pub fn set_target_fps(&self, fps: u32) {
        if fps > 0 {
            self.target_fps.store(fps, Ordering::Relaxed);
        }
    }

    /// The target picture rate (frames/sec), never zero.
    pub fn target_fps(&self) -> u32 {
        self.target_fps.load(Ordering::Relaxed).max(1)
    }

    /// Publish a new JPEG frame: swap the slot, stamp `updated_ms`, bump the counter, and clear any
    /// prior error (a fresh frame is a recovery). Connection state is owned by the worker, not touched
    /// here.
    pub fn publish(&self, jpeg: Vec<u8>, now_ms: u64) {
        if let Ok(mut slot) = self.frame.write() {
            *slot = Some(Arc::new(jpeg));
        }
        self.updated_ms.store(now_ms, Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut e) = self.last_error.lock() {
            *e = None;
        }
    }

    /// The number of frames published (also the "new frame?" counter for the MJPEG stream).
    pub fn frame_counter(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Wall-clock ms of the latest frame (0 = none yet).
    pub fn updated_ms(&self) -> u64 {
        self.updated_ms.load(Ordering::Relaxed)
    }

    /// The latest frame, or `None` if none has been published.
    pub fn latest_frame(&self) -> Option<Arc<Vec<u8>>> {
        self.frame
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(Arc::clone))
    }

    /// The latest frame paired with its counter (for the MJPEG "advanced since last sent?" check).
    pub fn latest(&self) -> Option<(Arc<Vec<u8>>, u64)> {
        let frame = self.latest_frame()?;
        Some((frame, self.frame_counter()))
    }

    /// Set the receiver connection state (worker-owned).
    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    /// Record the most recent error (or clear it with `None`).
    pub fn set_error(&self, err: Option<String>) {
        if let Ok(mut e) = self.last_error.lock() {
            *e = err;
        }
    }

    /// Record the measured emitted frame rate (frames/sec).
    pub fn record_fps(&self, fps: f32) {
        let milli = (fps.max(0.0) * 1000.0).round() as u64;
        self.fps_milli.store(milli, Ordering::Relaxed);
    }

    /// A point-in-time snapshot for the `/api/state` `video` facet.
    pub fn snapshot(&self, now_ms: u64) -> VideoStats {
        let updated = self.updated_ms.load(Ordering::Relaxed);
        let last_frame_age_ms = if updated == 0 {
            None
        } else {
            Some(now_ms.saturating_sub(updated))
        };
        VideoStats {
            source: self.source.clone(),
            connected: self.connected.load(Ordering::Relaxed),
            fps_actual: self.fps_milli.load(Ordering::Relaxed) as f32 / 1000.0,
            last_frame_age_ms,
            frames: self.frames.load(Ordering::Relaxed),
            last_error: self.last_error.lock().ok().and_then(|g| g.clone()),
        }
    }
}
