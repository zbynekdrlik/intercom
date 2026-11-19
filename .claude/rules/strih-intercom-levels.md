---
paths:
  - "hub/src/levels.rs"
  - "hub/src/levels_http.rs"
  - "hub/src/engine.rs"
  - "hub/src/http_serve.rs"
  - "hub/tests/levels_34.rs"
  - "web/levels.*"
  - "tests/e2e/levels*"
  - "tests/e2e/start-levels-hub.js"
  - "intercom.strih-snv.toml"
  - "intercom.strih-pp.toml"
  - "scripts/vbmatrix_to_intercom_toml.py"
  - "tray/**"
---

# The cutters' level sliders (intercom#34)

Owner 19.11.2026: FOH sends REPRO at 0 dB now, so the program came into the cutters' cans much
hotter than the cameras, and nothing let the cutter rebalance. Decision (comment 7008303820): a
cutter-adjustable per-source level control on the hub. Design: comment 7008323559.

## How it works

- **Config: `[[level]]` tables** in the routing TOML: `name` (a-z 0-9 _ -, it is the API path),
  `label` (Slovak, shown), `dst` (an output participant), `sources` (`"lv1"` = every channel of a
  participant, `"foh_dante:3"` = one input channel), optional `min_db` / `max_db` (default -60 / +12,
  must hold 0, within -120 .. +24). `levels::resolve` (called by `Matrix::from_toml`) sets
  `Point::level` on every point from a source into `dst`. Refused at load: a bad / repeated name, an
  empty label, an unknown participant or channel, a `dst` without outputs, a source with no point
  into `dst`, a point two sliders claim (also within one slider), a bad range, an unknown key.
- **The trim is on top of the TOML's `gain_db`.** `Engine::mix_block` multiplies `gain_linear` by
  `Levels::linear(i)`: one relaxed `AtomicU32` load (f32 bits) per point per block, no lock. At 0 dB
  the factor is exactly 1.0, so the mix is bit-identical to a matrix without sliders (pinned); muted =
  0.0 and the point is skipped. Never add a lock or a log to that read.
