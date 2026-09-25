// @ts-check
// Real-browser E2E for the interkom phone PWA (issue 1345, owner ruling 25.9.2026 — the phone UX
// rework). The page is the REAL intercom/web client AND the real vendored janus.js, served by
// stub_hub.py; only the external Janus SERVER is a test double (fake-janus-server.js: a WebSocket
// double with a real in-page RTCPeerConnection), so SDP directions, renegotiation and audio really
// flow through WebRTC. Chromium's fake media device gives both level meters a real signal. Every
// test asserts a completely clean browser console.
const fs = require("fs");
const path = require("path");
const { test, expect } = require("@playwright/test");

const SCREENSHOT_DIR = process.env.INTERKOM_E2E_SCREENSHOT_DIR || "";

// Before app.js runs in every test:
// - neutralise the service-worker registration (a claimed page would route fetches through the
//   SW; Playwright's own `serviceWorkers: "block"` logs a console warning, which the zero-console
//   gate would catch) — the same pattern as the bkshading panel spec;
// - count the page's own getUserMedia calls, keeping the unwrapped one for the fake Janus's
//   "room audio" (so the fake's remote track is never counted as a mic permission request);
// - install the fake Janus server.
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    if (navigator.serviceWorker) {
      Object.defineProperty(navigator.serviceWorker, "register", {
        value: () => new Promise(() => {}),
        configurable: true,
      });
    }
    window.__gumCalls = 0;
    const md = navigator.mediaDevices;
    if (md && md.getUserMedia) {
      const real = md.getUserMedia.bind(md);
      window.__realGUM = real;
      md.getUserMedia = (c) => {
        window.__gumCalls += 1;
        return real(c);
      };
    }
  });
  await page.addInitScript({ path: path.join(__dirname, "fake-janus-server.js") });
});

function watchConsole(page) {
  const seen = [];
  page.on("console", (msg) => seen.push(`${msg.type()}: ${msg.text()}`));
  page.on("pageerror", (err) => seen.push(`pageerror: ${err.message}`));
  return seen;
}

async function presetName(page, name) {
  await page.addInitScript((n) => {
    try {
      localStorage.setItem("interkom.display", n);
    } catch (e) {
      // private mode — the test would then see the name sheet and fail loudly, which is right.
    }
  }, name);
}

async function fake(page) {
  return page.evaluate(() => {
    const f = window.__fakeJanus;
    return {
      sessions: f.sessions,
      joins: f.joins,
      offers: f.offers,
      configures: f.configures,
      gumCalls: window.__gumCalls,
    };
  });
}

// Sample a meter's live level (data-db, dBFS) every 100 ms.
async function meterSamples(page, role, ms) {
  const out = [];
  const loc = page.locator(`[data-role="${role}"]`);
  const end = Date.now() + ms;
  while (Date.now() < end) {
    out.push(Number(await loc.getAttribute("data-db")));
    await page.waitForTimeout(100);
  }
  return out;
}

function expectMoving(samples, label) {
  const max = Math.max(...samples);
  const min = Math.min(...samples);
  expect(max, `${label}: the meter must show a real signal (max ${max} dB)`).toBeGreaterThan(-50);
  expect(max - min, `${label}: the meter must MOVE, not sit on one value (${min}..${max} dB)`).toBeGreaterThan(6);
}

async function expectConnected(page) {
  const conn = page.locator('[data-role="conn"]');
  await expect(conn).toHaveAttribute("data-state", "connected", { timeout: 15000 });
  await expect(conn).toContainText("Pripojené");
}

async function shot(page, testInfo, name) {
  const file = testInfo.outputPath(name);
  await page.screenshot({ path: file });
  if (SCREENSHOT_DIR) {
    fs.mkdirSync(SCREENSHOT_DIR, { recursive: true });
    fs.copyFileSync(file, path.join(SCREENSHOT_DIR, name));
  }
}

