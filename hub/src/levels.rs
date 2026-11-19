//! The cutters' level sliders (intercom#34): a trim on top of the matrix's fixed `gain_db`.
//!
//! A `[[level]]` table in the routing TOML names one slider: an id (`name`), the Slovak label the
//! page shows, the output participant it trims (`dst`, the cutters), and its `sources` — a
//! participant (`"lv1"`, every channel) or one input channel of it (`"foh_dante:3"`). At load
//! ([`resolve`]) each slider takes every existing point from its sources into `dst`; the TOML's
//! `gain_db` stays the base and the slider multiplies it (0 dB = today's mix, bit for bit).
//!
//! The real-time side is one relaxed atomic load per point per block ([`Levels::linear`]): an
//! `AtomicU32` holding the f32 linear factor (0 when muted). Everything else (the HTTP control
//! routes, `/api/state`, the state file) runs off the hub-mix thread; a writer lock orders the
//! changes and their file writes, and the real-time thread never takes it.
//!
//! The values survive a restart in `levels_file` ([`Levels::load_file`], [`Levels::save_file`]):
//! read once at start (known names clamped into their range; an unknown name or a broken file is
//! logged and ignored, never fatal), written atomically (a temp file + rename) on every change.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::matrix::{gain_linear, Participant, Point};

/// The state file the unit's `StateDirectory=intercom-hub` holds, beside dlink's `dlink.toml`.
pub const DEFAULT_LEVELS_FILE: &str = "/var/lib/intercom-hub/levels.toml";
/// The loopback-only control listener (the box's nginx and the public phone front proxy only
/// `:8790`, so nothing outside the box reaches the sliders).
pub const DEFAULT_CONTROL_BIND: &str = "127.0.0.1:8791";
/// A slider's range when the TOML gives none.
pub const DEFAULT_MIN_DB: f32 = -60.0;
pub const DEFAULT_MAX_DB: f32 = 12.0;
/// The widest range a slider may declare: below -120 dB is silence anyway, above +24 dB clips.
const RANGE_FLOOR_DB: f32 = -120.0;
const RANGE_CEIL_DB: f32 = 24.0;
/// A slider name is a URL path segment (`PUT /api/levels/{name}`).
const MAX_NAME_LEN: usize = 32;

fn default_min_db() -> f32 {
    DEFAULT_MIN_DB
}
fn default_max_db() -> f32 {
    DEFAULT_MAX_DB
}

/// The wire shape of a `[[level]]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LevelToml {
    pub name: String,
    pub label: String,
    pub dst: String,
    pub sources: Vec<String>,
    #[serde(default = "default_min_db")]
    pub min_db: f32,
    #[serde(default = "default_max_db")]
    pub max_db: f32,
}

/// One resolved slider.
#[derive(Debug, Clone)]
pub struct LevelSpec {
    pub name: String,
    pub label: String,
    /// The output participant's id and name.
    pub dst: usize,
    pub dst_name: String,
    /// The sources as authored (`"lv1"`, `"foh_dante:3"`).
    pub sources: Vec<String>,
    pub min_db: f32,
    pub max_db: f32,
    /// How many matrix points the slider trims.
    pub points: usize,
}

/// One slider's live value. `linear` is all the real-time thread reads.
#[derive(Debug)]
struct LevelValue {
    /// f32 bits: `10^(db/20)`, or 0 while muted.
    linear: AtomicU32,
    /// f32 bits of the dB the cutter set (kept while muted).
    db: AtomicU32,
    muted: AtomicBool,
}

impl LevelValue {
    fn unity() -> Self {
        LevelValue {
            linear: AtomicU32::new(1f32.to_bits()),
            db: AtomicU32::new(0f32.to_bits()),
            muted: AtomicBool::new(false),
        }
    }
}

/// The slider set: the specs + their live values, shared (an `Arc`) by the engine, `/api/state`
/// and the control routes. A clone shares the same values.
#[derive(Debug, Clone, Default)]
pub struct Levels {
    specs: Arc<[LevelSpec]>,
    values: Arc<[LevelValue]>,
    /// Orders the changes and their file writes. Never taken on the hub-mix thread.
    writer: Arc<Mutex<()>>,
}

/// A slider's value as the API and `/api/state` show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LevelState {
    pub name: String,
    pub label: String,
    pub dst: String,
    pub sources: Vec<String>,
    pub db: f32,
    pub muted: bool,
    pub min_db: f32,
    pub max_db: f32,
    pub points: usize,
}

/// What one change did (for the log line and the response).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelChange {
    pub old_db: f32,
    pub old_muted: bool,
    pub db: f32,
    pub muted: bool,
}

