// The cutter's level sliders (intercom#34), served by the hub's loopback control listener.
//
// GET /api/levels builds one strip per slider; moving a slider or the mute button PUTs
// /api/levels/<name> {db, muted}. One request per strip is in flight at a time: a newer value that
// arrives meanwhile is sent right after it, so the hub always ends on the last position. A 1 s poll
// keeps the output meter live and picks up a change made from another window (never while this
// strip is being moved). The page never writes to the console: every failure is page state.
"use strict";

const POLL_MS = 1000;
const SEND_THROTTLE_MS = 120;
const METER_FLOOR_DB = -60;

const strips = new Map(); // name -> strip state
const statusEl = document.querySelector('[data-role="status"]');
const meterEl = document.querySelector('[data-role="meter"]');
const meterFill = document.querySelector('[data-role="meter-fill"]');
const meterValue = document.querySelector('[data-role="meter-value"]');

function setStatus(state, text) {
  statusEl.dataset.state = state;
  statusEl.textContent = text;
}

function fmtDb(db) {
  const v = Math.round(db * 10) / 10;
  return (v > 0 ? "+" : "") + v.toFixed(1) + " dB";
}

function render(s) {
  s.dbEl.textContent = s.muted ? "stlmené" : fmtDb(s.db);
  s.el.dataset.db = String(s.db);
  s.el.dataset.muted = String(s.muted);
  s.mute.setAttribute("aria-pressed", String(s.muted));
  s.mute.textContent = s.muted ? "Zapnúť" : "Stlmiť";
  s.el.classList.toggle("is-muted", s.muted);
}

function applyServer(s, level) {
  s.db = level.db;
  s.muted = level.muted;
  s.slider.value = String(level.db);
  render(s);
}

function busy(s) {
  return s.dragging || s.inflight || s.dirty;
}

async function send(s) {
  if (s.inflight) {
    s.dirty = true;
    return;
  }
  s.inflight = true;
  s.dirty = false;
  const body = JSON.stringify({ db: s.db, muted: s.muted });
  try {
    const r = await fetch("/api/levels/" + encodeURIComponent(s.name), {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body,
    });
    if (!r.ok) {
      setStatus("bad", "Hub zmenu odmietol: " + (await r.text()));
    } else {
      const answer = await r.json();
      s.el.dataset.saved = String(answer.saved);
      if (answer.saved) {
        setStatus("good", "Pripojené — uložené");
      } else {
        setStatus("warn", "Platí, ale neuložilo sa (po reštarte sa vráti)");
      }
      if (!s.dirty && !s.dragging) applyServer(s, answer);
    }
  } catch (e) {
    setStatus("bad", "Hub nedostupný — zmena sa neuložila");
  } finally {
    s.inflight = false;
  }
  if (s.dirty) send(s);
}

function scheduleSend(s) {
  if (s.timer) return;
  s.timer = setTimeout(() => {
    s.timer = null;
    send(s);
  }, SEND_THROTTLE_MS);
}

function buildStrip(level) {
  const tpl = document.getElementById("strip-template");
  const el = tpl.content.firstElementChild.cloneNode(true);
  const slider = el.querySelector('[data-role="slider"]');
  const s = {
    name: level.name,
    el,
    slider,
    dbEl: el.querySelector('[data-role="db"]'),
    mute: el.querySelector('[data-role="mute"]'),
    db: level.db,
    muted: level.muted,
    dragging: false,
    inflight: false,
    dirty: false,
    timer: null,
  };
  el.dataset.name = level.name;
  el.querySelector('[data-role="label"]').textContent = level.label;
  slider.min = String(level.min_db);
  slider.max = String(level.max_db);
  slider.value = String(level.db);
  slider.setAttribute("aria-label", level.label);
  slider.addEventListener("pointerdown", () => { s.dragging = true; });
  const release = () => { s.dragging = false; };
  slider.addEventListener("pointerup", release);
  slider.addEventListener("pointercancel", release);
  slider.addEventListener("input", () => {
    s.db = Number(slider.value);
    render(s);
    scheduleSend(s);
  });
  slider.addEventListener("change", () => {
    s.dragging = false;
    s.db = Number(slider.value);
    render(s);
    if (s.timer) {
      clearTimeout(s.timer);
      s.timer = null;
    }
    send(s);
  });
  s.mute.addEventListener("click", () => {
    s.muted = !s.muted;
    render(s);
    send(s);
  });
  render(s);
  return s;
}

function renderMeter(outputs) {
  const out = outputs && outputs[0];
  const db = out && typeof out.out_level_dbfs === "number" ? out.out_level_dbfs : null;
  if (db === null) {
    meterValue.textContent = "— dB";
    meterEl.dataset.db = "-120";
    meterFill.style.height = "0%";
    return;
  }
  const pct = Math.max(0, Math.min(100, ((db - METER_FLOOR_DB) / -METER_FLOOR_DB) * 100));
  meterEl.dataset.db = String(db);
  meterEl.dataset.hot = String(db > -3);
  meterFill.style.height = pct.toFixed(1) + "%";
  meterValue.textContent = db <= -119 ? "ticho" : Math.round(db) + " dB";
}

async function fetchLevels() {
  const r = await fetch("/api/levels", { cache: "no-store" });
  if (!r.ok) throw new Error("HTTP " + r.status);
  return r.json();
}

async function poll() {
  try {
    const body = await fetchLevels();
    for (const level of body.levels) {
      const s = strips.get(level.name);
      if (s && !busy(s)) applyServer(s, level);
    }
    renderMeter(body.outputs);
    if (statusEl.dataset.state !== "good" && statusEl.dataset.state !== "warn") {
      setStatus("good", "Pripojené");
    }
  } catch (e) {
    setStatus("bad", "Hub nedostupný — skúšam znova");
  }
  setTimeout(poll, POLL_MS);
}

async function start() {
  try {
    const body = await fetchLevels();
    const holder = document.querySelector('[data-role="strips"]');
    for (const level of body.levels) {
      const s = buildStrip(level);
      strips.set(level.name, s);
      holder.appendChild(s.el);
    }
    renderMeter(body.outputs);
    setStatus("good", "Pripojené");
  } catch (e) {
    setStatus("bad", "Hub nedostupný — skúšam znova");
    setTimeout(start, POLL_MS);
    return;
  }
  setTimeout(poll, POLL_MS);
}

start();
