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

# The cutters' level sliders (intercom#34, per source + master since intercom#38)

Owner 19.11.2026: FOH sends REPRO at 0 dB now, so the program came into the cutters' cans much
hotter than the cameras, and nothing let the cutter rebalance. Decision (comment 7008303820): a
cutter-adjustable per-source level control on the hub. Design: comment 7008323559.
Owner 20.11.2026 (intercom#38): "strihac potrebuje lepsi mix kde vie nastavit viacej hlasitosti nie
len tie zakladne": the four #34 groups became one slider per source plus a master. Design: #38
comment 7038129667.

## How it works

- **Config: `[[level]]` tables** in the routing TOML: `name` (a-z 0-9 _ -, it is the API path),
  `label` (Slovak, shown), `dst` (an output participant), `sources` (`"lv1"` = every channel of a
  participant, `"foh_dante:3"` = one input channel), optional `min_db` / `max_db` (default -60 / +12,
  must hold 0, within -120 .. +24). `levels::resolve` (called by `Matrix::from_toml`) sets
  `Point::level` on every point from a source into `dst`. Refused at load: a bad / repeated name, an
  empty label, an unknown participant or channel, a `dst` without outputs, a source with no point
  into `dst`, a point two sliders claim (also within one slider), a bad range, an unknown key.
- **The master (intercom#38):** `master = true` and no `sources`: it trims the WHOLE `dst` output on
  top of the per-source sliders (effective = point gain x source trim x master). It claims no point
  (`Point::level` stays the source slider's), `points` = every point into `dst`. Refused: a master
  with sources, a second master for one `dst`, a `dst` with no point.
- **`from = "<old name>"` (intercom#38, the state-file migration):** a slider the state file does
  not name starts at that old slider's stored value (clamped into its range). Refused: a `from` that
  is not a valid name or names a slider of the same config.
- **The trim is on top of the TOML's `gain_db`.** `Engine::mix_block` multiplies `gain_linear` by
  `Levels::linear(i)`: one relaxed `AtomicU32` load (f32 bits) per point per block, no lock. At 0 dB
  the factor is exactly 1.0, so the mix is bit-identical to a matrix without sliders (pinned); muted =
  0.0 and the point is skipped. Never add a lock or a log to that read.
- **The master is applied after the point sum** (`Engine::mix_block`): `Engine::new` builds
  `Levels::master_of` (per participant id, the master slot) once; per block one relaxed load per
  output participant with a master, the f32 planes multiplied before the round / clamp, skipped at
  exactly 1.0 (0 dB bit-identical, pinned). Rejected: a per-point master factor (one load per point
  per block, 44 on strih-snv, for the same result).
- **`Matrix::levels` is an `Arc`:** every clone of the matrix (the engine's, the block loop's, the
  snapshot's) reads the same live values. The writer lock (`Levels::writer`) orders a change with its
  file write; only the HTTP side takes it. The dB and the mute share ONE `AtomicU64` (f32 bits + bit
  32), so `/api/levels` never shows a new mute with an old dB; `linear` is its own `AtomicU32`.
- **`HubState.levels` is the `Levels` itself** (an `Arc` clone on the hub-mix thread, no
  allocation); its `Serialize` reads the values when `/api/state` or `/ws` serializes, off the
  real-time thread. Never put `levels.states()` back into `HubState::snapshot`.
- **The deployed sliders (intercom#38):** strih-snv `foh` FOH = `foh_dante:1`, `:2` (4 points,
  from `program`) · `lv1` LV1 (4, from `program`) · `click` Click = `foh_dante:3` (4) · `cam1`..`cam7`
  Kamera 1..7 (4 each, from `kamery`) · `telefony` Telefóny (4) · `master` Master (44); strih-pp
  `program` Program = `dsg_dante` (2) · `cam1`..`cam7` (2 each, from `kamery`) · `telefony` (2) ·
  `master` (18). Every cutters point on both boxes is trimmed by exactly one SOURCE slider, never
  the master (pinned in `levels_34.rs`; the per-source shape and the migration in `levels_38.rs`).
  strih-snv's come from the converter's `LEVELS` (its `_check_level` fails the generation on a
  source with no point into dst, a master with sources, a master with no point); strih-pp's are
  hand-edited at the end of its TOML. A new cutters point needs its slider in the same change.

## The control listener (loopback only)

- `[hub] control_bind` (default `127.0.0.1:8791`); a non-loopback address fails the load (the
  controls have no login). The box's nginx and the public phone front proxy only `:8790`, so no
  phone and no internet client reaches it. Started only when the matrix has a `[[level]]`
  (`levels_http::start_level_controls`, called from `main.rs`), through the same `http_serve::serve`
  + Dante guard + `check_bind`. A failed bind (e.g. a taken port) is an error line and the hub runs
  on without sliders, never stopping the audio; a `control_bind` the Dante guard refuses still stops
  it at start (review findings, pinned in `levels_34.rs`). The stream boxes have no sliders and no
  :8791.
- **DNS rebinding guard:** loopback stops other machines, not a web page in the notebook's own
  browser that re-binds its name to 127.0.0.1. A middleware (`loopback_host_only`) answers 403 unless
  the `Host` (or the HTTP/2 authority) is `127.0.0.1` / `localhost` / `[::1]` (`is_loopback_host`) and
  an `Origin`, when sent, is `http://` one of those. A PUT with JSON also needs a CORS preflight,
  which the routes never answer.
- **The cutter opens the page from the tray icon** (`intercom-tray`, below: "Otvoriť hlasitosti");
  it is `http://127.0.0.1:8791/` on the strih notebook (their cans hang on its card). The page is
  a mixer (intercom#38): one narrow (62 px) channel strip per slider side by side, a vertical fader
  (0.5 dB steps, arrows work) + mute + its dB, the Master strip last and set apart (accent border,
  `data-master="true"`), the hub version (`v<semver>`), and a 1 Hz meter of the trimmed output (the
  cutters' peak dBFS after the sliders). strih-snv's 12 strips + the meter fit a 900-wide window
  (the tray's default, intercom#39) and a 1366-wide screen; a narrower window scrolls the strip row inside its own box, never the page. There is no
  per-source meter (the hub measures participants, not slider sources). It never writes to the
  console: failures are the status line.
- Routes: `GET /`, `/levels.js`, `/levels.css`, `/favicon.svg`, `/api/version`, `GET /api/levels`
  (`{version, levels: [{name, label, dst, sources, db, muted, min_db, max_db, points, master}], outputs:
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
  (warned); a slider the file does not name takes its `from` slider's value (an info line each:
  the #34 `program` -> `foh` + `lv1`, `kamery` -> `cam1..7`; its own stored value always wins);
  names that are neither a slider nor any `from` are ignored with ONE warn line listing them; a
  parse error, an unreadable file are warned and ignored; a missing file keeps 0 dB. Never fatal.
  `--check-config` never touches it. The next change rewrites the file with the current names
  only, carrying the migrated values, so nothing saved is lost.
  A DOWNGRADE to a #34 hub after that rewrite finds no `program` / `kamery` in the file, so those
  two sliders start at 0 dB there (review round 1, accepted: no downgrade is planned).
- Written on every change by `write_atomic` (`.<name>.tmp` beside it, fsync, rename, fsync the
  directory) in
  `spawn_blocking` under the writer lock. A failed write is a warn naming the path; the value still
  holds until the next restart and the answer says `saved: false` (the page shows it). The dB is
  stored as the f32's shortest decimal (`-7.3`, not toml's f64 widening).

## Tests

- `hub/tests/levels_38.rs`: the master (resolution, refusals, 0 dB identity, trim on top of the
  source trims, boost clamp, mute = silence on that output only, the talkback legs untouched), one
  camera's fader moving only that camera, the `from` migration (group -> sources, own value wins,
  clamp, unknown, the rewritten file loses nothing, a real #34 file on both deployed TOMLs), the
  additive `master` API field, both deployed TOMLs' per-source shape.
- `hub/tests/levels_34.rs`: the resolution and every refusal, both deployed TOMLs' sliders (and the
  stream boxes' none), the engine (0 dB bit identity, an exact trim, a boost on top of the TOML gain,
  mute only its points), the state file (round trip, clamp, unknown, corrupt, missing, the atomic
  write), `/api/state` facets, and the control routes on a real `http_serve` listener (reqwest).
- `tests/python/test_vbmatrix_to_intercom_toml_1345.py`: the emitted sliders cover every cutters
  point once; a source with no point fails the generation; the per-source list, `from` and the
  master are emitted; a master with sources (or no point) fails the generation.
- **Playwright on the REAL hub** (`tests/e2e/levels.spec.js`, project `levels`): `start-levels-hub.js`
  renders `levels.toml` (strih-snv's 12 strips in small: prog = FOH L/R + click, lv1, cam1..7 mono,
  phones; loopback ports from `levels-env.js`, `INTERKOM_E2E_PORT` + 1..3, the state file in a
  fresh `$TMPDIR/intercom-levels-e2e-<port>`) and runs `target/release/intercom-hub`
  (`INTERCOM_HUB_BIN` overrides). The spec drags a fader, steps it with the keyboard, toggles a
  mute, reads `/api/levels` and the state file back, checks the version shown and a clean console;
  moves Kamera 3 and checks every other slider unchanged, steps and mutes the master; checks the
  1366x768 layout (one row, every fader and mute on screen, no page scroll) and a 700-wide window
  (the strip row scrolls, the page does not); feeds the hub's `cam3` leg a real VBAN tone
  (`vban-tone.js`, the header as `VbanHeader::encode` writes it, paced on the wall clock) and
  watches the meter stay up with every other source muted, go to `ticho` when Kamera 3 or the
  master is muted, and rise again. One hub serves all tests, so a `beforeEach` resets every slider
  to 0 dB (without it a `--repeat-each` run fails on the previous test's mute).
  CI: `web-e2e` `needs: rust` and downloads its `intercom-hub-linux-amd64` artifact (chmod 0755: an
  artifact drops the mode).
- Local: `cargo build --release -p intercom-hub`, then `cd tests/e2e && INTERKOM_E2E_PORT=18793 npx
  playwright test --project=levels` (from a script file). Playwright starts EVERY `webServer` for
  any `--project`, so even a `--project=phone` run needs the release binary (or
  `INTERCOM_HUB_BIN`); a missing one fails the run loudly, never skips.
- **The fader is a horizontal range turned -90deg inside an upright 40x260 `.strip-fader` box**
  (max at the top). NEVER `writing-mode: vertical-lr` on the range: the tray's window is WebKitGTK
  (2.52.6 on the boxes), which ignores it and draws a tiny horizontal slider (live 20.11.2026);
  Playwright's WebKit and Chromium both honour it, so no browser test saw it. Not `appearance:
  slider-vertical` either (deprecated in Chromium: a console warning). Pinned statically in
  `tests/python/test_levels_fader_webkitgtk_39.py`; the click geometry (top = max, bottom = min,
  middle = mid) in `levels.spec.js` "every engine", run in Chromium and the `levels-webkit` project.
  Check a page change in REAL WebKitGTK: the tray `--open --url http://127.0.0.1:<port>/` under
  `xvfb-run` + `import -window root` (the smoke recipe in the tray section).
- Gotcha: a Playwright drag from the bottom edge of a fader first lands mid-range, so assert the end
  value, not the path.

## The tray icon `intercom-tray` (owner 19.11.2026: "ikonka na otvorenie dole v bare")

- **What:** `tray/` (its own Cargo workspace, see `.claude/rules/release-ci.md`), dstack's dtray
  pattern: `tray-icon` =0.21.2 without default features + `gtk` =0.18.2 / `glib` =0.18.5, plus the
  mix window's `wry` / `webkit2gtk` (below). The strih
  boxes run Xorg + openbox with tint2's systray, where dtray's icon already shows (camera-box
  19.11.2026). AppIndicator gives no click on the icon itself, so the page opens from the menu.
- **Menu:** the status line (`Hub v<x> beží` / `Hub nedostupný (<why>)`, read from the control
  listener's `/api/version` every 2 s on its own thread, with a loopback `Host` for the rebinding
  guard), **Otvoriť hlasitosti**, **Kopírovať URL**, **Ukončiť ikonu (hub beží ďalej)**. The icon is
  drawn in code: headphones, green while the hub answers, grey otherwise.
- **Open = the tray's own mix window (intercom#39, owner 20.11.2026: "ten firefox v okne so
  zalozkami je poriadny hnus").** `src/linux_mix.rs`: a plain `gtk::Window` titled `Interkom — mix`
  (the headphone icon, WM_CLASS `intercom-tray`, a normal window = its own taskbar entry) holding
  wry's WebKitGTK 4.1 WebView (`wry =0.57.1` without default features + `os-webview`,
  `webkit2gtk =2.0.2`, both on gtk 0.18) built with `build_gtk` into the tray's own GTK loop: no
  tao, no second loop or thread (design comment 7038134606). No tabs, no address bar.
  - A second Open raises the same window (`present`, deiconify; a hidden one is moved back to its
    last place first). Closing it (`delete-event`) only hides it; the tray keeps running.
  - Size + position: `$XDG_STATE_HOME/intercom-tray/window.json` (else
    `~/.local/state/intercom-tray/`; a relative `XDG_STATE_HOME` is ignored), written 1 s after a
    move/resize, on close and on Exit (temp file + rename); restored through `mix_window::place`:
    the monitor its centre is on, else centred on the primary; size within 360x280 .. the
    monitor's work area; pulled fully onto it. A broken file = the default 900x700, centred.
  - GOTCHA: hiding a GTK window emits a configure-event whose `position()` is GTK's stale FIRST
    position; the handler ignores a configure while `!is_visible()`, or the close overwrote the
    saved place (caught by the xvfb smoke, 20.11.2026).
  - The page down (WebKit `load-failed`, except `NetworkError::Cancelled`, which every replaced
    load raises) -> `mix_window::error_page` (Slovak "Hub sa nedá otvoriť … Skúšam znova o 3 s…",
    the URL and WebKit's reason, escaped) and a reload of the URL every `RETRY` (3 s), gated by
    `mix_window::RetryGate` (review round 1: unbounded, it logged a warn every 3 s also while
    hidden, ~29 000 lines a day): ONE warn per outage (the retries at debug, an info when the page
    loads again), no reload while the window is hidden, the parked retry runs when Open raises it.
    Tests: `tray/tests/tray_39_retry.rs`.
  - Fallback: the WebView fails to build -> one error line and the old browser launch
    (`lib::choose_launch`: Firefox `--new-window`, the snap `/snap/bin/firefox` even off `PATH`, then
    Chrome / Chromium `--app=`, then `xdg-open`) from then on (`mix_window::open_action`). Limit: the
    binary LINKS `libwebkit2gtk-4.1.so.0` (+ soup 3, JSC 4.1) like GTK, so a box without
    `libwebkit2gtk-4.1-0` cannot start the tray at all; strih-snv has 2.52.6 (read 20.11.2026).
  - `--open` shows the window at start too (the smoke run uses it).
  - Build: CI's tray job installs `libwebkit2gtk-4.1-dev`; `release.yml` installs the runtime
    `libwebkit2gtk-4.1-0` before its `intercom-tray --version` check (the runner image lacks it).
  - Local smoke (dev1 has xvfb-run, openbox, xdotool, ImageMagick `import`): a script FILE run by
    `xvfb-run -a -s '-screen 0 1920x1080x24'` that starts `openbox`, the tray with `--open` and
    `XDG_RUNTIME_DIR` / `XDG_STATE_HOME` in a scratch dir, a `python3 -m http.server` page started
    late (the retry), `xdotool windowmove/windowsize`, close with `windowactivate` + `key alt+F4`
    (openbox sends WM_DELETE; `xdotool windowclose` DESTROYS the window instead), then a restart.
    WebKit's `libEGL warning: ... DRI3` lines under Xvfb are noise. No systray host under Xvfb, so
    the menu's second Open (the raise) is checked live only.
- **URL:** `--url` (default `http://127.0.0.1:8791/`), refused unless `http://<loopback>:<port>`.
  One tray per session (`$XDG_RUNTIME_DIR/intercom-tray.lock`).
- **Ships** in the hub tarball; camera-box installs and starts it as a session app like
  `dtray.service` (its `strih-session-apps.sh`).
- **Tests:** `tray/tests/tray_34.rs` (the URL, the browser order on the boxes' real layout, the
  status read against a local listener, the icon, the lock path, the version equal to the root's);
  `tray/tests/tray_39.rs` (the window title, the state path / text / file round trip and failures,
  the placement on monitors, raise vs create vs browser, the Slovak error page and its escaping).

