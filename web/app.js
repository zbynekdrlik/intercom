"use strict";
// Interkom phone PWA (issue 1345 M3b). A cameraman opens ONE link, taps "Pripojiť" (the single
// autoplay + getUserMedia gesture), immediately HEARS the intercom and SEES the Interkom picture,
// and can (rarely) pick a mic and unmute it. Audio is WebRTC via a vendored janus.js talking to
// the Janus audiobridge; the picture is a dumb MJPEG pipe from the hub (M3c / issue 1347).
//
// Design: issue 1345 comment "Design (main, 19.9.2026)" (Prístup 1). This file NEVER logs to the
// console on a handled failure — every hub/Janus/picture problem is shown as a status CHIP, so the
// browser console stays clean (browser-console-zero-errors) even when the hub is unreachable.

// ---- Config (all overridable for a dev host via the query string) --------------------------
const ROOM = numParam("room", 1000); // the Janus audiobridge room the intercom mixes into.
const HUB_POLL_MS = 2000; // /api/state poll cadence for the hub chip.
const PICTURE_RETRY_MS = 5000; // retry the MJPEG picture while the hub is up but the picture is not.

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
// it for a dev host (e.g. a direct ws://strih-lx.lan:8188/janus).
function janusWsUrl() {
  const override = qs("janus");
  if (override) return override;
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/janus`;
}

// ---- DOM handles ---------------------------------------------------------------------------
const el = (sel) => document.querySelector(sel);
const connectBtn = el('[data-role="connect"]');
const micSelect = el('[data-role="mic-select"]');
const micToggle = el('[data-role="mic-toggle"]');
const nameInput = el('[data-role="name"]');
const pictureImg = el('[data-role="picture"]');
const picturePlaceholder = el('[data-role="picture-placeholder"]');
const remoteAudio = el('[data-role="remote-audio"]');
const versionEl = el('[data-role="version"]');
const chipHub = el('[data-role="chip-hub"]');
const chipJanus = el('[data-role="chip-janus"]');
const chipAudio = el('[data-role="chip-audio"]');
const chipPicture = el('[data-role="chip-picture"]');

function chip(node, text, state) {
  if (!node) return;
  node.textContent = text;
  node.dataset.state = state;
}

// ---- Runtime state -------------------------------------------------------------------------
let hubReachable = false;
let janus = null; // the Janus session
let bridge = null; // the audiobridge plugin handle
let joined = false;
let muted = true; // the mic starts OFF (design: joins MUTED)
let listenOnly = false; // set when we joined recv-only (no mic / permission denied)
let lastAudioBytes = 0;
let lastFrameAt = 0;
let statsTimer = null;

// The display name is typed once and remembered per device.
const NAME_KEY = "interkom.display";
try {
  const saved = localStorage.getItem(NAME_KEY);
  if (saved) nameInput.value = saved;
} catch (e) {
  // localStorage unavailable (private mode) — a per-device convenience, safe to skip.
}
nameInput.addEventListener("input", () => {
  try {
    localStorage.setItem(NAME_KEY, nameInput.value.trim());
  } catch (e) {
    // ignore — the name just won't persist on this device.
  }
});
function displayName() {
  return (nameInput.value || "").trim() || "Kameraman";
}

// ---- Hub status chip + version (fetch poll, never a WS — keeps the console clean offline) ----
async function pollHub() {
  try {
    const r = await fetch("/api/state", { cache: "no-store" });
    if (!r.ok) throw new Error("http " + r.status);
    const st = await r.json();
    hubReachable = true;
    const n = Array.isArray(st.participants) ? st.participants.length : "?";
    chip(chipHub, `Hub: OK (${n})`, "ok");
    tryPicture();
  } catch (e) {
    hubReachable = false;
    chip(chipHub, "Hub: nedostupný", "bad");
  }
}

async function refreshVersion() {
  try {
    const r = await fetch("/api/version", { cache: "no-store" });
    if (!r.ok) return;
    const v = await r.json();
    if (v && v.version && versionEl) versionEl.textContent = "v" + v.version;
  } catch (e) {
    // the {{VERSION}} the hub injected into the DOM stays as-is; not fatal.
  }
}

// ---- The MJPEG Interkom picture (M3c serves /interkom.mjpeg; until then: placeholder + retry) ----
function showPicture(on) {
  if (pictureImg) pictureImg.hidden = !on;
  if (picturePlaceholder) picturePlaceholder.hidden = on;
}
function tryPicture() {
  // Only ATTEMPT the picture once the hub is reachable — a bare <img src> to a missing route
  // would log a 404 to the console. While the hub is down we stay on the placeholder, silently.
  if (!hubReachable) return;
  if (pictureImg && !pictureImg.hidden) return; // already streaming
  if (pictureImg) pictureImg.src = `/interkom.mjpeg?t=${Date.now()}`;
}
if (pictureImg) {
  pictureImg.addEventListener("load", () => {
    lastFrameAt = Date.now();
    showPicture(true);
    chip(chipPicture, "Obraz: beží", "ok");
  });
  pictureImg.addEventListener("error", () => {
    // The M3c /interkom.mjpeg route may not exist yet (issue 1347) — stay on the placeholder and
    // let the retry timer try again. NOTE: precise frame-age (a "frozen picture" chip) needs the
    // real MJPEG stream semantics M3c serves; a plain <img> does not fire a reliable per-frame
    // event, so M3b only distinguishes streaming vs not-available and refines frame-age in M3c.
    showPicture(false);
    chip(chipPicture, "Obraz zatiaľ nie je k dispozícii", "wait");
  });
}
function tickPicture() {
  if (!hubReachable) {
    chip(chipPicture, "Obraz: čakám na hub…", "wait");
    return;
  }
  // Retry the picture while the hub is up but the picture is not yet streaming.
  if (pictureImg && pictureImg.hidden) tryPicture();
}

// ---- Mic device list (labels appear only after the first getUserMedia permission grant) ----
async function refreshMics() {
  if (!navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) return;
  try {
    const devs = await navigator.mediaDevices.enumerateDevices();
    const mics = devs.filter((d) => d.kind === "audioinput");
    const current = micSelect.value;
    // Keep the leading "default" option, replace the rest.
    while (micSelect.options.length > 1) micSelect.remove(1);
    let i = 1;
    for (const m of mics) {
      const opt = document.createElement("option");
      opt.value = m.deviceId;
      opt.textContent = m.label || `Mikrofón ${i}`;
      micSelect.appendChild(opt);
      i += 1;
    }
    if (current) micSelect.value = current;
  } catch (e) {
    // enumerateDevices can reject on a locked-down origin — leave the default option only.
  }
}
micSelect.addEventListener("change", () => {
  if (bridge && joined) switchMic(micSelect.value);
});

// ---- Janus WebRTC (audio) ------------------------------------------------------------------
// A minimal webRTC "adapter" shim so we do NOT have to vendor webrtc-adapter too: janus.js only
// reads `browserDetails.{browser,version}` for its per-browser branches; the stream-attach helpers
// live inside janus.js itself (v1.x).
function detectBrowser() {
  const ua = navigator.userAgent;
  let browser = "chrome";
  let version = 0;
  if (/firefox\//i.test(ua)) {
    browser = "firefox";
    version = parseInt((ua.match(/firefox\/(\d+)/i) || [])[1] || "0", 10);
  } else if (/edg\//i.test(ua)) {
    browser = "chrome";
    version = parseInt((ua.match(/edg\/(\d+)/i) || [])[1] || "0", 10);
  } else if (/chrome\//i.test(ua)) {
    browser = "chrome";
    version = parseInt((ua.match(/chrome\/(\d+)/i) || [])[1] || "0", 10);
  } else if (/safari\//i.test(ua)) {
    browser = "safari";
    version = parseInt((ua.match(/version\/(\d+)/i) || [])[1] || "605", 10);
  }
  return { browser, version };
}
function janusDeps() {
  return Janus.useDefaultDependencies({ adapter: { browserDetails: detectBrowser() } });
}

function setConnectedUi(connected) {
  connectBtn.textContent = connected ? "Odpojiť" : "Pripojiť";
  connectBtn.dataset.connected = connected ? "true" : "false";
}

function attachRemote(track) {
  // Attach the room mix (minus us) to the <audio autoplay playsinline> sink.
  const stream = new MediaStream([track]);
  remoteAudio.srcObject = stream;
  const p = remoteAudio.play();
  if (p && typeof p.catch === "function") p.catch(() => {});
}

function connect() {
  resetMicControls(); // a fresh cycle re-enables the mic (re-grant path after a listen-only join)
  chip(chipJanus, "Janus: spájam…", "wait");
  // debug:false → Janus.log/warn/error are all no-ops, so janus.js never writes to the console.
  Janus.init({
    debug: false,
    dependencies: janusDeps(),
    callback: () => {
      janus = new Janus({
        server: janusWsUrl(),
        dependencies: janusDeps(),
        success: () => attachBridge(),
        error: () => {
          chip(chipJanus, "Janus: nedostupný", "bad");
          teardown();
        },
        destroyed: () => {
          joined = false;
          chip(chipJanus, "Janus: odpojené", "idle");
        },
      });
    },
  });
  setConnectedUi(true);
}

function attachBridge() {
  janus.attach({
    plugin: "janus.plugin.audiobridge",
    success: (handle) => {
      bridge = handle;
      // Join the room MUTED, with the typed display name.
      bridge.send({
        message: { request: "join", room: ROOM, display: displayName(), muted: true },
      });
    },
    error: () => {
      chip(chipJanus, "Janus: plugin chyba", "bad");
      teardown();
    },
    onmessage: (msg, jsep) => onBridgeMessage(msg, jsep),
    onremotetrack: (track, mid, on) => {
      if (on && track.kind === "audio") attachRemote(track);
    },
    // janus 1.x single-stream fallback:
    onremotestream: (stream) => {
      remoteAudio.srcObject = stream;
      const p = remoteAudio.play();
      if (p && typeof p.catch === "function") p.catch(() => {});
    },
    oncleanup: () => {
      joined = false;
    },
  });
}

// A getUserMedia rejection (no microphone, or the user denied the permission) surfaces here as the
// createOffer error. Its DOMException `.name` is one of the getUserMedia error names; we also accept
// a message match so a wrapped error still routes to the listen-only fallback. Anything else is a
// real signalling error and is NOT silently turned into listen-only.
function isMicError(err) {
  if (!err) return false;
  const name = (err.name || (err.error && err.error.name) || "") + "";
  const micNames = [
    "NotFoundError",
    "NotAllowedError",
    "NotReadableError",
    "OverconstrainedError",
    "SecurityError",
    "AbortError",
    "PermissionDeniedError",
    "DevicesNotFoundError",
    "TrackStartError",
  ];
  if (micNames.indexOf(name) !== -1) return true;
  const text = ((err && err.message) || err || "") + "";
  return /getusermedia|permission|denied|microphone|mikrof|audio.?input|no.?(audio.?)?device/i.test(
    text
  );
}

// Try a SEND+RECV offer (publishes our muted mic so we can later unmute). If getUserMedia fails
// (no mic / permission denied) fall back to LISTEN-ONLY instead of dead-ending — the cameraman who
// refuses the permission still HEARS the intercom. muted:true keeps us silent until an unmute.
function joinWithMic() {
  const capture = micSelect.value ? { deviceId: { exact: micSelect.value } } : true;
  bridge.createOffer({
    tracks: [{ type: "audio", capture, recv: true }],
    success: (offerJsep) => {
      bridge.send({ message: { request: "configure", muted: true }, jsep: offerJsep });
      startStats();
    },
    error: (err) => {
      if (isMicError(err)) {
        joinListenOnly();
      } else {
        chip(chipJanus, "Janus: spojenie zlyhalo", "bad");
      }
    },
  });
}

// RECV-ONLY offer: a recv track with NO `capture`, so janus.js never calls getUserMedia. We only
// RECEIVE the room mix (kept playing on the <audio> sink), disable the mic controls, and show the
// listening chip. A later re-grant re-negotiates WITH send on the next "Pripojiť" cycle.
function joinListenOnly() {
  bridge.createOffer({
    tracks: [{ type: "audio", recv: true }],
    success: (offerJsep) => {
      bridge.send({ message: { request: "configure", muted: true }, jsep: offerJsep });
      enterListenOnly();
      startStats();
    },
    error: () => chip(chipJanus, "Janus: spojenie zlyhalo", "bad"),
  });
}

function onBridgeMessage(msg, jsep) {
  const event = msg && msg.audiobridge;
  if (event === "joined") {
    joined = true;
    chip(chipJanus, "Janus: v miestnosti", "ok");
    joinWithMic();
  } else if (event === "event" && msg.error) {
    chip(chipJanus, "Janus: " + msg.error, "bad");
  }
  if (jsep) {
    bridge.handleRemoteJsep({ jsep });
  }
}

// Enter listen-only: disable the mic toggle + device select (there is no mic to unmute) and show
// the listening chip. Called from the recv-only join path. The LEAVE direction is resetMicControls().
function enterListenOnly() {
  listenOnly = true;
  muted = true;
  micToggle.disabled = true;
  micToggle.dataset.muted = "true";
  micToggle.setAttribute("aria-pressed", "false");
  micToggle.textContent = "Mikrofón vypnutý";
  micSelect.disabled = true;
  chip(chipJanus, "Mikrofón: nedostupný (počúvate)", "warn");
}

// Re-enable the mic controls for a fresh connect (a re-granted mic then re-negotiates WITH send).
function resetMicControls() {
  listenOnly = false;
  micToggle.disabled = false;
  micSelect.disabled = false;
}

function setMuted(next) {
  if (listenOnly) return; // no mic to (un)mute in listen-only mode
  muted = next;
  micToggle.dataset.muted = muted ? "true" : "false";
  micToggle.setAttribute("aria-pressed", muted ? "false" : "true");
  micToggle.textContent = muted ? "Mikrofón vypnutý" : "Mikrofón ZAPNUTÝ";
  if (bridge && joined) {
    bridge.send({ message: { request: "configure", muted: muted } });
  }
}
micToggle.addEventListener("click", () => setMuted(!muted));

function switchMic(deviceId) {
  if (!bridge) return;
  const capture = deviceId ? { deviceId: { exact: deviceId } } : true;
  // Replace the sending audio track in place (janus 1.x multistream).
  bridge.replaceTracks({
    tracks: [{ type: "audio", capture, recv: true }],
    error: () => chip(chipAudio, "Zvuk: zmena mic zlyhala", "warn"),
  });
}

function startStats() {
  stopStats();
  lastAudioBytes = 0;
  statsTimer = setInterval(pollAudioStats, 2000);
}
function stopStats() {
  if (statsTimer) {
    clearInterval(statsTimer);
    statsTimer = null;
  }
}
async function pollAudioStats() {
  // Prove audio is FLOWING by watching the received bytes climb (getStats over the PeerConnection).
  const pc = bridge && bridge.webrtcStuff && bridge.webrtcStuff.pc;
  if (!pc || !pc.getStats) return;
  try {
    const report = await pc.getStats();
    let bytes = 0;
    report.forEach((s) => {
      if (s.type === "inbound-rtp" && s.kind === "audio") bytes += s.bytesReceived || 0;
    });
    if (bytes > lastAudioBytes) {
      chip(chipAudio, "Zvuk: počujem", "ok");
    } else {
      chip(chipAudio, "Zvuk: ticho", "warn");
    }
    lastAudioBytes = bytes;
  } catch (e) {
    // getStats can reject transiently — leave the last chip.
  }
}

function teardown() {
  stopStats();
  joined = false;
  try {
    if (janus) janus.destroy();
  } catch (e) {
    // ignore — a failed destroy still resets the UI.
  }
  janus = null;
  bridge = null;
  resetMicControls(); // a disconnected page shows the mic controls enabled (self-corrects a listen-only session)
  setConnectedUi(false);
}

connectBtn.addEventListener("click", () => {
  if (janus) {
    teardown();
    chip(chipJanus, "Janus: odpojené", "idle");
  } else {
    // The button click is the autoplay + getUserMedia gesture. Refresh the mic list now that a
    // permission grant is likely (labels populate after the first grant).
    connect();
    refreshMics();
  }
});

// ---- PWA service worker (installable; pure passthrough, no cache — server-truth) ------------
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

// ---- Boot ----------------------------------------------------------------------------------
setMuted(true);
refreshVersion();
refreshMics();
pollHub();
setInterval(pollHub, HUB_POLL_MS);
setInterval(tickPicture, PICTURE_RETRY_MS);
