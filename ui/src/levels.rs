//! The cutters' level mixer (intercom#34/#38), on Leptos since intercom#48.
//!
//! `GET /api/levels` builds one channel strip per slider (one per source, the master last and set
//! apart: it trims the whole output on top of the others); moving a fader or pressing mute PUTs
//! `/api/levels/<name>` `{db, muted}`. One request per strip is in flight at a time and a newer value
//! follows it, so the hub always ends on the last position ([`StripState`]). A 1 s poll keeps the
//! output meter live and picks up a change made from another window (never while that strip is
//! being moved). The page never writes to the console: every failure is the status line.
//!
//! The fader is a horizontal range turned -90deg inside an upright 40x260 box (`levels.css`).
//! NEVER `writing-mode: vertical-lr` or `appearance: slider-vertical`: the tray's WebKitGTK window
//! draws the first as a tiny horizontal slider, Chromium warns about the second (intercom#38/#39).

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::logic::{
    js_num, level_path, meter_view, mute_text, parse_levels, parse_put_answer, put_body,
    status_after_poll, strip_db_text, Level, LevelsBody, MeterView, SendOutcome, Status,
    StripState, POLL_MS, SEND_THROTTLE_MS,
};
use crate::net;

/// One strip's live state: the send bookkeeping and the signals its view shows.
#[derive(Clone, Copy)]
pub struct StripHandle {
    pub state: StoredValue<StripState>,
    /// The value shown in the dB line and `data-db`.
    pub db: RwSignal<f64>,
    pub muted: RwSignal<bool>,
    /// `data-saved` after the hub answered a change.
    pub saved: RwSignal<Option<bool>>,
    /// The range's position as the PAGE sets it (at build and from the hub's values); a drag moves
    /// the range itself and never writes this back.
    pub pos: RwSignal<f64>,
    /// The scheduled throttled send.
    pub timer: StoredValue<Option<TimeoutHandle>>,
}

impl StripHandle {
    pub fn new(level: &Level) -> Self {
        StripHandle {
            state: StoredValue::new(StripState::new(level)),
            db: RwSignal::new(level.db),
            muted: RwSignal::new(level.muted),
            saved: RwSignal::new(None),
            pos: RwSignal::new(level.db),
            timer: StoredValue::new(None),
        }
    }

    /// Show the state's value (the old page's `render`).
    fn render(self) {
        if let Some((db, muted)) = self.state.try_with_value(|s| (s.db, s.muted)) {
            self.db.set(db);
            self.muted.set(muted);
        }
    }

    /// The hub's value: the state, the view and the range's position.
    pub fn apply_server(self, db: f64, muted: bool) {
        self.state.update_value(|s| s.apply_server(db, muted));
        self.render();
        self.pos.set(db);
    }
}

/// What the user did on a strip.
#[derive(Debug, Clone, PartialEq)]
pub enum StripEvent {
    PointerDown,
    PointerUp,
    /// The range moved (its value).
    Input(String),
    /// The range was let go or stepped (its value).
    Change(String),
    Mute,
}

/// What must go to the hub after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendAction {
    Nothing,
    /// The throttled send (120 ms).
    Schedule,
    /// Send now; cancel a scheduled throttled send first when `cancel`.
    Now {
        cancel: bool,
    },
}

/// Apply one event to a strip's state and view; the caller does the network part.
pub fn reduce(h: StripHandle, ev: &StripEvent) -> SendAction {
    let action = h
        .state
        .try_update_value(|s| match ev {
            StripEvent::PointerDown => {
                s.pointer_down();
                SendAction::Nothing
            }
            StripEvent::PointerUp => {
                s.pointer_up();
                SendAction::Nothing
            }
            StripEvent::Input(v) => {
                if s.input(v) {
                    SendAction::Schedule
                } else {
                    SendAction::Nothing
                }
            }
            StripEvent::Change(v) => SendAction::Now {
                cancel: s.change(v),
            },
            StripEvent::Mute => {
                s.toggle_mute();
                SendAction::Now { cancel: false }
            }
        })
        .unwrap_or(SendAction::Nothing);
    if !matches!(ev, StripEvent::PointerDown | StripEvent::PointerUp) {
        h.render();
    }
    action
}

