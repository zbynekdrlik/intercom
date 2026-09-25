"use strict";
// Interkom phone PWA (issue 1345) — the phone UX rework (owner ruling 25.9.2026).
//
// Opening the link CONNECTS immediately: the page joins the Janus audiobridge room receive-only
// with the microphone OFF. There is no connect button. The screen answers three questions with
// LIVE data, never a static label:
//   1. Am I connected?          -> the connection bar (Pripojené / Pripájam… / Odpojené – skúšam
//                                  znova), with auto-reconnect and a capped backoff;
//   2. Does the cutter's voice arrive?  -> a WebAudio AnalyserNode meter on the received room audio;
//   3. Do they hear me?          -> one huge mic toggle with a live meter of my own mic.
// The mic permission is asked only on the FIRST mic ON; the session is then re-negotiated with
// send. The picture fills the width and a tap toggles fullscreen. The name is asked once and
// remembered; the mic device lives in a small settings sheet.
//
// Design: issue 1345 comment "Design (main, 25.9.2026) — redesign the phone interkom UX".
// This file NEVER logs to the console: every failure becomes visible page state
// (browser-console-zero-errors). The codec is whatever Janus negotiates — nothing here assumes one.

// ---- Config (all overridable for a dev host via the query string) ------------------------
const ROOM = numParam("room", 1000); // the Janus audiobridge room the intercom mixes into.
const HUB_POLL_MS = 3000; // /api/state poll (picture availability + the settings info lines).
const PICTURE_RETRY_MS = 5000; // retry the MJPEG picture while the hub is up but the picture is not.
const RECONNECT_BASE_MS = 1000; // first retry delay; doubles per failed attempt …
const RECONNECT_MAX_MS = 15000; // … up to this cap.
const CONNECT_TIMEOUT_MS = 15000; // a session that never gets media up is rebuilt.
const ICE_GRACE_MS = 5000; // a "disconnected" ICE state gets this long to recover by itself.
const ANSWER_TIMEOUT_MS = 10000; // an offer Janus has not answered by then rebuilds the session.
const METER_FLOOR_DB = -60; // the meters' left edge.
const VOICE_DB = -45; // above this the meter counts as "voice present".
const VOICE_HOLD_MS = 600; // keep "hovorí" this long after the level drops.
const METER_RELEASE_DB_PER_S = 30; // fall-back speed of the displayed level.
const NAME_KEY = "interkom.display";
const MIC_KEY = "interkom.micDevice";

function qs(name) {
  return new URLSearchParams(location.search).get(name);
}
function numParam(name, fallback) {
  const v = qs(name);
  const n = v == null ? NaN : Number(v);
  return Number.isFinite(n) ? n : fallback;
}

