//! The level mixer's plain-Rust logic (intercom#48): the API bodies, the number formats the page
//! shows and writes into its `data-*` hooks, the meter, the status line and one strip's send
//! state machine. No DOM and no WASM here, so `cargo test` runs all of it on the host; the
//! components in `levels` only wire these to signals and to `fetch`.
//!
//! The behaviour is the one `web/levels.js` had (intercom#34/#38), number for number: the E2E
//! spec `tests/e2e/levels.spec.js` reads these strings back unchanged.

use serde::{Deserialize, Serialize};

/// How often the page re-reads `/api/levels` (the meter and changes from another window).
pub const POLL_MS: u32 = 1000;
/// A dragged fader sends at most one change per this many ms (the `change` event sends at once).
pub const SEND_THROTTLE_MS: u32 = 120;
/// The bottom of the output meter.
pub const METER_FLOOR_DB: f64 = -60.0;
/// The fader's step (the range input's `step`).
pub const FADER_STEP_DB: f64 = 0.5;

/// One slider of `GET /api/levels` (the fields the page uses).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Level {
    pub name: String,
    #[serde(default)]
    pub label: String,
    pub db: f64,
    pub muted: bool,
    pub min_db: f64,
    pub max_db: f64,
    /// Only `true` makes the master strip (`level.master === true` in the old page).
    #[serde(default)]
    pub master: Option<bool>,
}

impl Level {
    pub fn is_master(&self) -> bool {
        self.master == Some(true)
    }
}

/// One trimmed output's meter.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Output {
    #[serde(default)]
    pub participant: String,
    #[serde(default)]
    pub out_level_dbfs: Option<f64>,
}

/// `GET /api/levels`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LevelsBody {
    pub levels: Vec<Level>,
    #[serde(default)]
    pub outputs: Vec<Output>,
}

/// The `PUT /api/levels/{name}` answer: the slider as it now is and whether it was stored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PutAnswer {
    pub db: f64,
    pub muted: bool,
    pub saved: bool,
}

/// The `PUT /api/levels/{name}` body: exactly these two fields (the hub denies unknown ones).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PutLevel {
    pub db: f64,
    pub muted: bool,
}

pub fn parse_levels(text: &str) -> Result<LevelsBody, String> {
    serde_json::from_str(text).map_err(|e| format!("bad /api/levels body: {e}"))
}

pub fn parse_put_answer(text: &str) -> Result<PutAnswer, String> {
    serde_json::from_str(text).map_err(|e| format!("bad PUT answer: {e}"))
}

pub fn put_body(db: f64, muted: bool) -> String {
    serde_json::to_string(&PutLevel { db, muted }).expect("two plain fields always serialize")
}

/// The API path of one slider (`encodeURIComponent` of a name the hub only allows as a-z 0-9 _ -).
pub fn level_path(name: &str) -> String {
    let mut out = String::from("/api/levels/");
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A number as JavaScript's `String(n)` writes it for the values this page holds (no exponent:
/// dB values within -120 .. +24): the shortest round-trip digits, `-0` as `0`.
pub fn js_num(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    format!("{v}")
}

/// JavaScript's `Math.round` (halves toward +infinity).
pub fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

/// JavaScript's `n.toFixed(1)` for these magnitudes (a `-0.0` reads `0.0`).
fn fixed1(v: f64) -> String {
    let s = format!("{v:.1}");
    if s == "-0.0" {
        "0.0".into()
    } else {
        s
    }
}

/// A strip's dB line: `+3.5 dB`, `0.0 dB`, `-12.0 dB` (one decimal, a plus above 0).
pub fn fmt_db(db: f64) -> String {
    let v = js_round(db * 10.0) / 10.0;
    format!("{}{} dB", if v > 0.0 { "+" } else { "" }, fixed1(v))
}

/// What a strip's dB line shows: its value, or `stlmené` while muted.
pub fn strip_db_text(db: f64, muted: bool) -> String {
    if muted {
        "stlmené".into()
    } else {
        fmt_db(db)
    }
}

/// The mute button's text: what a press will do.
pub fn mute_text(muted: bool) -> &'static str {
    if muted {
        "Zapnúť"
    } else {
        "Stlmiť"
    }
}