/// The page's shared state.
#[derive(Clone, Copy)]
pub struct Mixer {
    pub status: RwSignal<Status>,
    pub meter: RwSignal<MeterView>,
    /// `data-hot` of the meter (kept when a reading is missing, like the old page).
    pub hot: RwSignal<Option<bool>>,
    pub strips: RwSignal<Vec<(Level, StripHandle)>>,
}

impl Mixer {
    pub fn new() -> Self {
        Mixer {
            status: RwSignal::new(Status::loading()),
            meter: RwSignal::new(meter_view(&[])),
            hot: RwSignal::new(None),
            strips: RwSignal::new(Vec::new()),
        }
    }

    fn show_meter(self, body: &LevelsBody) {
        let view = meter_view(&body.outputs);
        if let Some(hot) = view.hot {
            self.hot.set(Some(hot));
        }
        self.meter.set(view);
    }

    /// Handle one strip event: reduce it, then schedule or send.
    pub fn on_event(self, h: StripHandle, ev: StripEvent) {
        match reduce(h, &ev) {
            SendAction::Nothing => {}
            SendAction::Schedule => {
                let fire = move || {
                    h.timer.set_value(None);
                    h.state.update_value(|s| s.timer_fired());
                    self.send(h);
                };
                if let Ok(handle) = set_timeout_with_handle(
                    fire,
                    Duration::from_millis(u64::from(SEND_THROTTLE_MS)),
                ) {
                    h.timer.set_value(Some(handle));
                }
            }
            SendAction::Now { cancel } => {
                if cancel {
                    if let Some(t) = h.timer.try_update_value(Option::take).flatten() {
                        t.clear();
                    }
                }
                self.send(h);
            }
        }
    }

    /// PUT the strip's value (or mark it to follow the one in flight).
    fn send(self, h: StripHandle) {
        let Some(body) = h.state.try_update_value(|s| s.begin_send()).flatten() else {
            return;
        };
        let path = h.state.with_value(|s| level_path(&s.name));
        spawn_local(async move {
            let outcome = match net::put_json(&path, &put_body(body.db, body.muted)).await {
                Ok(a) if a.ok => match parse_put_answer(&a.text) {
                    Ok(answer) => SendOutcome::Answer(answer),
                    Err(_) => SendOutcome::Failed,
                },
                Ok(a) => SendOutcome::Refused(a.text),
                Err(()) => SendOutcome::Failed,
            };
            let Some(effect) = h.state.try_update_value(|s| s.finish_send(outcome)) else {
                return;
            };
            if let Some(saved) = effect.saved {
                h.saved.set(Some(saved));
            }
            self.status.set(effect.status);
            if let Some((db, muted)) = effect.apply {
                h.apply_server(db, muted);
            }
            if effect.resend {
                self.send(h);
            }
        });
    }

    /// Load the sliders and build the strips; retry every second until the hub answers.
    pub fn start(self) {
        spawn_local(async move {
            match fetch_levels().await {
                Ok(body) => {
                    let strips = body
                        .levels
                        .iter()
                        .map(|l| (l.clone(), StripHandle::new(l)))
                        .collect();
                    self.strips.set(strips);
                    self.show_meter(&body);
                    self.status.set(Status::connected());
                    self.schedule_poll();
                }
                Err(()) => {
                    self.status.set(Status::unreachable());
                    set_timeout(
                        move || self.start(),
                        Duration::from_millis(u64::from(POLL_MS)),
                    );
                }
            }
        });
    }

    fn schedule_poll(self) {
        set_timeout(
            move || spawn_local(self.poll()),
            Duration::from_millis(u64::from(POLL_MS)),
        );
    }

    async fn poll(self) {
        match fetch_levels().await {
            Ok(body) => {
                let strips = self.strips.get_untracked();
                for level in &body.levels {
                    if let Some((_, h)) = strips.iter().find(|(l, _)| l.name == level.name) {
                        let applied = h.state.try_update_value(|s| s.poll(level)).unwrap_or(false);
                        if applied {
                            h.apply_server(level.db, level.muted);
                        }
                    }
                }
                self.show_meter(&body);
                if let Some(s) = status_after_poll(self.status.get_untracked().state) {
                    self.status.set(s);
                }
            }
            Err(()) => self.status.set(Status::unreachable()),
        }
        self.schedule_poll();
    }
}

impl Default for Mixer {
    fn default() -> Self {
        Mixer::new()
    }
}

async fn fetch_levels() -> Result<LevelsBody, ()> {
    let answer = net::get_fresh("/api/levels").await?;
    if !answer.ok {
        return Err(());
    }
    parse_levels(&answer.text).map_err(|_| ())
}

