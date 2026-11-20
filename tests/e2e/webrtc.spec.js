// @ts-check
// The in-hub WebRTC phone leg end to end (intercom#42): the REAL web client against the REAL
// intercom-hub binary with `adapter = "webrtc"` (webrtc.toml, started and stopped here through
// webrtc-hub.js). No Janus anywhere: the page reads `GET /api/phone` -> webrtc and talks to the
// hub's own WebRTC endpoint with a native RTCPeerConnection. Two Chromium pages are two phones with
// Chromium's fake microphone; a VBAN tone sent into the hub is the cutter's voice.
const fs = require("fs");
const path = require("path");
const { test, expect } = require("@playwright/test");
const hubCtl = require("./webrtc-hub");
const { startTone } = require("./vban-tone");

let hub = null;

test.beforeAll(async () => {
  hubCtl.renderConfig();
  hub = await hubCtl.startHub();
});

test.afterAll(async () => {
  if (hub) await hub.stop();
});

// Before app.js: the service worker never registers (the phone.spec.js pattern) and the name is
// preset, so the name sheet does not cover the page.
async function preparePage(page, name) {
  // The picture is not under test here and this hub has no [video] (its /interkom.mjpeg answers
  // 503, which the browser logs): serve one still frame instead (picture-sw.spec.js covers it).
  const still = fs.readFileSync(path.join(__dirname, "fixtures", "picture-0.jpg"));
  await page.route("**/interkom.mjpeg*", (route) =>
    route.fulfill({ status: 200, contentType: "image/jpeg", body: still })
  );
  await page.addInitScript((n) => {
    if (navigator.serviceWorker) {
      Object.defineProperty(navigator.serviceWorker, "register", {
        value: () => new Promise(() => {}),
        configurable: true,
      });
    }
    try {
      localStorage.setItem("interkom.display", n);
    } catch (e) {
      // private mode: the name sheet would show and the test fail loudly.
    }
  }, name);
}

function watchConsole(page) {
  const seen = [];
  page.on("console", (msg) => seen.push(`${msg.type()}: ${msg.text()}`));
  page.on("pageerror", (err) => seen.push(`pageerror: ${err.message}`));
  return seen;
}

async function expectConnected(page, timeout = 15000) {
  const conn = page.locator('[data-role="conn"]');
  await expect(conn).toHaveAttribute("data-state", "connected", { timeout });
  await expect(conn).toContainText("Pripojené");
}

// The highest level a meter showed while sampling it every 100 ms for `ms`.
async function meterMax(page, role, ms) {
  const loc = page.locator(`[data-role="${role}"]`);
  let max = -120;
  const end = Date.now() + ms;
  while (Date.now() < end) {
    max = Math.max(max, Number(await loc.getAttribute("data-db")));
    await page.waitForTimeout(100);
  }
  return max;
}

// The meter falls at 30 dB/s (app.js METER_RELEASE_DB_PER_S), so "quiet" is: it reaches the
// silence floor, then STAYS there for the whole window (reading it at once raced the release).
async function expectQuiet(page, role, ms, message) {
  const loc = page.locator(`[data-role="${role}"]`);
  await expect
    .poll(async () => Number(await loc.getAttribute("data-db")), { message: `${message}: reaches silence`, timeout: 10000 })
    .toBeLessThan(-70);
  expect(await meterMax(page, role, ms), `${message}: stays silent`).toBeLessThan(-70);
}

async function hubState() {
  const r = await fetch(`${hubCtl.BASE_URL}/api/state`);
  return r.json();
}