/// A fader position read from the range input, snapped to its 0.5 dB step and clamped into the
/// slider's range (the browser already does this; an unparsable value keeps `fallback`).
pub fn fader_db(value: &str, min_db: f64, max_db: f64, fallback: f64) -> f64 {
    let Ok(v) = value.trim().parse::<f64>() else {
        return fallback;
    };
    if !v.is_finite() {
        return fallback;
    }
    let snapped = min_db + ((v - min_db) / FADER_STEP_DB).round() * FADER_STEP_DB;
    snapped.clamp(min_db, max_db)
}

/// Where a dB value sits on a fader, 0.0 (min, bottom) .. 1.0 (max, top).
pub fn fader_fraction(db: f64, min_db: f64, max_db: f64) -> f64 {
    if max_db <= min_db {
        return 0.0;
    }
    ((db - min_db) / (max_db - min_db)).clamp(0.0, 1.0)
}

/// The dB value at a fader fraction (the inverse of [`fader_fraction`]), snapped to the step.
pub fn fader_db_at(fraction: f64, min_db: f64, max_db: f64) -> f64 {
    let raw = min_db + fraction.clamp(0.0, 1.0) * (max_db - min_db);
    let snapped = min_db + ((raw - min_db) / FADER_STEP_DB).round() * FADER_STEP_DB;
    snapped.clamp(min_db, max_db)
}

/// The output meter as the page shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct MeterView {
    /// `data-db` of the meter.
    pub db_attr: String,
    /// `data-hot`: `None` keeps whatever it was (the old page left it alone with no reading).
    pub hot: Option<bool>,
    /// The fill's CSS height, `"37.5%"`.
    pub height: String,
    /// The value under the meter.
    pub text: String,
}

/// The meter of the first trimmed output (the cutters' cans).
pub fn meter_view(outputs: &[Output]) -> MeterView {
    let Some(db) = outputs.first().and_then(|o| o.out_level_dbfs) else {
        return MeterView {
            db_attr: "-120".into(),
            hot: None,
            height: "0%".into(),
            text: "— dB".into(),
        };
    };
    let pct = (((db - METER_FLOOR_DB) / -METER_FLOOR_DB) * 100.0).clamp(0.0, 100.0);
    MeterView {
        db_attr: js_num(db),
        hot: Some(db > -3.0),
        height: format!("{}%", fixed1(pct)),
        text: if db <= -119.0 {
            "ticho".into()
        } else {
            format!("{} dB", js_num(js_round(db)))
        },
    }
}

/// The status line's state (`data-state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusState {
    Loading,
    Good,
    Warn,
    Bad,
}

impl StatusState {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusState::Loading => "loading",
            StatusState::Good => "good",
            StatusState::Warn => "warn",
            StatusState::Bad => "bad",
        }
    }
}

/// The status line: its state and its Slovak text.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub state: StatusState,
    pub text: String,
}

impl Status {
    pub fn new(state: StatusState, text: impl Into<String>) -> Self {
        Status {
            state,
            text: text.into(),
        }
    }
    pub fn loading() -> Self {
        Status::new(StatusState::Loading, "Načítavam…")
    }
    pub fn connected() -> Self {
        Status::new(StatusState::Good, "Pripojené")
    }
    pub fn unreachable() -> Self {
        Status::new(StatusState::Bad, "Hub nedostupný — skúšam znova")
    }
    pub fn send_failed() -> Self {
        Status::new(StatusState::Bad, "Hub nedostupný — zmena sa neuložila")
    }
    pub fn refused(body: &str) -> Self {
        Status::new(StatusState::Bad, format!("Hub zmenu odmietol: {body}"))
    }
    /// After the hub took a change: stored, or applied but not stored.
    pub fn saved(saved: bool) -> Self {
        if saved {
            Status::new(StatusState::Good, "Pripojené — uložené")
        } else {
            Status::new(
                StatusState::Warn,
                "Platí, ale neuložilo sa (po reštarte sa vráti)",
            )
        }
    }
}

/// What a successful poll does to the status line: `Pripojené` unless it already says good or
/// warn (a save's message stays until the next failure).
pub fn status_after_poll(current: StatusState) -> Option<Status> {
    match current {
        StatusState::Good | StatusState::Warn => None,
        _ => Some(Status::connected()),
    }
}

