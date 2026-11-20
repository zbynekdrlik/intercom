// The REAL intercom-hub for the in-hub WebRTC phone E2E (intercom#42): render webrtc.toml into a
// fresh scratch dir and start / stop the binary from the spec itself, so a test can kill the hub
// and start it again on the same ports. A missing binary fails the run (never a skip).
// Ports follow INTERKOM_E2E_PORT: the base + 4 (HTTP), + 5 (VBAN), + 6 (the WebRTC UDP socket);
// the level-sliders hub holds the base + 1..3. The forced-relay hub (phase 3, webrtc-turn-env.js)
// is a second instance from `hubAt` on the base + 7..9.
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const http = require("http");
const { spawn } = require("child_process");

const BASE = Number(process.env.INTERKOM_E2E_PORT || 8792);
const HUB_BIN = process.env.INTERCOM_HUB_BIN || path.resolve(__dirname, "../../target/release/intercom-hub");
const TEMPLATE = path.resolve(__dirname, "webrtc.toml");

/**
 * A hub on its own ports and scratch dir. `webrtcExtra` is appended to the `[webrtc]` table,
 * `args` (a list, or a function of the run dir) replace the default TURN arguments (no key file =
 * TURN off).
 */
function hubAt({ httpPort, vbanPort, udpPort, name, webrtcExtra = "", args = null }) {
  const runDir = path.join(os.tmpdir(), `intercom-${name}-e2e-${httpPort}`);
  const baseUrl = `http://127.0.0.1:${httpPort}`;

  function renderConfig() {
    fs.rmSync(runDir, { recursive: true, force: true });
    fs.mkdirSync(runDir, { recursive: true });
    const config = path.join(runDir, "webrtc.toml");
    let text = fs
      .readFileSync(TEMPLATE, "utf8")
      .replaceAll("@HTTP_PORT@", String(httpPort))
      .replaceAll("@VBAN_PORT@", String(vbanPort))
      .replaceAll("@UDP_PORT@", String(udpPort));
    if (webrtcExtra) text = text.replace("[webrtc]\n", `[webrtc]\n${webrtcExtra}\n`);
    fs.writeFileSync(config, text);
    return config;
  }

  function getVersion() {
    return new Promise((resolve) => {
      const req = http.get(`${baseUrl}/api/version`, (res) => {
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
    const config = fs.existsSync(path.join(runDir, "webrtc.toml")) ? path.join(runDir, "webrtc.toml") : renderConfig();
    const log = fs.openSync(path.join(runDir, "hub.log"), "a");
    const extra =
      typeof args === "function" ? args(runDir) : args || ["--turn-config", path.join(runDir, "no-turn.toml")];
    const child = spawn(HUB_BIN, ["--config", config, ...extra], {
      stdio: ["ignore", log, log],
      env: { ...process.env, RUST_LOG: process.env.RUST_LOG || "info" },
    });
    let exited = null;
    child.on("exit", (code, signal) => {
      exited = { code, signal };
    });
    const deadline = Date.now() + 20000;
    while (Date.now() < deadline) {
      if (exited) throw new Error(`the hub exited at start (${JSON.stringify(exited)}); see ${runDir}/hub.log`);
      if (await getVersion()) break;
      await new Promise((r) => setTimeout(r, 100));
    }
    if (!(await getVersion())) {
      child.kill("SIGKILL");
      throw new Error(`the hub did not answer within 20 s; see ${runDir}/hub.log`);
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

  return { startHub, renderConfig, HTTP_PORT: httpPort, VBAN_PORT: vbanPort, UDP_PORT: udpPort, BASE_URL: baseUrl, RUN_DIR: runDir };
}

// The phases 1 + 2 hub (webrtc.spec.js).
const main = hubAt({ httpPort: BASE + 4, vbanPort: BASE + 5, udpPort: BASE + 6, name: "webrtc" });

module.exports = { ...main, hubAt, BASE };
