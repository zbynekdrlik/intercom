// The REAL intercom-hub for the in-hub WebRTC phone E2E (intercom#42): render webrtc.toml into a
// fresh scratch dir and start / stop the binary from the spec itself, so a test can kill the hub
// and start it again on the same ports. A missing binary fails the run (never a skip).
// Ports follow INTERKOM_E2E_PORT: the base + 4 (HTTP), + 5 (VBAN), + 6 (the WebRTC UDP socket);
// the level-sliders hub holds the base + 1..3.
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const http = require("http");
const { spawn } = require("child_process");

const BASE = Number(process.env.INTERKOM_E2E_PORT || 8792);
const HTTP_PORT = BASE + 4;
const VBAN_PORT = BASE + 5;
const UDP_PORT = BASE + 6;
const HUB_BIN = process.env.INTERCOM_HUB_BIN || path.resolve(__dirname, "../../target/release/intercom-hub");
const RUN_DIR = path.join(os.tmpdir(), `intercom-webrtc-e2e-${HTTP_PORT}`);
const TEMPLATE = path.resolve(__dirname, "webrtc.toml");
const BASE_URL = `http://127.0.0.1:${HTTP_PORT}`;

function renderConfig() {
  fs.rmSync(RUN_DIR, { recursive: true, force: true });
  fs.mkdirSync(RUN_DIR, { recursive: true });
  const config = path.join(RUN_DIR, "webrtc.toml");
  const text = fs
    .readFileSync(TEMPLATE, "utf8")
    .replaceAll("@HTTP_PORT@", String(HTTP_PORT))
    .replaceAll("@VBAN_PORT@", String(VBAN_PORT))
    .replaceAll("@UDP_PORT@", String(UDP_PORT));
  fs.writeFileSync(config, text);
  return config;
}

function getVersion() {
  return new Promise((resolve) => {
    const req = http.get(`${BASE_URL}/api/version`, (res) => {
      res.resume();
      resolve(res.statusCode === 200);
    });
    req.on("error", () => resolve(false));
    req.setTimeout(500, () => {
      req.destroy();
      resolve(false);
    });
  });
}

/** Start the hub; resolves once /api/version answers. Returns a handle with stop(). */
async function startHub() {
  if (!fs.existsSync(HUB_BIN)) {
    throw new Error(`no hub binary at ${HUB_BIN} (cargo build --release -p intercom-hub, or set INTERCOM_HUB_BIN)`);
  }
  const config = fs.existsSync(path.join(RUN_DIR, "webrtc.toml")) ? path.join(RUN_DIR, "webrtc.toml") : renderConfig();
  const log = fs.openSync(path.join(RUN_DIR, "hub.log"), "a");
  const child = spawn(HUB_BIN, ["--config", config, "--turn-config", path.join(RUN_DIR, "no-turn.toml")], {
    stdio: ["ignore", log, log],
    env: { ...process.env, RUST_LOG: process.env.RUST_LOG || "info" },
  });
  let exited = null;
  child.on("exit", (code, signal) => {
    exited = { code, signal };
  });
  const deadline = Date.now() + 20000;
  while (Date.now() < deadline) {
    if (exited) throw new Error(`the hub exited at start (${JSON.stringify(exited)}); see ${RUN_DIR}/hub.log`);
    if (await getVersion()) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  if (!(await getVersion())) {
    child.kill("SIGKILL");
    throw new Error(`the hub did not answer within 20 s; see ${RUN_DIR}/hub.log`);
  }
  return {
    child,
    /** Stop it (SIGKILL = a crash, the default SIGTERM = a clean stop); resolves once it exited. */
    stop(signal = "SIGTERM") {
      return new Promise((resolve) => {
        if (exited) return resolve();
        child.once("exit", () => resolve());
        child.kill(signal);
      });
    },
  };
}

module.exports = { startHub, renderConfig, HTTP_PORT, VBAN_PORT, UDP_PORT, BASE_URL, RUN_DIR };