test("two phones: each hears the cutter's tone, and A's mic reaches B but never A itself", async ({ browser }) => {
  test.setTimeout(90000); // the ICE-quiet drop alone takes ~15 s (str0m's ICE-lite pair timeout)
  const ctxA = await browser.newContext();
  const ctxB = await browser.newContext();
  const a = await ctxA.newPage();
  const b = await ctxB.newPage();
  const seenA = watchConsole(a);
  const seenB = watchConsole(b);
  await preparePage(a, "Kamera A");
  await preparePage(b, "Kamera B");
  await a.goto("/");
  await b.goto("/");
  await expectConnected(a);
  await expectConnected(b);

  // The hub really carries two phones, by name, on its own WebRTC leg (not Janus).
  await expect
    .poll(async () => {
      const st = await hubState();
      return (st.webrtc && st.webrtc.phones.filter((p) => p.connected).map((p) => p.display).sort()) || [];
    })
    .toEqual(["Kamera A", "Kamera B"]);
  expect(await a.evaluate(() => transport)).toBe("webrtc");

  // Quiet room: nobody talks, both incoming meters sit at silence.
  await expectQuiet(a, "meter-in", 1500, "A in a quiet room");
  await expectQuiet(b, "meter-in", 1500, "B in a quiet room");

  // The cutter's voice (a VBAN tone into the hub) reaches BOTH phones.
  const stopTone = startTone({ port: hubCtl.VBAN_PORT, stream: "prog", channels: 1, hz: 997 });
  try {
    await expect(a.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "true", { timeout: 10000 });
    await expect(b.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "true", { timeout: 10000 });
    await expect(a.locator('[data-role="meter-in-state"]')).toHaveText("hovorí");
  } finally {
    stopTone();
  }
  await expect(a.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "false", { timeout: 10000 });
  await expect(b.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "false", { timeout: 10000 });

  // A turns the mic on: B hears A (Chromium's fake microphone), A never hears itself (N-1).
  // First A's meter must have released the tone fully, so what it shows after is A's own room.
  await expectQuiet(a, "meter-in", 500, "A before its mic goes on");
  const micA = a.locator('[data-role="mic-toggle"]');
  await micA.click();
  await expect(micA).toHaveAttribute("data-muted", "false");
  await expect(a.locator('[data-role="mic-hint"]')).toContainText("Počujú ťa");
  expect(await meterMax(b, "meter-in", 4000), "B hears A's mic").toBeGreaterThan(-50);
  expect(await meterMax(a, "meter-in", 3000), "A never hears its own mic").toBeLessThan(-70);
  await expect
    .poll(async () => {
      const st = await hubState();
      const pa = st.webrtc.phones.find((p) => p.display === "Kamera A");
      return pa && pa.mic_active;
    })
    .toBe(true);

  // Mic OFF = the track is disabled, no renegotiation: B's incoming goes quiet again.
  await micA.click();
  await expect(micA).toHaveAttribute("data-muted", "true");
  await expect(b.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "false", { timeout: 10000 });
  await expectQuiet(b, "meter-in", 2000, "B after A's mic OFF");

  // A renames itself: the hub shows the new name.
  await a.locator('[data-role="settings-open"]').click();
  await a.locator('[data-role="name-change"]').click();
  await a.locator('[data-role="name"]').fill("Kamera 7");
  await a.locator('[data-role="name-form"] button[type="submit"]').click();
  await expect
    .poll(async () => (await hubState()).webrtc.phones.map((p) => p.display).sort())
    .toEqual(["Kamera 7", "Kamera B"]);

  expect(seenA, "browser console must stay completely clean").toEqual([]);
  expect(seenB, "browser console must stay completely clean").toEqual([]);

  // Closing a page frees its slot at once (pagehide -> DELETE /api/phone/<id>) ...
  await b.close({ runBeforeUnload: true });
  await expect.poll(async () => (await hubState()).webrtc.phones.map((p) => p.display)).toEqual(["Kamera 7"]);
  await ctxB.close();
  // ... and a phone that vanishes without a word is dropped by the hub once its ICE goes quiet.
  // The renderer is crashed, so no pagehide, no DELETE and no DTLS close reach the hub: only the
  // missing STUN checks do (closing the context would fire pagehide -> DELETE).
  const cdp = await ctxA.newCDPSession(a);
  // Page.crash never answers (its target is gone), so it is not awaited: the hub sees the
  // phone's checks stop.
  cdp.send("Page.crash").catch(() => {});
  await expect.poll(async () => (await hubState()).webrtc.phones.length, { timeout: 30000 }).toBe(0);
  const ended = fs
    .readFileSync(path.join(hubCtl.RUN_DIR, "hub.log"), "utf8")
    .split("\n")
    .filter((l) => l.includes("phone session ended") && l.includes("Kamera 7"));
  expect(ended.length, ended.join("\n")).toBe(1);
  expect(ended[0], "dropped by ICE, not by a leave").toContain('"ICE disconnected"');
  await ctxA.close();
});

test("killing the hub and starting it again reconnects the page by itself", async ({ page }) => {
  test.setTimeout(90000);
  const seen = watchConsole(page);
  await preparePage(page, "Kamera R");
  await page.goto("/");
  await expectConnected(page);

  const conn = page.locator('[data-role="conn"]');
  const outageFrom = seen.length;
  await hub.stop("SIGKILL");
  await expect(conn, "the dead hub is noticed within seconds").toHaveAttribute("data-state", "reconnecting", {
    timeout: 15000,
  });
  await page.waitForTimeout(3000); // a few failed attempts against the dead hub
  hub = await hubCtl.startHub();
  // Back by itself: the 1 -> 4 s backoff brings it in within seconds, no reload.
  await expectConnected(page, 20000);
  // The outage ends once the page is connected again: a request refused just before the hub
  // answered may still be logged after startHub resolved.
  const outageTo = seen.length;

  // And the audio flows again.
  const stopTone = startTone({ port: hubCtl.VBAN_PORT, stream: "prog", channels: 1, hz: 997 });
  try {
    await expect(page.locator('[data-role="meter-in"]')).toHaveAttribute("data-active", "true", { timeout: 10000 });
  } finally {
    stopTone();
  }
  await expect
    .poll(async () => (await hubState()).webrtc.phones.filter((p) => p.connected).map((p) => p.display))
    .toEqual(["Kamera R"]);

  // While the hub was DOWN the browser itself logs its network line for each refused request
  // (network class, never a JS error; the phone.spec.js Janus outage logs the same). Outside that
  // window nothing at all.
  const outside = [...seen.slice(0, outageFrom), ...seen.slice(outageTo)];
  expect(outside, "browser console must stay clean outside the outage").toEqual([]);
  const during = seen.slice(outageFrom, outageTo);
  for (const line of during) {
    expect(line, "only the browser's own network lines while the hub is down").toMatch(
      /^error: Failed to load resource: net::ERR_CONNECTION_REFUSED$/
    );
  }
});
