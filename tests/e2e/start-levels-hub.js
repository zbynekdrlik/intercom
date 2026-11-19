// Start the REAL intercom-hub for the level-sliders E2E (intercom#34): render levels.toml into a
// fresh scratch dir, then run the binary on it in the foreground. Playwright's webServer waits for
// the control listener's /api/version and stops this process at the end; the signal is passed on.
// A missing binary fails the run (never a skip).
"use strict";
const fs = require("fs");
const path = require("path");
const { spawn } = require("child_process");
const env = require("./levels-env");

if (!fs.existsSync(env.HUB_BIN)) {
  process.stderr.write(`start-levels-hub: no hub binary at ${env.HUB_BIN} (cargo build --release -p intercom-hub, or set INTERCOM_HUB_BIN)\n`);
  process.exit(1);
}
fs.rmSync(env.RUN_DIR, { recursive: true, force: true });
fs.mkdirSync(env.RUN_DIR, { recursive: true });
const config = path.join(env.RUN_DIR, "levels.toml");
const text = fs
  .readFileSync(env.TEMPLATE, "utf8")
  .replaceAll("@HTTP_PORT@", String(env.HTTP_PORT))
  .replaceAll("@VBAN_PORT@", String(env.VBAN_PORT))
  .replaceAll("@CONTROL_PORT@", String(env.CONTROL_PORT))
  .replaceAll("@LEVELS_FILE@", path.join(env.RUN_DIR, "state.toml"));
fs.writeFileSync(config, text);

const hub = spawn(env.HUB_BIN, ["--config", config, "--turn-config", path.join(env.RUN_DIR, "no-turn.toml")], {
  stdio: "inherit",
  env: { ...process.env, RUST_LOG: process.env.RUST_LOG || "info" },
});
for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => hub.kill(sig));
}
hub.on("exit", (code, signal) => {
  process.exit(code === null ? (signal ? 0 : 1) : code);
});
