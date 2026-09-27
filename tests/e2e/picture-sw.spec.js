// @ts-check
// The interkom picture with the PWA service worker REGISTERED and CONTROLLING the page (issue 1379).
//
// phone.spec.js neutralises the service-worker registration so its tests stay about the audio UX.
// That hid the bug this file guards: `intercom/web/sw.js` answered every fetch with
// `event.respondWith(fetch(event.request))`, including the endless `multipart/x-mixed-replace`
// picture stream `/interkom.mjpeg`, and WebKit (iPhone Safari) does not hand that never-ending
// response to the page's <img> through a worker. The iPhone kept its installed worker across hub
// versions, so the picture was gone on every hub. Here the worker registers for real, the page is
// reloaded so the worker controls it, and the picture must then actually decode and keep moving:
// stub_hub.py streams the three committed JPEG frames (red / green / blue centre band) as real
// multipart parts. The last test starts from the OLD proxy-everything worker (the state every
// installed phone is in) and requires one reload to bring the picture back. Runs in the Chromium
// phone project AND the WebKit iPhone project; every test asserts a completely clean console.
const path = require("path");
const { test, expect } = require("@playwright/test");

test.beforeEach(async ({ page }) => {
  // The fake Janus server only keeps the page's audio side quiet; this file does not test audio.
  await page.addInitScript({ path: path.join(__dirname, "fake-janus-server.js") });
  await page.addInitScript(() => {
    try {
      localStorage.setItem("interkom.display", "Kamera 8");
    } catch (e) {
      // private mode: the name sheet shows, which does not affect the picture
    }
  });
});

// The upgrade test switches the stub to the old worker; never let that leak into the next test.
test.afterEach(async ({ request }) => {
  await request.get("/__test/legacy-sw/off");
});

function watchConsole(page) {
  const seen = [];
  page.on("console", (msg) => seen.push(`${msg.type()}: ${msg.text()}`));
  page.on("pageerror", (err) => seen.push(`pageerror: ${err.message}`));
  return seen;
}

// Load the page, wait for /sw.js to be installed and active, then reload so the worker controls the
// page from its very first request — the state an installed phone is in on every later visit.
async function openControlledByWorker(page) {
  await page.goto("/");
  await page.evaluate(() => navigator.serviceWorker.ready.then(() => true));
  await page.reload();
  const controlled = await page.evaluate(async () => {
    await navigator.serviceWorker.ready;
    return !!navigator.serviceWorker.controller;
  });
  expect(controlled, "the service worker controls the page").toBe(true);
}

// The native player's mirror canvas: app.js draws the picture <img> into it and feeds its
// captureStream() to the native video. A top-level `let` of the classic app.js script, so it is
// reachable by name from page.evaluate.
const NATIVE_MIRROR = "native-mirror";

// The centre-band colour of what an element currently shows, drawn into a canvas: "r" / "g" / "b"
// for one of the three stub frames, "bg" for the dark frame edge, "none" when nothing is decoded.
// `target` is a CSS selector or NATIVE_MIRROR.
async function centreColour(page, target) {
  return page.evaluate(([sel, mirror]) => {
    const el = sel === mirror ? (typeof nativeCanvas === "undefined" ? null : nativeCanvas) : document.querySelector(sel);
    if (!el) return "none";
    const isImg = el instanceof HTMLImageElement;
    const w = isImg ? el.naturalWidth : el.width;
    const h = isImg ? el.naturalHeight : el.height;
    if (!w || !h) return "none";
    const c = document.createElement("canvas");
    c.width = w;
    c.height = h;
    const ctx = c.getContext("2d");
    ctx.drawImage(el, 0, 0, w, h);
    const [r, g, b] = ctx.getImageData(Math.floor(w / 2), Math.floor(h / 2), 1, 1).data;
    if (r > 150 && g < 110 && b < 110) return "r";
    if (g > 150 && r < 110 && b < 110) return "g";
    if (b > 150 && r < 110 && g < 110) return "b";
    if (r < 60 && g < 60 && b < 70) return "bg";
    return `other(${r},${g},${b})`;
  }, [target, NATIVE_MIRROR]);
}

async function coloursSeen(page, target, ms) {
  const seen = new Set();
  const end = Date.now() + ms;
  while (Date.now() < end) {
    seen.add(await centreColour(page, target));
    await page.waitForTimeout(150);
  }
  return [...seen].sort();
}

test("with the service worker controlling the page the picture decodes and keeps moving (issue 1379)", async ({ page }) => {
  const seen = watchConsole(page);
  await openControlledByWorker(page);

  const img = page.locator('[data-role="picture"]');
  await expect(img, "the picture replaces the placeholder").toBeVisible({ timeout: 15000 });
  await expect(page.locator('[data-role="picture-placeholder"]')).toBeHidden();
  const size = await img.evaluate((el) => [el.naturalWidth, el.naturalHeight]);
  expect(size, "a decoded 320x180 frame").toEqual([320, 180]);

  // A live stream, not one frozen part: the centre band changes colour as the parts arrive.
  const colours = (await coloursSeen(page, '[data-role="picture"]', 2500)).filter((c) => c.length === 1);
  expect(colours.length, `the picture keeps updating (saw ${colours})`).toBeGreaterThanOrEqual(2);
  expect(seen, "browser console must stay completely clean").toEqual([]);
});