test("opening the link joins receive-only with the mic OFF, and the incoming meter moves", async ({ page }, testInfo) => {
  const seen = watchConsole(page);
  await presetName(page, "Kamera 1");
  await page.goto("/");

  // No connect button at all; the page shows it is connecting, then connected.
  await expect(page.locator('[data-role="connect"]')).toHaveCount(0);
  await expectConnected(page);

  const mic = page.locator('[data-role="mic-toggle"]');
  await expect(mic).toHaveAttribute("data-muted", "true");
  await expect(mic).toContainText("Mikrofón vypnutý");
  await expect(page.locator('[data-role="tap-for-sound"]')).toBeHidden();
  await expect(page.locator('[data-role="name-sheet"]')).toBeHidden();

  const f = await fake(page);
  expect(f.joins.length).toBe(1);
  expect(f.joins[0].muted).toBe(true);
  expect(f.joins[0].display).toBe("Kamera 1");
  expect(f.offers[0].direction, "the auto-join offer is receive-only").toBe("recvonly");
  expect(f.gumCalls, "no microphone permission is requested on load").toBe(0);

  // The incoming ("Strihač / réžia") meter shows the live room audio.
  await expect(page.locator('[data-role="meter-in"]')).toBeVisible();
  expectMoving(await meterSamples(page, "meter-in", 3000), "incoming meter");

  // The picture is on the page, full width.
  const img = page.locator('[data-role="picture"]');
  await expect(img).toBeVisible({ timeout: 10000 });
  const box = await page.locator('[data-role="picture-wrap"]').boundingBox();
  expect(box && box.width, "the picture fills the phone width").toBeGreaterThanOrEqual(388);

  // version-on-dashboard: the deployed version is visible and matches the hub's /api/version.
  const version = page.locator('[data-testid="version"]');
  await expect(version).toBeVisible();
  await expect(version).toHaveText(/^v\d+\.\d+\.\d+(-dev\.\d+)?$/);
  const api = await page.evaluate(() => fetch("/api/version").then((r) => r.json()));
  await expect(version).toHaveText(`v${api.version}`);

  await shot(page, testInfo, "phone-390x844-connected.png");
  expect(seen, "browser console must stay completely clean").toEqual([]);
});

test("the mic toggle asks for permission once, sends my mic with a live meter, and mutes again", async ({ page }, testInfo) => {
  const seen = watchConsole(page);
  await presetName(page, "Kamera 2");
  await page.goto("/");
  await expectConnected(page);

  const mic = page.locator('[data-role="mic-toggle"]');
  await mic.click();
  await expect(mic).toHaveAttribute("data-muted", "false");
  await expect(mic).toContainText("MIKROFÓN ZAPNUTÝ");

  let f = await fake(page);
  expect(f.gumCalls, "the first mic ON asks for the microphone").toBe(1);
  await expect.poll(async () => (await fake(page)).offers.length).toBe(2);
  f = await fake(page);
  expect(f.offers[1].direction, "the first mic ON re-negotiates the same session with send").toBe("sendrecv");
  expect(f.sessions, "the session is NOT rebuilt for the mic").toBe(1);
  expect(f.joins.length).toBe(1);
  const on = f.configures[f.configures.length - 1];
  expect(on.message.muted).toBe(false);
  expect(on.jsep).toBe(true);
  await expect(page.locator('[data-role="conn"]')).toHaveAttribute("data-state", "connected");
  // The mic audio really reaches the (fake) Janus over WebRTC.
  const before = await page.evaluate(() => window.__fakeJanus.inboundAudioBytes());
  await expect
    .poll(async () => page.evaluate(() => window.__fakeJanus.inboundAudioBytes()), { timeout: 10000 })
    .toBeGreaterThan(before + 1000);

  const micMeter = page.locator('[data-role="meter-mic"]');
  await expect(micMeter).toBeVisible();
  expectMoving(await meterSamples(page, "meter-mic", 3000), "own mic meter");
  await shot(page, testInfo, "phone-390x844-mic-on.png");

  await mic.click();
  await expect(mic).toHaveAttribute("data-muted", "true");
  await expect(mic).toContainText("Mikrofón vypnutý");
  await expect(micMeter).toBeHidden();
  f = await fake(page);
  const off = f.configures[f.configures.length - 1];
  expect(off.message.muted).toBe(true);
  expect(off.jsep).toBe(false);

  // ON again: no second permission prompt and no re-negotiation, just unmute.
  await mic.click();
  await expect(mic).toHaveAttribute("data-muted", "false");
  f = await fake(page);
  expect(f.gumCalls).toBe(1);
  expect(f.offers.length, "ON again does not re-negotiate").toBe(2);
  const again = f.configures[f.configures.length - 1];
  expect(again.message.muted).toBe(false);
  expect(again.jsep).toBe(false);

  // Another mic from the settings sheet: a fresh getUserMedia, swapped in place — no new offer,
  // no new session, the connection stays up.
  await page.locator('[data-role="settings-open"]').click();
  const select = page.locator('[data-role="mic-select"]');
  await expect.poll(async () => select.locator("option").count()).toBeGreaterThan(2);
  const other = await select.locator("option").nth(2).getAttribute("value");
  await select.selectOption(other);
  await page.locator('[data-role="settings-close"]').click();
  await expect.poll(async () => (await fake(page)).gumCalls).toBe(2);
  f = await fake(page);
  expect(f.offers.length, "a device change is a plain track swap").toBe(2);
  expect(f.sessions).toBe(1);
  await expect(page.locator('[data-role="conn"]')).toHaveAttribute("data-state", "connected");
  await expect(mic).toHaveAttribute("data-muted", "false");
  expectMoving(await meterSamples(page, "meter-mic", 2500), "own mic meter after a device change");

  expect(seen, "browser console must stay completely clean").toEqual([]);
});

