// The level-sliders E2E (intercom#34) runs the REAL hub binary. Its ports and its scratch dir are
// derived here once, so playwright.config.js, start-levels-hub.js and levels.spec.js agree.
// Ports follow INTERKOM_E2E_PORT (dev1: 8792 is another account's, run with 18792 -> 18793..18795).
const os = require("os");
const path = require("path");

const BASE = Number(process.env.INTERKOM_E2E_PORT || 8792);
const HTTP_PORT = BASE + 1;
const VBAN_PORT = BASE + 2;
const CONTROL_PORT = BASE + 3;

module.exports = {
  HTTP_PORT,
  VBAN_PORT,
  CONTROL_PORT,
  CONTROL_URL: `http://127.0.0.1:${CONTROL_PORT}`,
  // The hub CI's rust job built (downloaded into target/release by the web-e2e job), or a local one.
  HUB_BIN: process.env.INTERCOM_HUB_BIN || path.resolve(__dirname, "../../target/release/intercom-hub"),
  // Fresh per run (start-levels-hub.js empties it): the rendered TOML and the hub's state file.
  RUN_DIR: path.join(os.tmpdir(), `intercom-levels-e2e-${CONTROL_PORT}`),
  TEMPLATE: path.resolve(__dirname, "levels.toml"),
};
