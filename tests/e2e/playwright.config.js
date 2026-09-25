// @ts-check
// Playwright config for the phone interkom PWA E2E (issue 1345, the 25.9.2026 phone UX rework).
// One webServer: the stdlib stub hub (stub_hub.py) that serves the REAL intercom/web client plus a
// stubbed hub API and the fake Janus. Chromium only, one worker, a phone-sized viewport, and the
// Chromium fake-media flags so both level meters have a real (fake-device) audio signal.
//
// Autoplay is ALLOWED here (an installed PWA / a site the phone already trusts). Playwright-driven
// Chromium plays media without a gesture whatever --autoplay-policy says (probed 25.9.2026: a plain
// WAV and a MediaStream both play, headless shell AND headed), so the blocked-autoplay test
// emulates the browser policy itself (see phone.spec.js) instead of a second project.
const { defineConfig, devices } = require("@playwright/test");

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
    { name: "phone", use: phone() },
  ],
  webServer: [
    {
      command: `python3 ${require("path").resolve(__dirname, "stub_hub.py")} --port ${PORT}`,
      url: `http://127.0.0.1:${PORT}/__health`,
      reuseExistingServer: false,
      timeout: 20000,
    },
  ],
});