/// One channel strip: its dB, the upright fader, its name and the mute button.
#[component]
pub fn StripView(
    level: Level,
    handle: StripHandle,
    on_event: Callback<StripEvent>,
) -> impl IntoView {
    let h = handle;
    let master = level.is_master();
    let label = level.label.clone();
    let min = js_num(level.min_db);
    let max = js_num(level.max_db);
    view! {
        <div
            class="strip"
            class:is-master=master
            class:is-muted=move || h.muted.get()
            data-role="strip"
            data-name=level.name.clone()
            data-master=master.to_string()
            data-db=move || js_num(h.db.get())
            data-muted=move || h.muted.get().to_string()
            data-saved=move || h.saved.get().map(|s| s.to_string())
        >
            <div class="strip-db" data-role="db">
                {move || strip_db_text(h.db.get(), h.muted.get())}
            </div>
            <div class="strip-fader">
                <input
                    class="strip-slider"
                    data-role="slider"
                    type="range"
                    step="0.5"
                    min=min
                    max=max
                    aria-label=label.clone()
                    prop:value=move || js_num(h.pos.get())
                    on:pointerdown=move |_| on_event.run(StripEvent::PointerDown)
                    on:pointerup=move |_| on_event.run(StripEvent::PointerUp)
                    on:pointercancel=move |_| on_event.run(StripEvent::PointerUp)
                    on:input=move |ev| on_event.run(StripEvent::Input(event_target_value(&ev)))
                    on:change=move |ev| on_event.run(StripEvent::Change(event_target_value(&ev)))
                />
            </div>
            <div class="strip-label" data-role="label">
                {label}
            </div>
            <button
                class="strip-mute"
                data-role="mute"
                type="button"
                aria-pressed=move || h.muted.get().to_string()
                on:click=move |_| on_event.run(StripEvent::Mute)
            >
                {move || mute_text(h.muted.get())}
            </button>
        </div>
    }
}

/// The output meter: the cutters' peak after the sliders.
#[component]
pub fn Meter(meter: RwSignal<MeterView>, hot: RwSignal<Option<bool>>) -> impl IntoView {
    view! {
        <section class="meter-wrap" aria-label="Výstup do slúchadiel">
            <div
                class="meter"
                data-role="meter"
                data-db=move || meter.get().db_attr
                data-hot=move || hot.get().map(|h| h.to_string())
            >
                <div
                    class="meter-fill"
                    data-role="meter-fill"
                    style:height=move || meter.get().height
                ></div>
            </div>
            <div class="meter-label">"Výstup"</div>
            <div class="meter-value" data-role="meter-value">
                {move || meter.get().text}
            </div>
        </section>
    }
}

/// The mixer page: the header with the hub's version, the status line, the strips, the meter.
#[component]
pub fn LevelsPage(version: String) -> impl IntoView {
    let mixer = Mixer::new();
    mixer.start();
    view! { <LevelsBoard mixer=mixer version=version /> }
}

/// The page's markup over a [`Mixer`] (the tests drive one without a hub).
#[component]
pub fn LevelsBoard(mixer: Mixer, version: String) -> impl IntoView {
    let on_event = move |h: StripHandle| Callback::new(move |ev: StripEvent| mixer.on_event(h, ev));
    view! {
        <header class="top">
            <h1>"Hlasitosti v slúchadlách"</h1>
            <span class="version" data-role="version">
                {format!("v{version}")}
            </span>
        </header>
        <p
            class="status"
            data-role="status"
            data-state=move || mixer.status.get().state.as_str()
            role="status"
            aria-live="polite"
        >
            {move || mixer.status.get().text}
        </p>
        <main class="board">
            <section class="strips" data-role="strips" aria-label="Mixér">
                {move || {
                    mixer
                        .strips
                        .get()
                        .into_iter()
                        .map(|(level, h)| {
                            view! { <StripView level=level handle=h on_event=on_event(h) /> }
                        })
                        .collect_view()
                }}
            </section>
            <Meter meter=mixer.meter hot=mixer.hot />
        </main>
        <p class="hint">
            "Každý zdroj má svoj posuvník, Master stíši alebo zosilní celý výstup. 0 dB = pôvodné nastavenie. Zmena platí hneď a ostane aj po reštarte."
        </p>
    }
}