/// How a PUT ended.
#[derive(Debug, Clone, PartialEq)]
pub enum SendOutcome {
    /// 200 with the answer.
    Answer(PutAnswer),
    /// Another status, with the body text.
    Refused(String),
    /// No answer at all (network).
    Failed,
}

/// What the page does after a PUT ended.
#[derive(Debug, Clone, PartialEq)]
pub struct SendEffect {
    pub status: Status,
    /// `data-saved` of the strip, when the hub answered.
    pub saved: Option<bool>,
    /// The hub's value to show (only when nothing newer is pending and no drag runs).
    pub apply: Option<(f64, bool)>,
    /// Send again at once: a newer value arrived while this one was in flight.
    pub resend: bool,
}

/// One strip's state: the value shown and the send bookkeeping. One request per strip is in
/// flight at a time; a newer value that arrives meanwhile is sent right after it, so the hub always
/// ends on the last position. A poll never overwrites a strip that is busy.
#[derive(Debug, Clone, PartialEq)]
pub struct StripState {
    pub name: String,
    pub min_db: f64,
    pub max_db: f64,
    pub db: f64,
    pub muted: bool,
    pub dragging: bool,
    pub inflight: bool,
    pub dirty: bool,
    /// A throttled send is scheduled.
    pub timer: bool,
}

impl StripState {
    pub fn new(level: &Level) -> Self {
        StripState {
            name: level.name.clone(),
            min_db: level.min_db,
            max_db: level.max_db,
            db: level.db,
            muted: level.muted,
            dragging: false,
            inflight: false,
            dirty: false,
            timer: false,
        }
    }

    pub fn busy(&self) -> bool {
        self.dragging || self.inflight || self.dirty
    }

    pub fn pointer_down(&mut self) {
        self.dragging = true;
    }

    pub fn pointer_up(&mut self) {
        self.dragging = false;
    }

    /// The range moved (`input`): show it; `true` = schedule the throttled send (none scheduled).
    pub fn input(&mut self, value: &str) -> bool {
        self.db = fader_db(value, self.min_db, self.max_db, self.db);
        if self.timer {
            false
        } else {
            self.timer = true;
            true
        }
    }

    /// The throttle timer fired.
    pub fn timer_fired(&mut self) {
        self.timer = false;
    }

    /// The range was let go or stepped (`change`): the value, the drag ended; `true` = a
    /// throttled send was pending and must be cancelled (the caller sends now).
    pub fn change(&mut self, value: &str) -> bool {
        self.dragging = false;
        self.db = fader_db(value, self.min_db, self.max_db, self.db);
        std::mem::replace(&mut self.timer, false)
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
    }

    /// Start a send: the body to PUT, or `None` when one is in flight (this one follows it).
    pub fn begin_send(&mut self) -> Option<PutLevel> {
        if self.inflight {
            self.dirty = true;
            return None;
        }
        self.inflight = true;
        self.dirty = false;
        Some(PutLevel {
            db: self.db,
            muted: self.muted,
        })
    }

    /// A send ended.
    pub fn finish_send(&mut self, outcome: SendOutcome) -> SendEffect {
        let mut effect = match outcome {
            SendOutcome::Answer(a) => SendEffect {
                status: Status::saved(a.saved),
                saved: Some(a.saved),
                apply: (!self.dirty && !self.dragging).then_some((a.db, a.muted)),
                resend: false,
            },
            SendOutcome::Refused(body) => SendEffect {
                status: Status::refused(&body),
                saved: None,
                apply: None,
                resend: false,
            },
            SendOutcome::Failed => SendEffect {
                status: Status::send_failed(),
                saved: None,
                apply: None,
                resend: false,
            },
        };
        if let Some((db, muted)) = effect.apply {
            self.apply_server(db, muted);
        }
        self.inflight = false;
        effect.resend = self.dirty;
        effect
    }

    /// The hub's value replaces the shown one.
    pub fn apply_server(&mut self, db: f64, muted: bool) {
        self.db = db;
        self.muted = muted;
    }

    /// A poll's value for this strip: applied unless the strip is busy; `true` = applied.
    pub fn poll(&mut self, level: &Level) -> bool {
        if self.busy() {
            return false;
        }
        self.apply_server(level.db, level.muted);
        true
    }
}
