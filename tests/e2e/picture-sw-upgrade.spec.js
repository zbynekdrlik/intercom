// @ts-check
// A phone that still runs the OLD interkom service worker takes the new one (issue 1379).
//
// Every installed phone is in this state when the fixed hub goes live: its worker is the one that
// proxied every request, the endless /interkom.mjpeg picture stream included, which WebKit (iPhone
// Safari) never hands to the page's <img>. The next update check must install the new worker and
// hand it the page, and the next load must show the picture with a completely clean console.
//
// WebKit only (the iphone-webkit project; the Chromium phone project ignores this file): the old
// worker never broke the picture in Chromium, and Chromium keeps the new skip-waiting worker in
// `waiting` for as long as the old one serves the endless stream, activating it only once the old
// worker is idle (probed 27.9.2026: still `installed`/waiting 5 s after `update()`, activated with a
// controllerchange within 3 s of the <img> stopping). So the WebKit-shaped swap below does not apply
// there; an Android phone swaps on its next navigation, which tears the old stream down.
//
// The browser runs the update check on the next navigation; the test runs it explicitly with
// `reg.update()` and waits for the swap before it reloads. A reload that STARTS under the old worker
// races the swap: headless WebKit then closes the old worker's context under the reload's own
// requests ("Service Worker context closed", about 1 in 60 runs), and while the old worker proxies
// the picture it logs a varying set of errors for every failed picture request (page errors
// "" / "Cannot load ." / "Load failed", sometimes a console "Failed to load resource: ..."). That
// noise belongs to the old worker, so nothing is asserted about the console before the swap. The
// new worker's own console is proven completely clean from a fresh load by picture-sw.spec.js.
const { test, expect } = require("@playwright/test");
const { preparePicturePage, watchConsole, openControlledByWorker, pictureColours } = require("./picture-helpers");

test.beforeEach(async ({ page }) => {
  await preparePicturePage(page);
});

// This test switches the stub to the old worker; never let that leak into the next test.
test.afterEach(async ({ request }) => {
  await request.get("/__test/legacy-sw/off");
});

test("a phone still on the old proxy-everything worker takes the new one and the next load shows the picture (issue 1379)", async ({ page, request }) => {
  const seen = watchConsole(page);
  const isPicture = (req) => new URL(req.url()).pathname === "/interkom.mjpeg";
  let failedPictures = 0;
  page.on("requestfailed", (req) => {
    if (isPicture(req)) failedPictures += 1;
  });

  const on = await request.get("/__test/legacy-sw/on");
  expect(on.ok(), "the stub serves the old worker").toBe(true);
  await openControlledByWorker(page);
  // Prove the OLD worker really breaks the picture in the page it controls, or this test would
  // quietly become a copy of the first test in picture-sw.spec.js. Count only from here: the reload
  // inside openControlledByWorker cancels the first, uncontrolled document's stream, which is also
  // reported as a failed request.
  const failedWhenControlled = failedPictures;
  await expect
    .poll(() => failedPictures - failedWhenControlled, { timeout: 15000, message: "the old worker breaks the picture" })
    .toBeGreaterThanOrEqual(1);
  const off = await request.get("/__test/legacy-sw/off");
  expect(off.ok(), "the stub serves the new worker again").toBe(true);

  // The update check: /sw.js now has different bytes, so the new worker installs, skips waiting,
  // activates and claims the page (a controllerchange in this document).
  const swapped = await page.evaluate(async () => {
    const reg = await navigator.serviceWorker.getRegistration();
    const changed = new Promise((resolve) => {
      navigator.serviceWorker.addEventListener("controllerchange", () => resolve(true), { once: true });
    });
    await reg.update();
    return Promise.race([changed, new Promise((resolve) => setTimeout(() => resolve(false), 10000))]);
  });
  expect(swapped, "the new worker takes the page over").toBe(true);

  // The running page heals by itself: its 5 s picture retry now goes past the new worker. Once the
  // picture is back, this document too must stay clean with no failed picture request.
  const img = page.locator('[data-role="picture"]');
  await expect(img, "the running page shows the picture again").toBeVisible({ timeout: 15000 });
  const seenAtHeal = seen.length;
  const failedAtHeal = failedPictures;
  const healed = await pictureColours(page, 2500);
  expect(healed.length, `the healed picture keeps updating (saw ${healed})`).toBeGreaterThanOrEqual(2);
  expect(seen.slice(seenAtHeal), "the console stays clean once the running page has healed").toEqual([]);
  expect(failedPictures - failedAtHeal, "no picture request fails once the running page has healed").toBe(0);

  // The next load: from its commit on, the console must stay completely clean and no picture
  // request may fail.
  let seenAtCommit = -1;
  let failedAtCommit = -1;
  const onNavigated = (frame) => {
    if (frame !== page.mainFrame()) return;
    seenAtCommit = seen.length;
    failedAtCommit = failedPictures;
  };
  page.on("framenavigated", onNavigated);
  await page.reload();
  page.off("framenavigated", onNavigated);
  expect(seenAtCommit, "the reload committed a new document").toBeGreaterThanOrEqual(0);

  await expect(img, "the next load shows the picture").toBeVisible({ timeout: 15000 });
  const colours = await pictureColours(page, 2500);
  expect(colours.length, `the picture keeps updating (saw ${colours})`).toBeGreaterThanOrEqual(2);
  expect(seen.slice(seenAtCommit), "the console stays completely clean on the next load").toEqual([]);
  expect(failedPictures - failedAtCommit, "no picture request fails on the next load").toBe(0);
});