// The Janus WebSocket endpoint. PATH-RELATIVE by default: the TLS front proxies `/janus` on the
// SAME host to the Janus WS, so a phone never needs to know Janus's port. `?janus=<url>` overrides
// it for a dev host.
function janusWsUrl() {
  const override = qs("janus");
  if (override) return override;
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/janus`;
}

// ---- DOM handles -------------------------------------------------------------------------
const el = (sel) => document.querySelector(sel);
const role = (r) => el(`[data-role="${r}"]`);
const connEl = role("conn");
const connText = role("conn-text");
const connDetail = role("conn-detail");
const pictureWrap = role("picture-wrap");
const pictureImg = role("picture");
const picturePlaceholder = role("picture-placeholder");
const meterInEl = role("meter-in");
const meterInState = role("meter-in-state");
const incomingCard = meterInEl.closest(".incoming");
const micToggle = role("mic-toggle");
const micLabel = role("mic-label");
const micHint = role("mic-hint");
const meterMicEl = role("meter-mic");
const nameValue = role("name-value");
const nameShow = role("name-show");
const nameSheet = role("name-sheet");
const nameForm = role("name-form");
const nameInput = role("name");
const settingsSheet = role("settings");
const settingsOpen = role("settings-open");
const settingsClose = role("settings-close");
const nameChange = role("name-change");
const micSelect = role("mic-select");
const infoHub = role("info-hub");
const infoRoom = role("info-room");
const tapLayer = role("tap-for-sound");
const remoteAudio = role("remote-audio");
const versionEl = role("version");

// ---- Per-device storage (a convenience: private mode just forgets) ------------------------
function load(key) {
  try {
    return localStorage.getItem(key) || "";
  } catch (e) {
    return "";
  }
}
function store(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch (e) {
    // localStorage unavailable (private mode) — the value just won't persist on this device.
  }
}

// ---- 1. The connection indicator ---------------------------------------------------------
const CONN_TEXT = {
  connecting: "Pripájam…",
  connected: "Pripojené",
  reconnecting: "Odpojené – skúšam znova",
};
function setConn(state, detail) {
  connEl.dataset.state = state;
  connText.textContent = CONN_TEXT[state];
  connDetail.textContent = detail || "";
  renderMicHint();
}

// ---- Janus session (auto-join, reconnect with backoff) -----------------------------------
// A minimal webRTC "adapter" shim so we do NOT have to vendor webrtc-adapter too: janus.js only
// reads `browserDetails.{browser,version}` for its per-browser branches.
function detectBrowser() {
  const ua = navigator.userAgent;
  let browser = "chrome";
  let version = 0;
  if (/firefox\//i.test(ua)) {
    browser = "firefox";
    version = parseInt((ua.match(/firefox\/(\d+)/i) || [])[1] || "0", 10);
  } else if (/edg\//i.test(ua)) {
    version = parseInt((ua.match(/edg\/(\d+)/i) || [])[1] || "0", 10);
  } else if (/chrome\//i.test(ua)) {
    version = parseInt((ua.match(/chrome\/(\d+)/i) || [])[1] || "0", 10);
  } else if (/safari\//i.test(ua)) {
    browser = "safari";
    version = parseInt((ua.match(/version\/(\d+)/i) || [])[1] || "605", 10);
  }
  return { browser, version };
}
// Every Janus WebSocket goes through this wrapper so a torn-down session's socket can be closed
// even when janus.js never got its session id (its destroy() then returns without touching the
// socket, and a late connect would create a session nobody uses). Janus drops the sessions of a
// closed transport.
const NativeWebSocket = window.WebSocket;
const liveSockets = new Set();
function TrackedWebSocket(url, protocols) {
  const ws = new NativeWebSocket(url, protocols);
  liveSockets.add(ws);
  ws.addEventListener("close", () => liveSockets.delete(ws));
  ws.addEventListener("open", () => {
    // Abandoned while still connecting: close it once open (after janus.js's own "create" went
    // out — closing a CONNECTING socket makes the browser log an error).
    if (ws.abandoned) setTimeout(() => ws.close(), 0);
  });
  return ws;
}
function abandonSockets() {
  for (const ws of liveSockets) {
    ws.abandoned = true;
    if (ws.readyState === 1) ws.close();
  }
  liveSockets.clear();
}
function janusDeps() {
  return Janus.useDefaultDependencies({
    adapter: { browserDetails: detectBrowser() },
    WebSocket: TrackedWebSocket,
  });
}

let janusReady = false; // Janus.init has run
let gen = 0; // session generation: a callback from an older session is ignored
let janus = null; // the Janus session
let bridge = null; // the audiobridge plugin handle
let joined = false;
let mediaUp = false; // the PeerConnection is up (webrtcState true)
let sentTrack = null; // the mic track the current PeerConnection carries (null = receive-only)
let negotiating = false; // an offer is out and its answer has not been applied yet
let micPushPending = false; // a mic change waits for that answer
let answerTimer = null;
let reconnectAttempt = 0;
let reconnectTimer = null;
let reconnectAt = 0;
let watchdogTimer = null;
let iceTimer = null;

// Start (or restart) the session. Called at boot, by the reconnect timer, and on "come back".
function startSession() {
  clearTimeout(reconnectTimer);
  reconnectTimer = null;
  reconnectAt = 0;
  const my = ++gen;
  joined = false;
  mediaUp = false;
  sentTrack = null;
  negotiating = false;
  micPushPending = false;
  clearTimeout(answerTimer);
  if (reconnectAttempt > 0) setConn("reconnecting", "skúšam…");
  else setConn("connecting");
  clearTimeout(watchdogTimer);
  watchdogTimer = setTimeout(() => {
    if (my === gen && !mediaUp) scheduleReconnect();
  }, CONNECT_TIMEOUT_MS);
  const begin = () => {
    if (my !== gen) return;
    janus = new Janus({
      server: janusWsUrl(),
      dependencies: janusDeps(),
      success: () => {
        if (my === gen) attachBridge(my);
      },
      error: () => {
        if (my === gen) scheduleReconnect();
      },
      destroyed: () => {
        if (my === gen) scheduleReconnect();
      },
    });
  };
  if (janusReady) {
    begin();
  } else {
    // debug:false -> Janus.log/warn/error are all no-ops, so janus.js never writes to the console.
    Janus.init({
      debug: false,
      dependencies: janusDeps(),
      callback: () => {
        janusReady = true;
        begin();
      },
    });
  }
}

// Drop the current session silently: bump the generation first so none of its callbacks act.
function teardownSession() {
  gen += 1;
  clearTimeout(watchdogTimer);
  clearTimeout(iceTimer);
  const old = janus;
  janus = null;
  bridge = null;
  joined = false;
  mediaUp = false;
  sentTrack = null;
  negotiating = false;
  micPushPending = false;
  clearTimeout(answerTimer);
  if (old) {
    try {
      old.destroy({ cleanupHandles: true, notifyDestroyed: false });
    } catch (e) {
      // a destroy on a dead socket may throw — the session is gone either way.
    }
  }
  abandonSockets();
  setMeterTrack(meterIn, null); // the old room track is dead: "čakám…", never a stale "ticho"
}

function scheduleReconnect() {
  teardownSession();
  const delay = Math.min(RECONNECT_MAX_MS, RECONNECT_BASE_MS * 2 ** reconnectAttempt);
  reconnectAttempt += 1;
  reconnectAt = Date.now() + delay;
  renderReconnectCountdown();
  clearTimeout(reconnectTimer);
  reconnectTimer = setTimeout(startSession, delay);
}
function renderReconnectCountdown() {
  if (!reconnectAt) return;
  const s = Math.max(1, Math.ceil((reconnectAt - Date.now()) / 1000));
  setConn("reconnecting", `ďalší pokus o ${s} s`);
}
// A reason to try right now (network back, page visible again) short-circuits the backoff.
function reconnectNow() {
  if (connEl.dataset.state === "connected") return;
  if (!reconnectTimer && janus) return; // an attempt is already in flight
  teardownSession();
  startSession();
}

function onConnected() {
  clearTimeout(watchdogTimer);
  clearTimeout(iceTimer);
  reconnectAttempt = 0;
  setConn("connected");
}

function attachBridge(my) {
  janus.attach({
    plugin: "janus.plugin.audiobridge",
    success: (handle) => {
      if (my !== gen) return;
      bridge = handle;
      // Join MUTED, with the remembered name. The name can change later via `configure`.
      bridge.send({
        message: { request: "join", room: ROOM, display: displayName(), muted: true },
      });
    },
    error: () => {
      if (my === gen) scheduleReconnect();
    },
    onmessage: (msg, jsep) => {
      if (my === gen) onBridgeMessage(my, msg, jsep);
    },
    onremotetrack: (track, mid, on) => {
      if (my === gen && on && track.kind === "audio") attachRemote(track);
    },
    webrtcState: (on) => {
      if (my !== gen) return;
      if (on) {
        mediaUp = true;
        onConnected();
      } else {
        scheduleReconnect();
      }
    },
    iceState: (state) => {
      if (my !== gen) return;
      if (state === "failed" || state === "closed") {
        scheduleReconnect();
      } else if (state === "disconnected") {
        setConn("connecting", "sieť sa zasekla…");
        clearTimeout(iceTimer);
        iceTimer = setTimeout(() => {
          if (my === gen) scheduleReconnect();
        }, ICE_GRACE_MS);
      } else if (state === "connected" || state === "completed") {
        clearTimeout(iceTimer);
        if (mediaUp) onConnected();
      }
    },
    oncleanup: () => {
      if (my === gen && joined) scheduleReconnect();
    },
  });
}

function onBridgeMessage(my, msg, jsep) {
  const event = msg && msg.audiobridge;
  if (event === "joined" && !joined) {
    // Our own join reply (later "joined" events announce OTHER participants).
    joined = true;
    negotiate(my);
  } else if (event === "event" && msg.error && (!joined || negotiating)) {
    // A failed join (e.g. the room does not exist yet) or a rejected offer: rebuild, visibly. A
    // rejected mic renegotiation must never leave "Počujú ťa" on screen with nobody hearing us —
    // the rebuilt session offers WITH the mic from the start.
    scheduleReconnect();
  }
  if (jsep && bridge) {
    bridge.handleRemoteJsep({
      jsep,
      success: () => {
        if (my !== gen) return;
        negotiating = false;
        clearTimeout(answerTimer);
        if (micPushPending) {
          micPushPending = false;
          pushMicToSession(micOn);
        }
      },
      error: () => {
        if (my === gen) scheduleReconnect();
      },
    });
  }
}

// The session's first offer: receive-only unless the mic was already granted (a reconnect with
// the mic ON keeps sending it — no second permission prompt).
// An offer is out: mark it, and give Janus ANSWER_TIMEOUT_MS to answer before rebuilding.
function beginNegotiation(my) {
  negotiating = true;
  clearTimeout(answerTimer);
  answerTimer = setTimeout(() => {
    if (my === gen && negotiating) scheduleReconnect();
  }, ANSWER_TIMEOUT_MS);
}

function negotiate(my) {
  beginNegotiation(my);
  const mic = micTrack;
  const spec = mic
    ? { tracks: [{ type: "audio", capture: mic, recv: true, dontStop: true }] }
    : { tracks: [{ type: "audio", recv: true }] }; // receive-only: no getUserMedia at all
  bridge.createOffer({
    ...spec,
    success: (offer) => {
      if (my !== gen) return;
      sentTrack = mic;
      bridge.send({ message: { request: "configure", muted: !(micOn && mic) }, jsep: offer });
    },
    error: () => {
      if (my === gen) scheduleReconnect();
    },
  });
}

// ---- 2 + 3. Live level meters (WebAudio AnalyserNode) -----------------------------------
let audioCtx = null;
const meterIn = { el: meterInEl, track: null, source: null, analyser: null, buf: null, shown: -120, voiceUntil: 0 };
const meterMic = { el: meterMicEl, track: null, source: null, analyser: null, buf: null, shown: -120, voiceUntil: 0 };
let meterLoopRunning = false;
let meterLast = 0;

// The AudioContext is created only once the page may play sound (autoplay allowed, or a user
// gesture) — creating it earlier makes Chrome log an autoplay warning.
function ensureAudioCtx() {
  if (audioCtx) return audioCtx;
  const Ctx = window.AudioContext || window.webkitAudioContext;
  if (!Ctx) return null;
  audioCtx = new Ctx();
  wireMeter(meterIn);
  wireMeter(meterMic);
  return audioCtx;
}
function setMeterTrack(m, track) {
  m.track = track;
  wireMeter(m);
}
function wireMeter(m) {
  if (m.source) {
    try {
      m.source.disconnect();
    } catch (e) {
      // already disconnected
    }
  }
  m.source = null;
  m.analyser = null;
  if (!m.track || !audioCtx || m.track.readyState === "ended") {
    paintMeter(m, -120);
    return;
  }
  const source = audioCtx.createMediaStreamSource(new MediaStream([m.track]));
  const analyser = audioCtx.createAnalyser();
  analyser.fftSize = 1024;
  analyser.smoothingTimeConstant = 0;
  source.connect(analyser); // analysed only — never routed to the speakers (no double playback)
  m.source = source;
  m.analyser = analyser;
  m.buf = new Float32Array(analyser.fftSize);
  if (!meterLoopRunning) {
    meterLoopRunning = true;
    meterLast = 0;
    requestAnimationFrame(meterFrame);
  }
}
function measureDb(m) {
  m.analyser.getFloatTimeDomainData(m.buf);
  let sum = 0;
  for (let i = 0; i < m.buf.length; i += 1) sum += m.buf[i] * m.buf[i];
  const rms = Math.sqrt(sum / m.buf.length);
  return rms > 0 ? Math.max(-120, 20 * Math.log10(rms)) : -120;
}
function paintMeter(m, db) {
  m.shown = db;
  const pct = Math.min(1, Math.max(0, (db - METER_FLOOR_DB) / -METER_FLOOR_DB)) * 100;
  m.el.style.setProperty("--level", `${pct.toFixed(1)}%`);
  m.el.dataset.db = String(Math.round(db));
  const now = performance.now();
  if (db > VOICE_DB) m.voiceUntil = now + VOICE_HOLD_MS;
  const voice = now < m.voiceUntil;
  m.el.dataset.active = voice ? "true" : "false";
  if (m === meterIn) {
    incomingCard.dataset.voice = voice ? "true" : "false";
    if (!audioCtx || !m.analyser) meterInState.textContent = m.track ? "ťukni pre zvuk" : "čakám…";
    else meterInState.textContent = voice ? "hovorí" : "ticho";
  }
}
function meterFrame(now) {
  const dt = meterLast ? Math.min(0.25, (now - meterLast) / 1000) : 0;
  meterLast = now;
  let any = false;
  for (const m of [meterIn, meterMic]) {
    if (!m.analyser) continue;
    any = true;
    const db = measureDb(m);
    // Fast attack, steady release: a short word stays readable on the bar.
    paintMeter(m, db >= m.shown ? db : Math.max(db, m.shown - METER_RELEASE_DB_PER_S * dt));
  }
  if (any) requestAnimationFrame(meterFrame);
  else meterLoopRunning = false;
}

// ---- Incoming audio + the autoplay unlock ------------------------------------------------
let audioUnlocked = false;
function attachRemote(track) {
  remoteAudio.srcObject = new MediaStream([track]);
  setMeterTrack(meterIn, track);
  playRemote();
}
function playRemote() {
  let p;
  try {
    p = remoteAudio.play();
  } catch (e) {
    p = null;
  }
  if (p && typeof p.then === "function") {
    p.then(onAudioUnlocked, () => {
      // Autoplay blocked: the connection is up, the sound waits for the first touch.
      if (!audioUnlocked) tapLayer.hidden = false;
      paintMeter(meterIn, -120);
    });
  } else {
    onAudioUnlocked();
  }
}
// play() succeeding means the page may make sound; Chrome and Safari apply the same rule to an
// AudioContext. Where the browser can say so explicitly, ask it first.
function audioContextAllowed() {
  if (navigator.userActivation && navigator.userActivation.hasBeenActive) return true;
  if (typeof navigator.getAutoplayPolicy === "function") {
    try {
      return navigator.getAutoplayPolicy("audiocontext") === "allowed";
    } catch (e) {
      // an older signature — fall through
    }
  }
  return true;
}
function onAudioUnlocked() {
  audioUnlocked = true;
  tapLayer.hidden = true;
  const ctx = audioContextAllowed() ? ensureAudioCtx() : null;
  if (ctx && ctx.state === "suspended") ctx.resume().catch(() => {});
}
// Any real user gesture (click / touchend / key) may start sound: play the room audio if it is
// paused, and create or resume the AudioContext for the meters.
function unlockAudio() {
  const ctx = ensureAudioCtx();
  if (ctx && ctx.state === "suspended") ctx.resume().catch(() => {});
  if (remoteAudio.srcObject && remoteAudio.paused) playRemote();
  else if (remoteAudio.srcObject) onAudioUnlocked();
}
function onUserGesture(e) {
  if (e.type === "keydown" && e.key === "Escape") return; // Escape does not activate the page
  unlockAudio();
}
for (const type of ["click", "touchend", "keydown"]) {
  document.addEventListener(type, onUserGesture, { capture: true, passive: true });
}
tapLayer.addEventListener("click", () => {
  tapLayer.hidden = true;
});

// ---- 3. My microphone (asked for only on the first ON) ---------------------------------------
let micTrack = null; // the granted mic track; kept (enabled/disabled) so re-enabling never re-asks
let micOn = false;

const MIC_UI = {
  off: ["Mikrofón vypnutý", "Ťukni pre zapnutie"],
  pending: ["Povoľ mikrofón…", "Prehliadač sa pýta na povolenie"],
  on: ["MIKROFÓN ZAPNUTÝ", "Počujú ťa — ťukni pre vypnutie"],
  denied: ["Mikrofón nie je povolený", "Povoľ ho v nastaveniach prehliadača a ťukni znova"],
};
const MIC_ON_OFFLINE_HINT = "Zapnutý — čakám na spojenie";
function setMicUi(state) {
  micToggle.dataset.state = state;
  micToggle.dataset.muted = state === "on" ? "false" : "true";
  micToggle.setAttribute("aria-pressed", state === "on" ? "true" : "false");
  micLabel.textContent = MIC_UI[state][0];
  meterMicEl.hidden = state !== "on";
  renderMicHint();
}
// "Počujú ťa" only while the session is really up; ON but offline says so.
function renderMicHint() {
  const state = micToggle.dataset.state || "off";
  if (state === "on" && connEl.dataset.state !== "connected") micHint.textContent = MIC_ON_OFFLINE_HINT;
  else micHint.textContent = MIC_UI[state][1];
}

function micConstraints(deviceId) {
  const audio = { echoCancellation: true, noiseSuppression: true, autoGainControl: true };
  if (deviceId) audio.deviceId = { exact: deviceId };
  return { audio };
}
// The ONE place this page asks for the microphone.
async function acquireMic(deviceId) {
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    throw new Error("microphone unavailable in this context");
  }
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia(micConstraints(deviceId));
  } catch (e) {
    // A remembered device may be gone: fall back to the default mic once.
    if (!deviceId || (e && e.name !== "OverconstrainedError" && e.name !== "NotFoundError")) throw e;
    micSelect.value = "";
    store(MIC_KEY, "");
    return acquireMic("");
  }
  const track = stream.getAudioTracks()[0];
  track.dontStop = true; // janus.js must never stop it on a session teardown — it is ours
  track.addEventListener("ended", () => onMicEnded(track));
  const old = micTrack;
  micTrack = track;
  if (old && old !== track) old.stop();
  refreshMics();
  return track;
}
function onMicEnded(track) {
  if (track !== micTrack) return;
  micTrack = null;
  setMeterTrack(meterMic, null);
  if (micOn) {
    micOn = false;
    setMicUi("off");
    sendMute(true);
  }
}

function sendMute(muted) {
  if (bridge && joined) bridge.send({ message: { request: "configure", muted } });
}

// The audio transceiver of the live PeerConnection (the receive-only offer created it).
function audioTransceiver() {
  const pc = bridge && bridge.webrtcStuff && bridge.webrtcStuff.pc;
  if (!pc) return null;
  return pc.getTransceivers().find((t) => t.receiver && t.receiver.track && t.receiver.track.kind === "audio") || null;
}

// Put the current mic into the live session WITHOUT rebuilding it. The track goes straight onto
// the audio transceiver's sender: janus.js 1.1.2's own replace path (createOffer replace:true /
// replaceTracks) dereferences its null local stream after a receive-only offer and throws. On the
// FIRST ON the transceiver turns recvonly -> sendrecv and janus.js only creates the renegotiation
// offer (tracks: [] = "nothing to capture"); a later device change is a plain sender swap with no
// renegotiation. While an offer is out, the change waits for its answer.
function pushMicToSession(unmute) {
  if (!bridge || !joined || !micTrack) return;
  if (negotiating) {
    micPushPending = true;
    return;
  }
  const tr = audioTransceiver();
  if (!tr) {
    scheduleReconnect(); // no PeerConnection to put the mic on — the rebuilt session offers WITH it
    return;
  }
  const my = gen;
  const track = micTrack;
  if (!sentTrack) {
    beginNegotiation(my);
    tr.sender.replaceTrack(track).then(
      () => {
        if (my !== gen) return;
        if (tr.setDirection) tr.setDirection("sendrecv");
        else tr.direction = "sendrecv";
        bridge.createOffer({
          tracks: [],
          success: (offer) => {
            if (my !== gen) return;
            sentTrack = track;
            bridge.send({ message: { request: "configure", muted: !micOn }, jsep: offer });
          },
          error: () => {
            if (my === gen) scheduleReconnect();
          },
        });
      },
      () => {
        if (my === gen) scheduleReconnect();
      }
    );
    return;
  }
  if (sentTrack !== track) {
    tr.sender.replaceTrack(track).then(
      () => {
        if (my === gen) sentTrack = track;
      },
      () => {
        if (my === gen) scheduleReconnect();
      }
    );
  }
  if (unmute) sendMute(false);
}

async function turnMicOn() {
  if (!micTrack) {
    setMicUi("pending");
    try {
      await acquireMic(micSelect.value || load(MIC_KEY));
    } catch (e) {
      setMicUi("denied"); // the page keeps listening; the next tap asks again
      return;
    }
  }
  micOn = true;
  micTrack.enabled = true;
  setMicUi("on");
  setMeterTrack(meterMic, micTrack);
  pushMicToSession(true);
}
function turnMicOff() {
  micOn = false;
  if (micTrack) micTrack.enabled = false; // belt and braces: silence even before Janus mutes
  setMicUi("off");
  setMeterTrack(meterMic, null);
  sendMute(true);
}
micToggle.addEventListener("click", () => {
  if (micToggle.dataset.state === "pending") return;
  if (micOn) turnMicOff();
  else turnMicOn();
});

// ---- Settings: the mic device --------------------------------------------------------------
async function refreshMics() {
  if (!navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) return;
  try {
    const devs = await navigator.mediaDevices.enumerateDevices();
    const mics = devs.filter((d) => d.kind === "audioinput" && d.deviceId && d.deviceId !== "default");
    const current = micSelect.value || load(MIC_KEY);
    while (micSelect.options.length > 1) micSelect.remove(1);
    let i = 1;
    for (const m of mics) {
      const opt = document.createElement("option");
      opt.value = m.deviceId;
      opt.textContent = m.label || `Mikrofón ${i}`;
      micSelect.appendChild(opt);
      i += 1;
    }
    if (current && mics.some((m) => m.deviceId === current)) micSelect.value = current;
  } catch (e) {
    // enumerateDevices can reject on a locked-down origin — the default option stays.
  }
}
micSelect.addEventListener("change", async () => {
  store(MIC_KEY, micSelect.value);
  if (!micTrack) return; // used on the next mic ON
  try {
    await acquireMic(micSelect.value);
  } catch (e) {
    setMicUi("denied");
    micOn = false;
    sendMute(true);
    return;
  }
  micTrack.enabled = micOn;
  if (micOn) setMeterTrack(meterMic, micTrack);
  pushMicToSession(micOn);
});

function openSettings() {
  refreshMics();
  settingsSheet.hidden = false;
}
settingsOpen.addEventListener("click", openSettings);
settingsClose.addEventListener("click", () => {
  settingsSheet.hidden = true;
});
settingsSheet.addEventListener("click", (e) => {
  if (e.target === settingsSheet) settingsSheet.hidden = true;
});

// ---- The name (asked once, remembered) ---------------------------------------------------
function displayName() {
  return load(NAME_KEY).trim() || "Kameraman";
}
function showName() {
  nameValue.textContent = displayName();
}
function openNameSheet() {
  settingsSheet.hidden = true;
  nameInput.value = load(NAME_KEY);
  nameSheet.hidden = false;
  nameInput.focus();
}
nameForm.addEventListener("submit", (e) => {
  e.preventDefault();
  const name = nameInput.value.trim();
  if (!name) {
    nameInput.focus();
    return;
  }
  store(NAME_KEY, name);
  nameSheet.hidden = true;
  showName();
  if (bridge && joined) bridge.send({ message: { request: "configure", display: name } });
});
nameShow.addEventListener("click", openNameSheet);
nameChange.addEventListener("click", openNameSheet);

// ---- The picture: full width, tap = fullscreen ---------------------------------------------
let hubReachable = false;
function showPicture(on) {
  pictureImg.hidden = !on;
  picturePlaceholder.hidden = on;
}
function tryPicture() {
  // Only ATTEMPT the picture once the hub is reachable — a bare <img src> to a missing route
  // would log a 404 to the console. While the hub is down we stay on the placeholder, silently.
  if (!hubReachable || !pictureImg.hidden) return;
  pictureImg.src = `/interkom.mjpeg?t=${Date.now()}`;
}
pictureImg.addEventListener("load", () => showPicture(true));
pictureImg.addEventListener("error", () => {
  // The stream ended (the hub closes a stale stream after 5 s) or is not there yet: placeholder,
  // and the retry timer tries again.
  showPicture(false);
});
setInterval(() => {
  if (hubReachable && pictureImg.hidden) tryPicture();
}, PICTURE_RETRY_MS);

function fullscreenElement() {
  return document.fullscreenElement || document.webkitFullscreenElement || null;
}
function syncFullscreen() {
  const on = fullscreenElement() === pictureWrap || pictureWrap.classList.contains("is-max");
  pictureWrap.dataset.fullscreen = on ? "true" : "false";
}
function maximise(on) {
  pictureWrap.classList.toggle("is-max", on);
  syncFullscreen();
}
function lockLandscape() {
  const o = screen.orientation;
  if (o && typeof o.lock === "function") o.lock("landscape").catch(() => {});
}
function enterFullscreen() {
  const req = pictureWrap.requestFullscreen || pictureWrap.webkitRequestFullscreen;
  if (!req) {
    maximise(true); // iPhone Safari: no element fullscreen — a CSS maximised overlay instead
    return;
  }
  let p;
  try {
    p = req.call(pictureWrap);
  } catch (e) {
    maximise(true);
    return;
  }
  if (p && typeof p.then === "function") p.then(lockLandscape, () => maximise(true));
  else lockLandscape();
}
function exitFullscreen() {
  if (pictureWrap.classList.contains("is-max")) {
    maximise(false);
    return;
  }
  const exit = document.exitFullscreen || document.webkitExitFullscreen;
  if (!exit) return;
  try {
    const p = exit.call(document);
    if (p && typeof p.catch === "function") p.catch(() => {});
  } catch (e) {
    // not in fullscreen any more
  }
}
function toggleFullscreen() {
  if (pictureWrap.dataset.fullscreen === "true") exitFullscreen();
  else if (!pictureImg.hidden) enterFullscreen(); // nothing to enlarge while there is no picture
}
pictureWrap.addEventListener("click", toggleFullscreen);
pictureWrap.addEventListener("keydown", (e) => {
  if (e.key === "Enter" || e.key === " ") {
    e.preventDefault();
    toggleFullscreen();
  }
});
document.addEventListener("fullscreenchange", syncFullscreen);
document.addEventListener("webkitfullscreenchange", syncFullscreen);

// ---- Hub info (fetch poll — never a WS, so an unreachable hub logs nothing) ---------------------
async function pollHub() {
  try {
    const r = await fetch("/api/state", { cache: "no-store" });
    if (!r.ok) throw new Error(`http ${r.status}`);
    const st = await r.json();
    hubReachable = true;
    const n = Array.isArray(st.participants) ? st.participants.length : "?";
    infoHub.textContent = "beží";
    infoRoom.textContent = `${n} účastníkov`;
    tryPicture();
  } catch (e) {
    hubReachable = false;
    infoHub.textContent = "nedostupný";
    infoRoom.textContent = "—";
  }
}
async function refreshVersion() {
  try {
    const r = await fetch("/api/version", { cache: "no-store" });
    if (!r.ok) return;
    const v = await r.json();
    if (v && v.version) versionEl.textContent = `v${v.version}`;
  } catch (e) {
    // the {{VERSION}} the hub injected into the DOM stays as-is; not fatal.
  }
}

// ---- Coming back (network restored, screen on again) ---------------------------------------
window.addEventListener("online", reconnectNow);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState !== "visible") return;
  if (audioCtx && audioCtx.state === "suspended") audioCtx.resume().catch(() => {});
  reconnectNow();
});
setInterval(renderReconnectCountdown, 500);

// ---- PWA service worker (installable; pure passthrough, no cache — server-truth) ------------
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

// ---- Boot ----------------------------------------------------------------------------------
setMicUi("off");
showName();
paintMeter(meterIn, -120);
if (!load(NAME_KEY)) openNameSheet(); // asked once; the connection does not wait for it
startSession(); // auto-join, receive-only, mic OFF
refreshVersion();
pollHub();
setInterval(pollHub, HUB_POLL_MS);