test("with the service worker controlling the page an iPhone tap still opens the native player carrying the picture (issue 1379)", async ({ page }) => {
  // iPhone Safari's shape: no element Fullscreen API, only HTMLVideoElement.webkitEnterFullscreen.
  // WebKit has the real function; record the call instead of entering a headless fullscreen.
  await page.addInitScript(() => {
    delete Element.prototype.requestFullscreen;
    delete Element.prototype.webkitRequestFullscreen;
    window.__nativeFs = [];
    HTMLVideoElement.prototype.webkitEnterFullscreen = function () {
      window.__nativeFs.push({ role: this.dataset.role, readyState: this.readyState, hasStream: !!this.srcObject });
    };
  });
  const seen = watchConsole(page);
  await openControlledByWorker(page);

  await expect(page.locator('[data-role="picture"]')).toBeVisible({ timeout: 15000 });
  // The native video is armed from the picture and plays the mirror's frames at the picture's size.
  await expect
    .poll(
      () =>
        page.evaluate(() => {
          const v = document.querySelector('[data-role="picture-native"]');
          return v.readyState >= 2 && !v.paused ? `${v.videoWidth}x${v.videoHeight}` : "not playing";
        }),
      { timeout: 10000 }
    )
    .toBe("320x180");
  // What the video plays IS the mirror canvas: its track is that canvas's capture track.
  const playsMirror = await page.evaluate(() => {
    const v = document.querySelector('[data-role="picture-native"]');
    const track = v.srcObject && v.srcObject.getVideoTracks()[0];
    return !!track && typeof nativeCanvas !== "undefined" && track.canvas === nativeCanvas;
  });
  expect(playsMirror, "the native video plays the mirror canvas's capture track").toBe(true);
  // And the mirror really carries the LIVE multipart picture (the 27.9.2026 rollback's first
  // suspect: WebKit handing a multipart <img> frame to a canvas). The mirror is read, not the video:
  // in Playwright's headless WebKit a canvas draw of ANY captureStream-fed video reads transparent (a
  // solid-colour canvas stream reads [0,0,0,0] too, probed 27.9.2026), which is the test engine, not
  // the page.
  // Timing coupling: app.js redraws the mirror every NATIVE_IDLE_MS (500 ms) and the stub cycles
  // three colours at 10 fps (a 300 ms cycle), so each redraw moves 5 frames and the colour changes.
  // If NATIVE_IDLE_MS becomes a multiple of 300 ms, every redraw lands on the same colour and this
  // check fails for a reason unrelated to the picture: change the stub's frame count or rate then.
  const mirror = (await coloursSeen(page, NATIVE_MIRROR, 3000)).filter((c) => c.length === 1);
  expect(mirror.length, `the native mirror follows the live picture (saw ${mirror})`).toBeGreaterThanOrEqual(2);

  await page.locator('[data-role="picture-wrap"]').click();
  const calls = await page.evaluate(() => window.__nativeFs);
  expect(calls, "the tap opens the native player exactly once").toHaveLength(1);
  expect(calls[0].role).toBe("picture-native");
  expect(calls[0].hasStream).toBe(true);
  await expect(page.locator('[data-role="picture-wrap"]')).toHaveAttribute("data-fullscreen", "false");
  expect(seen, "browser console must stay completely clean").toEqual([]);
});

// The page errors WebKit logs when the OLD worker's proxied picture stream is aborted (see below).
const OLD_WORKER_STREAM_ABORT = new Set(["pageerror: ", "pageerror: Cannot load .", "pageerror: Load failed"]);

// Every installed phone is in this state when the fixed hub goes live: it still runs the old
// worker that proxies every request. One ordinary reload must install the new worker and bring the
// picture back (the new worker claims the page, and the page's 5 s picture retry then fetches the
// stream natively). On WebKit the picture can only appear once the new worker is in control; on
// Chromium the old worker never broke the picture, so there this only proves the upgrade is clean.
test("a phone still on the old proxy-everything worker takes the new one on its next load and the picture returns (issue 1379)", async ({ page, request }) => {
  const seen = watchConsole(page);
  await request.get("/__test/legacy-sw/on");
  await openControlledByWorker(page);
  await request.get("/__test/legacy-sw/off");

  await page.reload();
  const img = page.locator('[data-role="picture"]');
  await expect(img, "the picture is back after one reload").toBeVisible({ timeout: 20000 });
  const settled = seen.length;
  const colours = (await coloursSeen(page, '[data-role="picture"]', 2500)).filter((c) => c.length === 1);
  expect(colours.length, `the picture keeps updating (saw ${colours})`).toBeGreaterThanOrEqual(2);

  // Until the new worker serves the page, the OLD worker's proxied picture stream gets aborted (while
  // it is in control, and once more when the new worker takes the reloaded page over), and WebKit
  // reports each abort as a stack-less page error ("", "Cannot load .", "Load failed") next to a
  // failed /interkom.mjpeg request (probed 27.9.2026). That is the broken state and its one-time
  // handover; nothing else may log then, and nothing at all once the picture is back.
  const handover = seen.slice(0, settled).filter((e) => !OLD_WORKER_STREAM_ABORT.has(e));
  expect(handover, "only the old worker's aborted picture stream may log during the upgrade").toEqual([]);
  expect(seen.slice(settled), "the console stays completely clean once the picture is back").toEqual([]);
});
