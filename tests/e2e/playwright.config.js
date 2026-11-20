// @ts-check
// Playwright config for the phone interkom PWA E2E (issue 1345, the 25.9.2026 phone UX rework).
// One webServer: the stdlib stub hub (stub_hub.py) that serves the REAL web client plus a
// stubbed hub API and the fake Janus. One worker. The `phone` project is Chromium at a phone-sized
// viewport with the Chromium fake-media flags, so both level meters have a real (fake-device) audio
// signal; it runs every spec but the WebKit-only upgrade spec. The `iphone-webkit` project (issue
// 1379) is the iPhone engine and runs only picture-sw.spec.js (the picture with the service worker in
// control -- a WebKit-only failure that no Chromium run can catch) and picture-sw-upgrade.spec.js.
//
// Autoplay is ALLOWED here (an installed PWA / a site the phone already trusts). Playwright-driven
// Chromium plays media without a gesture whatever --autoplay-policy says (probed 25.9.2026: a plain
// WAV and a MediaStream both play, headless shell AND headed), so the blocked-autoplay test
// emulates the browser policy itself (see phone.spec.js) instead of a second project.
//
// intercom#34: the `levels` project runs levels.spec.js against the REAL hub binary (the second
// webServer, start-levels-hub.js: the cutter's loopback level page). The phone projects ignore it.
const { defineConfig, devices } = require("@playwright/test");
const levelsEnv = require("./levels-env");

const PORT = Number(process.env.INTERKOM_E2E_PORT || 8792);

function phone() {
  return {
    ...devices["Desktop Chrome"],
    viewport: { width: 390, height: 844 },
    deviceScaleFactor: 2,
    isMobile: true,
    hasTouch: true,
    permissions: ["microphone"],
    launchOptions: {
      // A local run may borrow an already-installed Chromium whose revision differs from this
      // package's pinned one (INTERKOM_E2E_CHROMIUM=<path to chrome>); CI installs the pinned one.
      executablePath: process.env.INTERKOM_E2E_CHROMIUM || undefined,
      args: [
        "--use-fake-ui-for-media-stream",
        "--use-fake-device-for-media-stream",
        "--autoplay-policy=no-user-gesture-required",
      ],
    },
  };
}

module.exports = defineConfig({
  testDir: ".",
  timeout: 45000,
  fullyParallel: false,
  workers: 1,
  reporter: "line",
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "off",
  },
  projects: [
    // Every spec except the WebKit-only worker-upgrade spec (see its header).
    { name: "phone", use: phone(), testIgnore: /(picture-sw-upgrade|levels)\.spec\.js$/ },
    // The iPhone engine (issue 1379): WebKit at the iPhone 14 shape. Only the picture/service-worker
    // specs run here -- the audio specs lean on Chromium's fake media devices, which WebKit lacks.
    { name: "iphone-webkit", use: { ...devices["iPhone 14"] }, testMatch: /picture-sw(-upgrade)?\.spec\.js$/ },
    // The cutter's level page on the notebook (intercom#34): a desktop Chromium on the real hub.
    {
      name: "levels",
      testMatch: /levels\.spec\.js$/,
      use: {
        ...devices["Desktop Chrome"],
        baseURL: levelsEnv.CONTROL_URL,
        launchOptions: { executablePath: process.env.INTERKOM_E2E_CHROMIUM || undefined },
      },
    },
    {
      name: "levels-webkit",
      testMatch: /levels\.spec\.js$/,
      // The tray's mix window is WebKitGTK (intercom#39): the fader geometry runs in WebKit too.
      grep: /every engine/,
      use: {
        ...devices["Desktop Safari"],
        baseURL: levelsEnv.CONTROL_URL,
      },
    },
  ],
  webServer: [
    {
      command: `python3 ${require("path").resolve(__dirname, "stub_hub.py")} --port ${PORT}`,
      url: `http://127.0.0.1:${PORT}/__health`,
      reuseExistingServer: false,
      timeout: 20000,
    },
    {
      command: `node ${require("path").resolve(__dirname, "start-levels-hub.js")}`,
      url: `${levelsEnv.CONTROL_URL}/api/version`,
      reuseExistingServer: false,
      timeout: 30000,
      stdout: "pipe",
    },
  ],
});