test("a rejected mic renegotiation never leaves a false 'Počujú ťa': the session is rebuilt with the mic", async ({ page }) => {
  const seen = watchConsole(page);
  await presetName(page, "Kamera 7");
  await page.goto("/");
  await expectConnected(page);

  // Janus rejects the first-ON renegotiation (an error event, no answer).
  await page.evaluate(() => {
    window.__fakeJanus.rejectNextConfigures = 1;
  });
  const mic = page.locator('[data-role="mic-toggle"]');
  await mic.click();
  await expect(mic).toHaveAttribute("data-muted", "false");

  // The page must not sit on "Pripojené" + "Počujú ťa" with nobody hearing it: it rebuilds the
  // session, which offers WITH the mic, and only then claims the mic is heard.
  await expect.poll(async () => (await fake(page)).sessions, { timeout: 15000 }).toBe(2);
  await expectConnected(page);
  const f = await fake(page);
  expect(f.offers[f.offers.length - 1].direction).toBe("sendrecv");
  expect(f.configures[f.configures.length - 1].message.muted).toBe(false);
  await expect(page.locator('[data-role="mic-hint"]')).toContainText("Počujú ťa");
  expect(f.gumCalls, "no second permission prompt").toBe(1);
  expect(seen, "browser console must stay completely clean").toEqual([]);
});

test("a tap on the picture makes it fullscreen and a second tap returns", async ({ page }) => {
  const seen = watchConsole(page);
  await presetName(page, "Kamera 3");
  await page.goto("/");
  await expectConnected(page);

  const wrap = page.locator('[data-role="picture-wrap"]');
  await expect(page.locator('[data-role="picture"]')).toBeVisible({ timeout: 10000 });
  await expect(wrap).toHaveAttribute("data-fullscreen", "false");

  await wrap.click();
  await expect(wrap).toHaveAttribute("data-fullscreen", "true");
  const box = await wrap.boundingBox();
  expect(box && box.height, "fullscreen covers the whole screen height").toBeGreaterThanOrEqual(800);

  await wrap.click();
  await expect(wrap).toHaveAttribute("data-fullscreen", "false");
  expect(seen, "browser console must stay completely clean").toEqual([]);
});

test("a dropped connection shows the reconnect state and reconnects with the mic state kept", async ({ page }, testInfo) => {
  const seen = watchConsole(page);
  await presetName(page, "Kamera 4");
  await page.goto("/");
  await expectConnected(page);

  const mic = page.locator('[data-role="mic-toggle"]');
  await mic.click();
  await expect(mic).toHaveAttribute("data-muted", "false");

  // The server drops the session and the first retry also fails.
  await page.evaluate(() => {
    window.__fakeJanus.failConnects = 1;
    window.__fakeJanus.dropConnection();
  });
  const conn = page.locator('[data-role="conn"]');
  await expect(conn).toHaveAttribute("data-state", "reconnecting");
  await expect(conn).toContainText("Odpojené – skúšam znova");
  // Red the instant the state changes (no colour transition that would show green for a moment).
  expect(await conn.evaluate((e) => getComputedStyle(e).backgroundColor)).toBe("rgb(74, 20, 20)");
  // The mic stays ON but never claims "they hear you" while the session is down.
  await expect(page.locator('[data-role="mic-hint"]')).toHaveText("Zapnutý — čakám na spojenie");
  await expect(page.locator('[data-role="meter-in-state"]')).toHaveText("čakám…");
  await shot(page, testInfo, "phone-390x844-reconnecting.png");

  await expectConnected(page);
  const f = await fake(page);
  expect(f.joins.length, "the page joined again on its own").toBe(2);
  const offer = f.offers[f.offers.length - 1];
  expect(offer.direction, "the rebuilt session sends the mic that was ON").toBe("sendrecv");
  await expect(page.locator('[data-role="mic-hint"]')).toContainText("Počujú ťa");
  const cfg = f.configures[f.configures.length - 1];
  expect(cfg.message.muted).toBe(false);
  expect(f.gumCalls, "no new permission prompt after a reconnect").toBe(1);
  expect(seen, "browser console must stay completely clean").toEqual([]);
});

