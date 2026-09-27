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
// multipart parts. Runs in the Chromium phone project AND the WebKit iPhone project; every test
// asserts a completely clean console.
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
  await page.evaluate(async () => {
    const reg = await navigator.serviceWorker.ready;
    return reg.active ? reg.active.state : "none";
  });
  await page.reload();
  const controlled = await page.evaluate(async () => {
    await navigator.serviceWorker.ready;
    return !!navigator.serviceWorker.controller;
  });
  expect(controlled, "the service worker controls the page").toBe(true);
}

// The centre-band colour of what an element currently shows, drawn into a canvas: "r" / "g" / "b"
// for one of the three stub frames, "bg" for the dark frame edge, "none" when nothing is decoded.
async function centreColour(page, selector) {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel);
    if (!el) return "none";
    const w = el.naturalWidth || el.videoWidth || 0;
    const h = el.naturalHeight || el.videoHeight || 0;
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
  }, selector);
}

async function coloursSeen(page, selector, ms) {
  const seen = new Set();
  const end = Date.now() + ms;
  while (Date.now() < end) {
    seen.add(await centreColour(page, selector));
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
  // The native video is armed from the picture and really carries it (the canvas mirror of the
  // multipart <img> is drawn into the stream the video plays).
  await expect
    .poll(() => page.evaluate(() => document.querySelector('[data-role="picture-native"]').readyState), {
      timeout: 10000,
    })
    .toBeGreaterThanOrEqual(2);
  await expect
    .poll(() => centreColour(page, '[data-role="picture-native"]'), { timeout: 10000 })
    .toMatch(/^[rgb]$/);

  await page.locator('[data-role="picture-wrap"]').click();
  const calls = await page.evaluate(() => window.__nativeFs);
  expect(calls, "the tap opens the native player exactly once").toHaveLength(1);
  expect(calls[0].role).toBe("picture-native");
  expect(calls[0].hasStream).toBe(true);
  await expect(page.locator('[data-role="picture-wrap"]')).toHaveAttribute("data-fullscreen", "false");
  expect(seen, "browser console must stay completely clean").toEqual([]);
});
