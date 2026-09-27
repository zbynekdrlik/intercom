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
// asserts a completely clean console. The upgrade from the old worker is picture-sw-upgrade.spec.js.
const { test, expect } = require("@playwright/test");
const {
  preparePicturePage,
  watchConsole,
  openControlledByWorker,
  NATIVE_MIRROR,
  coloursSeen,
  pictureColours,
} = require("./picture-helpers");

test.beforeEach(async ({ page }) => {
  await preparePicturePage(page);
});

test("with the service worker controlling the page the picture decodes and keeps moving (issue 1379)", async ({ page }) => {
  const seen = watchConsole(page);
  await openControlledByWorker(page);

  const img = page.locator('[data-role="picture"]');
  await expect(img, "the picture replaces the placeholder").toBeVisible({ timeout: 15000 });
  await expect(page.locator('[data-role="picture-placeholder"]')).toBeHidden();
  const size = await img.evaluate((el) => [el.naturalWidth, el.naturalHeight]);
  expect(size, "a decoded 320x180 frame").toEqual([320, 180]);

  // A live stream, not one frozen part: the centre band changes colour as the parts arrive.
  const colours = await pictureColours(page, 2500);
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