test("the name is asked once and remembered", async ({ page }) => {
  const seen = watchConsole(page);
  await page.goto("/");

  const sheet = page.locator('[data-role="name-sheet"]');
  await expect(sheet).toBeVisible();
  // The connection does not wait for the name.
  await expectConnected(page);

  await page.locator('[data-role="name"]').fill("Kamera 5");
  await page.locator('[data-role="name-save"]').click();
  await expect(sheet).toBeHidden();
  await expect(page.locator('[data-role="name-value"]')).toHaveText("Kamera 5");
  const f = await fake(page);
  expect(f.configures.some((c) => c.message.display === "Kamera 5"), "the new name is sent to the room").toBe(true);

  await page.reload();
  await expectConnected(page);
  await expect(sheet).toBeHidden();
  await expect(page.locator('[data-role="name-value"]')).toHaveText("Kamera 5");
  const f2 = await fake(page);
  expect(f2.joins[0].display).toBe("Kamera 5");
  expect(seen, "browser console must stay completely clean").toEqual([]);
});

// Emulate a phone browser that blocks sound until the first touch: the spec-defined autoplay
// rejection (play() -> NotAllowedError) until the first TRUSTED tap/key. Automated Chromium never
// blocks on its own, and its navigator.userActivation already reads true on a fresh page under
// Playwright (probed 25.9.2026), so the gate is our own trusted-event flag. It also records any
// AudioContext created before the first touch — the page must not create one then (a real Chrome
// logs a warning for it).
async function emulateAutoplayBlocked(page) {
  await page.addInitScript(() => {
    window.__touched = false;
    for (const type of ["click", "touchend", "keydown"]) {
      document.addEventListener(
        type,
        (e) => {
          if (e.isTrusted) window.__touched = true;
        },
        true
      );
    }
    const realPlay = HTMLMediaElement.prototype.play;
    HTMLMediaElement.prototype.play = function () {
      if (!window.__touched) {
        return Promise.reject(new DOMException("play() needs a user gesture", "NotAllowedError"));
      }
      return realPlay.call(this);
    };
    window.__ctxBeforeGesture = 0;
    const RealCtx = window.AudioContext;
    window.AudioContext = function (...args) {
      if (!window.__touched) window.__ctxBeforeGesture += 1;
      return new RealCtx(...args);
    };
  });
}

test("when autoplay is blocked, one tap on 'Ťukni pre zvuk' turns the sound on", async ({ page }, testInfo) => {
  const seen = watchConsole(page);
  await presetName(page, "Kamera 6");
  await emulateAutoplayBlocked(page);
  await page.goto("/");
  await expectConnected(page);

  const layer = page.locator('[data-role="tap-for-sound"]');
  await expect(layer).toBeVisible();
  await expect(layer).toContainText("Ťukni pre zvuk");
  await shot(page, testInfo, "phone-390x844-tap-for-sound.png");

  expect(await page.evaluate(() => window.__ctxBeforeGesture), "no AudioContext before the first touch").toBe(0);

  await layer.click();
  await expect(layer).toBeHidden();
  const paused = await page.evaluate(() => document.querySelector('[data-role="remote-audio"]').paused);
  expect(paused, "the room audio plays after the tap").toBe(false);
  expectMoving(await meterSamples(page, "meter-in", 3000), "incoming meter after the tap");
  expect(seen, "browser console must stay completely clean").toEqual([]);
});