- **`Matrix::levels` is an `Arc`:** every clone of the matrix (the engine's, the block loop's, the
  snapshot's) reads the same live values. The writer lock (`Levels::writer`) orders a change with its
  file write; only the HTTP side takes it. The dB and the mute share ONE `AtomicU64` (f32 bits + bit
  32), so `/api/levels` never shows a new mute with an old dB; `linear` is its own `AtomicU32`.
- **`HubState.levels` is the `Levels` itself** (an `Arc` clone on the hub-mix thread, no
  allocation); its `Serialize` reads the values when `/api/state` or `/ws` serializes, off the
  real-time thread. Never put `levels.states()` back into `HubState::snapshot`.
- **The deployed sliders:** strih-snv Program = `foh_dante:1`, `foh_dante:2`, `lv1` (8 points) ·
  Click = `foh_dante:3` (4) · Kamery = cam1..cam7 (28) · Telefóny = `phones` (4); strih-pp Program =
  `dsg_dante` (2) · Kamery (14) · Telefóny (2). Every cutters point on both boxes is trimmed by exactly
  one slider (pinned in `levels_34.rs`). strih-snv's come from the converter's `LEVELS` (its
  `_check_level` fails the generation on a source with no point into dst); strih-pp's are hand-edited
  at the end of its TOML. A new cutters point needs its slider in the same change.

## The control listener (loopback only)

- `[hub] control_bind` (default `127.0.0.1:8791`); a non-loopback address fails the load (the
  controls have no login). The box's nginx and the public phone front proxy only `:8790`, so no
  phone and no internet client reaches it. Started only when the matrix has a `[[level]]`
  (`levels_http::start_level_controls`, called from `main.rs`), through the same
  `http_serve::serve` + Dante guard + `check_bind`. A taken port is an error line and the hub runs
  on without sliders, never stopping the audio; a `control_bind` the Dante guard refuses still
  stops it at start (review findings, pinned in `levels_34.rs`). The stream boxes have no sliders
  and no :8791.
- **DNS rebinding guard:** loopback stops other machines, not a web page in the notebook's own
  browser that re-binds its name to 127.0.0.1. A middleware (`loopback_host_only`) answers 403 unless
  the `Host` (or the HTTP/2 authority) is `127.0.0.1` / `localhost` / `[::1]` (`is_loopback_host`) and
  an `Origin`, when sent, is `http://` one of those. A PUT with JSON also needs a CORS preflight,
  which the routes never answer.
- **The cutter opens the page from the tray icon** (`intercom-tray`, below: "Otvoriť hlasitosti");
  it is `http://127.0.0.1:8791/` on the strih notebook (their cans hang on its card). The page: one
  vertical slider (0.5 dB steps, arrows work) + mute per slider with its dB, the hub version
  (`v<semver>`), and a 1 Hz meter of the trimmed output (the cutters' peak dBFS after the sliders).
  It never writes to the console: failures are the status line.
- Routes: `GET /`, `/levels.js`, `/levels.css`, `/favicon.svg`, `/api/version`, `GET /api/levels`
  (`{version, levels: [{name, label, dst, sources, db, muted, min_db, max_db, points}], outputs:
  [{participant, out_level_dbfs}]}`), `PUT /api/levels/{name}` with exactly `{"db": f32, "muted":
  bool}` -> 200 with the slider + `saved`, 400 for a bad body or a value outside the range (nothing
  changes), 404 for an unknown name. Each change logs `level <name> <old> -> <new> dB (muted=..)
  from <peer>`; `http_serve` attaches the peer as `ConnectInfo` on every connection for that.
- `/api/state` (main listener) gains, additively, a read-only `levels` array (omitted without
  sliders) and per participant with outputs `out_level_dbfs` (the max of `OutputBlock::peak_abs`
  over the status second, on the hub-mix thread, into a vector allocated once). No existing field
  changed.

## The state file

- `[hub] levels_file` (default `/var/lib/intercom-hub/levels.toml`, the unit's
  `StateDirectory=intercom-hub`, beside dlink's state file; no unit change was needed). Shape:
  `[levels.<name>]` `db = ..` `muted = ..`.
- Read once at start (`Levels::load_file`): known names take their value clamped into the range
  (warned); unknown names, a parse error, an unreadable file are warned and ignored; a missing file
  keeps 0 dB. Never fatal. `--check-config` never touches it.
- Written on every change by `write_atomic` (`.<name>.tmp` beside it, fsync, rename, fsync the
  directory) in
  `spawn_blocking` under the writer lock. A failed write is a warn naming the path; the value still
  holds until the next restart and the answer says `saved: false` (the page shows it). The dB is
  stored as the f32's shortest decimal (`-7.3`, not toml's f64 widening).

## Tests

- `hub/tests/levels_34.rs`: the resolution and every refusal, both deployed TOMLs' sliders (and the
  stream boxes' none), the engine (0 dB bit identity, an exact trim, a boost on top of the TOML gain,
  mute only its points), the state file (round trip, clamp, unknown, corrupt, missing, the atomic
  write), `/api/state` facets, and the control routes on a real `http_serve` listener (reqwest).
- `tests/python/test_vbmatrix_to_intercom_toml_1345.py`: the emitted sliders cover every cutters
  point once; a source with no point fails the generation.
- **Playwright on the REAL hub** (`tests/e2e/levels.spec.js`, project `levels`): `start-levels-hub.js`
  renders `levels.toml` (loopback ports from `levels-env.js`, `INTERKOM_E2E_PORT` + 1..3, the state
  file in a fresh `$TMPDIR/intercom-levels-e2e-<port>`) and runs `target/release/intercom-hub`
  (`INTERCOM_HUB_BIN` overrides). The spec drags a slider, steps it with the keyboard, toggles a
  mute, reads `/api/levels` and the state file back, checks the version shown and a clean console;
  a third test feeds the hub's `prog` leg a real VBAN tone (`vban-tone.js`, the header as
  `VbanHeader::encode` writes it, paced on the wall clock) and watches the page's meter rise, go to
  `ticho` when both sliders are muted, and rise again. One hub serves all tests, so a `beforeEach`
  resets both sliders to 0 dB (without it a `--repeat-each` run fails on the previous test's mute).
  CI: `web-e2e` `needs: rust` and downloads its `intercom-hub-linux-amd64` artifact (chmod 0755: an
  artifact drops the mode).
- Local: `cargo build --release -p intercom-hub`, then `cd tests/e2e && INTERKOM_E2E_PORT=18792 npx
  playwright test --project=levels` (from a script file). Playwright starts EVERY `webServer` for
  any `--project`, so even a `--project=phone` run needs the release binary (or
  `INTERCOM_HUB_BIN`); a missing one fails the run loudly, never skips.
- Gotcha: a vertical range (`writing-mode: vertical-lr; direction: rtl`) puts max at the top; a
  Playwright drag from the bottom edge first lands mid-range (the first `input` read about -28 dB on
  Chromium 131), so assert the end value, not the path.

## The tray icon `intercom-tray` (owner 19.11.2026: "ikonka na otvorenie dole v bare")

- **What:** `tray/` (its own Cargo workspace, see `.claude/rules/release-ci.md`), dstack's dtray
  pattern: `tray-icon` =0.21.2 without default features + `gtk` =0.18.2 / `glib` =0.18.5. The strih
  boxes run Xorg + openbox with tint2's systray, where dtray's icon already shows (camera-box
  19.11.2026). AppIndicator gives no click on the icon itself, so the page opens from the menu.
- **Menu:** the status line (`Hub v<x> beží` / `Hub nedostupný (<why>)`, read from the control
  listener's `/api/version` every 2 s on its own thread, with a loopback `Host` for the rebinding
  guard), **Otvoriť hlasitosti**, **Kopírovať URL**, **Ukončiť ikonu (hub beží ďalej)**. The icon is
  drawn in code: headphones, green while the hub answers, grey otherwise.
- **Open:** `lib::choose_launch`: Firefox `--new-window` first (the boxes' only browser is the snap
  `/snap/bin/firefox`; `/snap/bin` is searched even off `PATH`), then Chrome / Chromium `--app=`,
  then `xdg-open`. The choice and the launcher's exit are logged.
- **URL:** `--url` (default `http://127.0.0.1:8791/`), refused unless `http://<loopback>:<port>`.
  One tray per session (`$XDG_RUNTIME_DIR/intercom-tray.lock`).
- **Ships** in the hub tarball; camera-box installs and starts it as a session app like
  `dtray.service` (its `strih-session-apps.sh`).
- **Tests:** `tray/tests/tray_34.rs` (the URL, the browser order on the boxes' real layout, the
  status read against a local listener, the icon, the lock path, the version equal to the root's).

