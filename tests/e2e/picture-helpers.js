// @ts-check
// Shared helpers for the interkom picture specs that run with the PWA service worker REGISTERED
// (issue 1379): picture-sw.spec.js and picture-sw-upgrade.spec.js. Not a spec file itself.
const path = require("path");
const { expect } = require("@playwright/test");

// Before app.js runs: the fake Janus server (keeps the page's audio side quiet; these specs do not
// test audio) and a remembered name (so the name sheet stays closed). Unlike phone.spec.js, the
// service-worker registration is NOT neutralised.
async function preparePicturePage(page) {
  await page.addInitScript({ path: path.join(__dirname, "fake-janus-server.js") });
  await page.addInitScript(() => {
    try {
      localStorage.setItem("interkom.display", "Kamera 8");
    } catch (e) {
      // private mode: the name sheet shows, which does not affect the picture
    }
  });
}

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

// The distinct centre colours seen over `ms`, sampled every 150 ms.
async function coloursSeen(page, target, ms) {
  const seen = new Set();
  const end = Date.now() + ms;
  while (Date.now() < end) {
    seen.add(await centreColour(page, target));
    await page.waitForTimeout(150);
  }
  return [...seen].sort();
}

// The picture stream's own colours (one letter each) over `ms`.
async function pictureColours(page, ms) {
  return (await coloursSeen(page, '[data-role="picture"]', ms)).filter((c) => c.length === 1);
}

module.exports = {
  preparePicturePage,
  watchConsole,
  openControlledByWorker,
  NATIVE_MIRROR,
  centreColour,
  coloursSeen,
  pictureColours,
};
