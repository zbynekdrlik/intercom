// The forced-relay E2E environment (intercom#42 phase 3): a REAL TURN server (coturn's
// `turnserver` as a user process: $INTERCOM_TURNSERVER or `turnserver` on PATH; CI installs the
// coturn package), a fake Cloudflare TURN key API that mints that server's credentials, and the
// REAL hub with `[webrtc] turn = "cloudflare"` pointed at the fake API (`--turn-api`). A missing
// turnserver FAILS the run, never skips it.
//
// coturn listens and relays on 127.0.0.2 and DENIES 127.0.0.1 as a peer: the hub (bound to
// 127.0.0.1) is reachable through its own relayed candidate only, and a page with
// iceTransportPolicy "relay" has nothing but its own relay. Ports: the base + 7..9 (hub HTTP,
// VBAN, WebRTC UDP), + 10 (coturn on 127.0.0.2), + 11 (the fake key API).
"use strict";
const fs = require("fs");
const path = require("path");
const http = require("http");
const dgram = require("dgram");
const crypto = require("crypto");
const { spawn } = require("child_process");
const { hubAt, BASE } = require("./webrtc-hub");

const TURN_IP = "127.0.0.2";
const TURN_PORT = BASE + 10;
const MINT_PORT = BASE + 11;
const TURN_USER = "e2e-hub";
// A throwaway long-term password per run (coturn and the fake key API share it).
const TURN_PASS = crypto.randomBytes(12).toString("hex");
const KEY_ID = "e2ekey";
const RELAY_MIN = 30000 + (BASE % 200) * 100;

const hub = hubAt({
  httpPort: BASE + 7,
  vbanPort: BASE + 8,
  udpPort: BASE + 9,
  name: "webrtc-turn",
  webrtcExtra: 'turn = "cloudflare"',
  args: (runDir) => ["--turn-config", path.join(runDir, "turn.toml"), "--turn-api", `http://127.0.0.1:${MINT_PORT}`],
});

/** True once coturn answers a STUN Binding request. */
function stunAnswers() {
  return new Promise((resolve) => {
    const sock = dgram.createSocket("udp4");
    const req = Buffer.alloc(20);
    req.writeUInt16BE(0x0001, 0); // Binding request
    req.writeUInt32BE(0x2112a442, 4);
    crypto.randomBytes(12).copy(req, 8);
    const timer = setTimeout(() => {
      sock.close();
      resolve(false);
    }, 200);
    sock.on("message", () => {
      clearTimeout(timer);
      sock.close();
      resolve(true);
    });
    sock.on("error", () => {
      clearTimeout(timer);
      sock.close();
      resolve(false);
    });
    sock.send(req, TURN_PORT, TURN_IP);
  });
}

async function startCoturn(runDir) {
  const bin = process.env.INTERCOM_TURNSERVER || "turnserver";
  fs.mkdirSync(runDir, { recursive: true });
  const child = spawn(
    bin,
    [
      "-n",
      `--listening-ip=${TURN_IP}`,
      `--listening-port=${TURN_PORT}`,
      `--relay-ip=${TURN_IP}`,
      `--min-port=${RELAY_MIN}`,
      `--max-port=${RELAY_MIN + 99}`,
      "--lt-cred-mech",
      "--realm=intercom.test",
      `--user=${TURN_USER}:${TURN_PASS}`,
      "--no-tls",
      "--no-dtls",
      "--no-tcp",
      "--no-tcp-relay",
      "--allow-loopback-peers",
      "--denied-peer-ip=127.0.0.1",
      "--no-cli",
      "--no-software-attribute",
      "--simple-log",
      `--log-file=${path.join(runDir, "turn.log")}`,
      `--db=${path.join(runDir, "turndb")}`,
      `--pidfile=${path.join(runDir, "turn.pid")}`,
    ],
    { stdio: "ignore" }
  );
  let failed = null;
  child.on("error", (e) => {
    failed = e;
  });
  child.on("exit", (code, signal) => {
    failed = failed || new Error(`turnserver exited (${code} ${signal})`);
  });
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline) {
    if (failed) throw new Error(`TEST FAILED: cannot run coturn's turnserver (${bin}): ${failed.message}`);
    if (await stunAnswers()) return child;
    await new Promise((r) => setTimeout(r, 100));
  }
  child.kill("SIGKILL");
  throw new Error(`TEST FAILED: turnserver did not answer on ${TURN_IP}:${TURN_PORT}`);
}

/** Cloudflare's generate-ice-servers, answering this coturn's long-term credential. */
function startMint() {
  const server = http.createServer((req, res) => {
    if (req.method === "POST" && req.url === `/${KEY_ID}/credentials/generate-ice-servers`) {
      req.resume();
      res.writeHead(201, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          iceServers: [
            {
              urls: [`turn:${TURN_IP}:${TURN_PORT}?transport=udp`],
              username: TURN_USER,
              credential: TURN_PASS,
            },
          ],
        })
      );
      return;
    }
    res.writeHead(404);
    res.end();
  });
  return new Promise((resolve) => server.listen(MINT_PORT, "127.0.0.1", () => resolve(server)));
}

/** coturn + the fake key API + the hub with turn = "cloudflare". Returns a handle with stop(). */
async function startRelayEnv() {
  hub.renderConfig();
  const turn = await startCoturn(hub.RUN_DIR);
  const mint = await startMint();
  const keyFile = path.join(hub.RUN_DIR, "turn.toml");
  // A throwaway token per run: the fake key API never checks it.
  const token = crypto.randomBytes(16).toString("hex");
  fs.writeFileSync(keyFile, `key_id = "${KEY_ID}"\nkey_api_token = "${token}"\n`, { mode: 0o600 });
  fs.chmodSync(keyFile, 0o600);
  const h = await hub.startHub();
  return {
    async stop() {
      await h.stop();
      turn.kill("SIGTERM");
      await new Promise((r) => mint.close(r));
    },
  };
}

module.exports = { startRelayEnv, hub, TURN_IP, TURN_PORT };