/// A source as authored: a participant, or one 1-based input channel of it.
fn parse_source(s: &str) -> Result<(&str, Option<usize>)> {
    match s.split_once(':') {
        None => Ok((s, None)),
        Some((name, ch)) => {
            let ch: usize = ch
                .parse()
                .map_err(|_| anyhow!("source '{s}': the channel after ':' must be a number"))?;
            Ok((name, Some(ch)))
        }
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_NAME_LEN
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Resolve the `[[level]]` tables onto the loaded points: each slider claims every point from its
/// sources into its `dst` (sets `Point::level`). Refuses a bad or repeated name, an empty label,
/// an unknown participant or channel, a `dst` with no output, a source with no point into `dst`, a
/// point two sliders claim, and a bad range.
pub fn resolve(
    raw: Vec<LevelToml>,
    participants: &[Participant],
    index: &HashMap<String, usize>,
    points: &mut [Point],
) -> Result<Levels> {
    let mut specs = Vec::with_capacity(raw.len());
    let mut names = std::collections::HashSet::new();
    for (slot, l) in raw.into_iter().enumerate() {
        let who = format!("level '{}'", l.name);
        if !valid_name(&l.name) {
            bail!("{who}: the name must be 1..={MAX_NAME_LEN} of a-z 0-9 _ - (it is a URL path)");
        }
        if !names.insert(l.name.clone()) {
            bail!("{who}: declared twice");
        }
        if l.label.trim().is_empty() {
            bail!("{who}: the label must not be empty");
        }
        if !(l.min_db.is_finite() && l.max_db.is_finite())
            || l.min_db < RANGE_FLOOR_DB
            || l.max_db > RANGE_CEIL_DB
            || l.min_db > 0.0
            || l.max_db < 0.0
            || l.min_db >= l.max_db
        {
            bail!(
                "{who}: range {} .. {} dB must hold 0 dB inside {RANGE_FLOOR_DB} .. {RANGE_CEIL_DB}",
                l.min_db,
                l.max_db
            );
        }
        let dst = *index
            .get(&l.dst)
            .ok_or_else(|| anyhow!("{who}: unknown dst participant '{}'", l.dst))?;
        if participants[dst].out_channels == 0 {
            bail!("{who}: dst '{}' has no output channels", l.dst);
        }
        if l.sources.is_empty() {
            bail!("{who}: needs at least one source");
        }
        let mut claimed = 0usize;
        for s in &l.sources {
            let (name, ch) = parse_source(s).map_err(|e| anyhow!("{who}: {e}"))?;
            let src = *index
                .get(name)
                .ok_or_else(|| anyhow!("{who}: unknown source participant '{name}'"))?;
            if let Some(ch) = ch {
                let n = participants[src].in_channels;
                if ch < 1 || ch > n {
                    bail!("{who}: source '{s}' channel {ch} out of range 1..={n}");
                }
            }
            let mut hits = 0usize;
            for p in points
                .iter_mut()
                .filter(|p| p.src == src && p.dst == dst && ch.is_none_or(|c| p.in_ch == c))
            {
                if let Some(other) = p.level {
                    let other = specs
                        .get(other)
                        .map_or(l.name.as_str(), |o: &LevelSpec| o.name.as_str());
                    bail!(
                        "{who}: point {}:{} -> {}:{} is already trimmed by level '{other}'",
                        participants[p.src].name,
                        p.in_ch,
                        l.dst,
                        p.out_ch
                    );
                }
                p.level = Some(slot);
                hits += 1;
            }
            if hits == 0 {
                bail!("{who}: source '{s}' has no point into '{}'", l.dst);
            }
            claimed += hits;
        }
        specs.push(LevelSpec {
            name: l.name,
            label: l.label,
            dst,
            dst_name: participants[dst].name.clone(),
            sources: l.sources,
            min_db: l.min_db,
            max_db: l.max_db,
            points: claimed,
        });
    }
    let values: Vec<LevelValue> = specs.iter().map(|_| LevelValue::unity()).collect();
    Ok(Levels {
        specs: specs.into(),
        values: values.into(),
        writer: Arc::new(Mutex::new(())),
    })
}

impl Levels {
    /// The real-time read: slider `i`'s linear factor (1.0 at 0 dB, 0.0 muted). One relaxed load.
    #[inline]
    pub fn linear(&self, i: usize) -> f32 {
        self.values
            .get(i)
            .map_or(1.0, |v| f32::from_bits(v.linear.load(Ordering::Relaxed)))
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn specs(&self) -> &[LevelSpec] {
        &self.specs
    }

    /// The slider's index by name.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.specs.iter().position(|s| s.name == name)
    }

    /// Take the writer lock that orders changes and their file writes (poison is recovered: the
    /// values are atomics, a panicked holder cannot leave them half-written).
    pub fn writer(&self) -> MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn store(&self, i: usize, db: f32, muted: bool) {
        let v = &self.values[i];
        v.db.store(db.to_bits(), Ordering::Relaxed);
        v.muted.store(muted, Ordering::Relaxed);
        let lin = if muted { 0.0 } else { gain_linear(db) };
        v.linear.store(lin.to_bits(), Ordering::Relaxed);
    }

    fn current(&self, i: usize) -> (f32, bool) {
        let v = &self.values[i];
        (
            f32::from_bits(v.db.load(Ordering::Relaxed)),
            v.muted.load(Ordering::Relaxed),
        )
    }

    /// Set slider `i`. Refuses a value that is not finite or lies outside the slider's range. The
    /// caller holds [`Levels::writer`] when it also saves the file.
    pub fn set(&self, i: usize, db: f32, muted: bool) -> Result<LevelChange> {
        let spec = self
            .specs
            .get(i)
            .ok_or_else(|| anyhow!("no level slot {i}"))?;
        if !db.is_finite() || db < spec.min_db || db > spec.max_db {
            bail!(
                "level '{}': {db} dB is outside {} .. {} dB",
                spec.name,
                spec.min_db,
                spec.max_db
            );
        }
        let (old_db, old_muted) = self.current(i);
        self.store(i, db, muted);
        Ok(LevelChange {
            old_db,
            old_muted,
            db,
            muted,
        })
    }

    /// Every slider's current value.
    pub fn states(&self) -> Vec<LevelState> {
        self.specs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (db, muted) = self.current(i);
                LevelState {
                    name: s.name.clone(),
                    label: s.label.clone(),
                    dst: s.dst_name.clone(),
                    sources: s.sources.clone(),
                    db,
                    muted,
                    min_db: s.min_db,
                    max_db: s.max_db,
                    points: s.points,
                }
            })
            .collect()
    }

    /// The output participants the sliders trim (ids, deduplicated, in slider order).
    pub fn dsts(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        for s in self.specs.iter() {
            if !out.contains(&s.dst) {
                out.push(s.dst);
            }
        }
        out
    }

    /// One info line per slider: what it trims (the load log).
    pub fn log_resolved(&self) {
        for s in self.specs.iter() {
            tracing::info!(
                level = %s.name,
                label = %s.label,
                dst = %s.dst_name,
                sources = ?s.sources,
                points = s.points,
                min_db = s.min_db,
                max_db = s.max_db,
                "level slider resolved"
            );
        }
    }

    /// Apply a stored state file's text: known names take their value clamped into the slider's
    /// range; unknown names are reported. A text that does not parse changes nothing.
    pub fn apply_stored(&self, text: &str) -> Result<StoredReport> {
        let stored: StoredFile = toml::from_str(text).context("parse the levels state file")?;
        let mut report = StoredReport::default();
        for (name, v) in stored.levels {
            let Some(i) = self.index_of(&name) else {
                report.unknown.push(name);
                continue;
            };
            let spec = &self.specs[i];
            let db = if v.db.is_finite() { v.db } else { 0.0 };
            let clamped = db.clamp(spec.min_db, spec.max_db);
            if clamped != v.db {
                report.clamped.push(name.clone());
            }
            self.store(i, clamped, v.muted);
            report.taken.push((name, clamped, v.muted));
        }
        Ok(report)
    }

    /// Read the state file at start. A missing file keeps every slider at 0 dB; an unreadable or
    /// broken one is logged and ignored. Never fatal.
    pub fn load_file(&self, path: &Path) {
        if self.is_empty() {
            return;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %path.display(), "levels: no state file yet, every slider at 0 dB");
                return;
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "levels: state file unreadable, every slider at 0 dB");
                return;
            }
        };
        match self.apply_stored(&text) {
            Ok(r) => {
                for (name, db, muted) in &r.taken {
                    tracing::info!(level = %name, db, muted, "levels: stored value taken");
                }
                for name in &r.clamped {
                    tracing::warn!(level = %name, "levels: stored value outside the slider's range, clamped");
                }
                for name in &r.unknown {
                    tracing::warn!(level = %name, path = %path.display(), "levels: the state file names a slider this config has not, ignored");
                }
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "levels: state file ignored, every slider at 0 dB");
            }
        }
    }

    /// The state file's text for the current values.
    pub fn render_stored(&self) -> String {
        let mut levels = BTreeMap::new();
        for (i, s) in self.specs.iter().enumerate() {
            let (db, muted) = self.current(i);
            levels.insert(s.name.clone(), StoredLevel { db, muted });
        }
        let body = toml::to_string(&StoredFile { levels }).unwrap_or_default();
        format!("# intercom-hub level sliders (intercom#34); written by the hub on every change.\n{body}")
    }

    /// Write the state file atomically: a temp file beside it, fsync, rename over it.
    pub fn save_file(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, &self.render_stored())
    }
}

/// What a stored file did at load.
#[derive(Debug, Default, PartialEq)]
pub struct StoredReport {
    pub taken: Vec<(String, f32, bool)>,
    pub clamped: Vec<String>,
    pub unknown: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredLevel {
    db: f32,
    #[serde(default)]
    muted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredFile {
    #[serde(default)]
    levels: BTreeMap<String, StoredLevel>,
}

/// Write `text` to `path` so a reader sees the old file or the new one, never half of one.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other(format!("{} has no file name", path.display())))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
