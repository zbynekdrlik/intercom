---
paths:
  - "intercom/**"
  - "scripts/vbmatrix_to_intercom_toml.py"
  - "systemd/intercom-hub.service"
---

# strih-lx intercom hub (issue 1345)

The strih intercom is migrating off the Windows **VB-Audio Matrix** (a GUI-only static N-1 grid) +
**VDO.Ninja** onto a Rust mix-minus hub on the strih-lx Linux notebook (`intercom/hub`), with Janus
as the only WebRTC code and our own phone PWA. This rule covers the M1 core (the VBAN leg + engine +
converter). Design: issue 1345 comment "Design (main, 19.9.2026)"; the VB-Matrix read-out: issue
1344 comment "VB-Matrix setup READ".

## The workspace crates

- **`intercom/vban` (`intercom-vban`)** — the shared VBAN codec, MOVED out of `src/vban.rs` (which
  is now `pub use intercom_vban::*;`). It is the ONE source of truth for the wire format: the
  appliance (`src/intercom.rs`) and the hub both use it, so the 7 camboxes need NO change. Pure
  (only `anyhow`) — it is a path dependency of the appliance, so the root build compiles it, but its
  tests run only in the `intercom-hub` CI job (`cargo test -p intercom-vban`), never the root
  `cargo test`.
- **`intercom/hub` (`intercom-hub`, bin `intercom-hub`)** — the tokio + axum daemon (the bkshading
  service skeleton): `matrix` (declarative `Matrix::from_toml`), `engine` (the N-1 mixer), `vban_io`
  (recv/jitter/demux/send), `state` + `http` (`/api/state` + `/ws` + `/api/version`). Its own heavier
  deps (tokio/axum/toml) live only in its manifest, NOT the appliance tree.

## The cambox wire contract (keep byte-identical — never break it)

`src/intercom.rs` is the reference: a cambox **SENDS** its mic as VBAN stream `camN` (stereo PCM16,
48 kHz, our header, port 6980) to its hub at `target:6980`, and **RECEIVES** only packets whose
stream name == its own `camN` on `0.0.0.0:6980` (any other name ignored), playing them stereo. The
hub must speak exactly this: it demuxes incoming `camN` into that participant's jitter buffer
(unknown names dropped), and sends each cambox's mixed **stereo PCM16** output back as stream `camN`
to `camN.lan:6980`. Because the codec is the SAME crate, the bytes are identical by construction.

## The N-1 (mix-minus) invariant — enforced structurally, not by config

For every output channel, the engine sums the routed input channels with gain, EXCLUDING a
participant's own source. This is enforced at TWO layers: the converter never emits a `src == dst`
point (the VB-Matrix data has none — 0 self-routes in 216 points), AND `Matrix::from_toml` REFUSES
any `src == dst` point at load. So a cambox can never hear itself no matter what the TOML says. When
adding/regenerating the matrix, never hand-add a self-route — it will fail to load.

## The converter + the fixture-parity rule

`scripts/vbmatrix_to_intercom_toml.py` parses the VB-Matrix XML into `intercom/intercom.strih-lx.toml`
(the checked-in live matrix). Slot roles are DERIVED from the XML's own attributes, not hard-coded:
an active `VBANStreamOut` (`status=1`) makes a slot a `cambox` (cam1..7), else a `program_ref`
(fohabl/lv1/mbc = the stream name minus `-strih`); `AMDevice` type 256/4/1 → cutters/speakers/line34;
online VAIO1 → phones, VASIO8 → program_monitor. Per-participant `in_channels`/`out_channels` = the
max channel routed (this preserves cam2's 8-ch input + the cam3 ch-2 asymmetry). Only the VBAN
adapter is live in M1; every non-VBAN participant is declared `adapter = "none"`.

**PARITY IS PINNED:** `tests/python/test_vbmatrix_to_intercom_toml_1345.py` regenerates from the
fixture (`intercom/tests/fixtures/vbmatrix-coconut-today.xml`) and asserts it reproduces the
checked-in TOML BYTE-FOR-BYTE. So whenever the VB-Matrix XML changes (re-read from the Windows
strih), regenerate + re-commit BOTH the fixture and the TOML together:

```
python3 scripts/vbmatrix_to_intercom_toml.py intercom/tests/fixtures/vbmatrix-coconut-today.xml \
  > intercom/intercom.strih-lx.toml
python3 -m pytest -q tests/python/test_vbmatrix_to_intercom_toml_1345.py
```

Never hand-edit `intercom.strih-lx.toml` — the parity test will go red; edit the converter (or the
fixture) instead.

## NEVER send VBAN to a real cambox while the Windows strih is its live hub

Launching `intercom-hub` against the live rig SENDS VBAN back to the real camboxes (`camN.lan:6980`)
— which would collide with the Windows strih that is still their production hub until the M4
cut-over. So:

- The systemd unit (`systemd/intercom-hub.service`) is installed **ENABLE-ONLY** by
  `setup-strih.sh` (step 13) — enabled, NEVER started/restarted. `verify-strih.sh` reports it
  report-only (an installed+enabled but inactive unit is correct while parallel).
- Never run `intercom-hub` from a dev lane. M1b (the supervisor) does the first live test by
  repointing ONLY dev cambox cam1 at strih-lx via a `/run` env override (`CAMERA_BOX_INTERCOM_TARGET`,
  per the design) — cam2-7 stay on the Windows strih until M4.

## M1b — repointing ONE cambox at the Linux hub (`CAMERA_BOX_INTERCOM_TARGET`)

The cambox root filesystem is READ-ONLY, so `/etc/camera-box/config.toml` cannot be edited to change
the intercom target host. camera-box therefore honours an env override:

- **`CAMERA_BOX_INTERCOM_TARGET=<host>`** rewrites the `target_host` of the resolved intercom config.
  Precedence is **env > `--intercom-target` CLI flag > `config.toml`** — the env is applied AFTER the
  CLI-vs-config resolution in `main.rs`, and logs an `info!` note naming the env var + the new and old
  hosts when it overrides. The pure resolver is `src/intercom_target.rs::resolve_intercom_target`
  (a `None`/empty/whitespace value is a no-op); an empty override is treated as unset (falls through
  to the CLI/config target).

- **The `/run` drop-in** carries that env into the systemd unit without touching the ro root:
  `scripts/lib/intercom-target-dropin.sh` builds the remote bash.
  `INTERCOM_TARGET_DROPIN` defaults to `/run/systemd/system/camera-box.service.d/zz-intercom-target.conf`
  — under `/run` (tmpfs), so a **reboot auto-reverts** to the deployed Windows-strih target.
  - **Set** (repoint cam1 at strih-lx): `intercom_target_dropin_set_cmds strih-lx.lan` prints remote
    text that writes `[Service]\nEnvironment=CAMERA_BOX_INTERCOM_TARGET=strih-lx.lan`, `daemon-reload`s,
    `restart`s camera-box, then reads `systemctl show -p Environment --value camera-box` back and greps
    the var (fails loud if the override did not take). The host is validated (non-empty, no
    whitespace/quote) before any text is emitted.
  - **Clear** (revert to the deployed target): `intercom_target_dropin_clear_cmds` removes the drop-in,
    `daemon-reload`s and `restart`s camera-box.
  - Every emitted statement is `;`-terminated (the `$(...)` trailing-newline-strip gotcha).

This lib is a **supervisor tool** — it is deliberately NOT wired into `recording-e2e.sh` / `rig-mode.sh`.
The supervisor uses it by hand at the M1b live step (repoint cam1, run a cam1 ↔ strih-lx loopback with a
real headset, then clear).

**NEVER repoint cam2-7** at strih-lx before the M4 cut-over — those camboxes are on the LIVE Windows
strih, and a second hub sending VBAN back to them is double talkback.

### The M1b live-loopback recipe (ran 19.9.2026 — what actually worked)

- **Never start the enabled `intercom-hub.service` with the full checked-in matrix while the Windows
  strih is the live hub** — the hub SENDS `camN` to every `vban` participant with `out_channels > 0`,
  i.e. cam1–7. Run the test hub as a TRANSIENT unit on a cam1-only matrix instead: filter
  `intercom/intercom.strih-lx.toml` down to participants `{cam1, cutters}` + the points between them
  (a 20-line `tomllib` script), install it as `/etc/intercom-hub/intercom.m1b-cam1.toml`, then
  `systemd-run --unit intercom-hub-m1b --property=DynamicUser=yes --property=Restart=no
  /usr/local/bin/intercom-hub --config /etc/intercom-hub/intercom.m1b-cam1.toml`; `systemctl stop
  intercom-hub-m1b` ends it. The production unit stays `enabled` + `inactive` until M4.
- **Even the cam1-only hub is a SECOND sender of stream `cam1` into cam1:6980** (the Windows VB-Matrix
  keeps sending its own `cam1` mix) — cam1's receiver has no source filter, so its headset audio is
  interleaved/garbled for the duration. Keep the window short: start the hub right before the
  drop-in `set`, stop it right after the `clear`.
- **cam1's intercom mic starts MUTED** (`src/intercom.rs`: `muted = AtomicBool::new(true)`, toggled by
  the physical power button via evdev). For a hands-off test, FIRST verify `loginctl show-logind |
  grep HandlePowerKey=ignore` on the box (provisioning sets it; without it a KEY_POWER would power the
  box off), then inject one synthetic press on the ACPI node: a 24-byte `input_event` `(EV_KEY=1,
  KEY_POWER=116, 1)` + `EV_SYN`, then `(…, 0)` + `EV_SYN`, written to `/dev/input/event1` with a
  python3 `struct.pack("llHHi", …)` one-off (python3 exists on the boxes; no evemu). The journal
  confirms `🎤 Microphone UNMUTED (via /dev/input/event1)`. The `clear` step's camera-box restart
  re-mutes by construction — no second injection needed.
- **What "live" looks like:** hub `/api/state` (`:8790`) for cam1 → `rx_packets` climbing at cam1's
  `send` rate (375 pkt/s = 128 mono frames/pkt at 48 k), `last_rx_age_ms` ≈ 1, `level_dbfs` ≈ −50
  (room/headset floor, −120 = silence), `tx_packets` at 187.5/s (256-frame blocks); cam1's
  `Intercom: recv N pkt/s` rises by the hub's tx rate over the Windows-only ~466 pkt/s. A hub `tx`
  far below 187.5/s at 0 % CPU + an `overruns` storm = the block loop is BLOCKING on something
  (19.9.: a per-block DNS lookup — destinations are now resolved once, off the hot path).
- The cambox headset ADC runs ~+540 ppm against the hub's timer (cam1 `capture 48026 samp/s`) —
  without a per-stream rate servo that is one 128-frame overrun every ~5 s (an M2 item, not M1b).
- **Put cam1 BACK on main's release build the moment the test ends** (`CAMERA_SET=cam1
  scripts/deploy-fleet.sh` with no `--run` = main's latest artifact). The M1b test needs the DEV
  build on cam1 (the env override is not in the release yet), but a single box left on a dev build
  makes the release PR's Full-path E2E REFUSE at the camera-box version-parity gate (`!! GATE FAILED:
  1 active box(es) are NOT on the pinned main camera-box`, 19.9.2026 PR 1348) — the auto-align only
  heals a UNIFORMLY-stale fleet, never a MIXED one. Restoring cam1 is part of the test's cleanup,
  same as the drop-in `clear` and the hub `stop`.

## Milestone map

- **M1 (this lane):** the vban crate extraction + hub engine + VBAN adapter + converter + TOML +
  `/api/state` + CI + unit + the enable-only systemd unit. Zero production impact.
- **M1b (supervisor):** the cam1 env override on the appliance + the live cam1 ↔ strih-lx loopback.
- **M2:** the MiniFuse/PipeWire adapter (`pipewire_io`) — cutters' cans, speakers, line-3/4, the
  −8/−10 dB program refs into the cutters' cans only (= issue 1344, needs the MiniFuse plugged into
  strih-lx).
- **M3:** Janus (apt) audiobridge/streaming + the phone PWA + the Interkom video (`STRIH-LX (interkom)`
  NDI republish, issue 1347).
- **M4:** the cut-over — repoint the 7 camboxes + fohabl/lv1/mbc VBAN targets to strih-lx, retire
  VB-Matrix + the OBS browser source.

## Gotchas folded from the M1 review

- **Jitter underruns count ONLY for a previously-LIVE stream that ran dry** (`vban_io.rs::pop_block`
  guards on `last_rx.is_some()`). A buffer that has NEVER received a packet — an `adapter="none"`
  participant, an unrouted program ref, or a vban leg before startup — pops silence with NO
  underrun, so the watchdog status line isn't swamped by ~1 phantom underrun/participant/block.
- **`mbc` is a declared-but-unrouted `program_ref`** (`in_channels=0`/`out_channels=0`): the captured
  VB-Matrix routes fohabl + lv1 as program refs but not mbc, and the parity test pins that fidelity.
  If mbc should carry program audio, re-capture the VB-Matrix XML when it is active, then regenerate.
- **The deployed matrix is load-tested through the REAL `Matrix::from_toml`** (not just tomllib) by
  `intercom/hub/tests/deployed_matrix.rs` (`include_str!("../../intercom.strih-lx.toml")`) — so a
  future converter/XML change that produces a byte-parity-passing but daemon-REJECTED TOML fails at
  CI, not at rig startup. Keep that test's expected shape (15 participants / 216 points) in sync.

## Tier-0 (issue 557) — what verifies locally vs at CI

- **Locally:** `cargo fmt --all --check` (parses all the new Rust), the converter pytest, `python3 -c
  "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"`, `bash -n`/`shellcheck -S warning`
  on the scripts, the doc-lazy grep on touched `.rs`. A worktree lane can ALSO cross-check the
  generated TOML against every `Matrix::from_toml` invariant with a small `tomllib` script (known
  roles/adapters, channel bounds, no self-route).
- **At CI (first real compile):** the `intercom-hub` job type-checks + runs the Rust unit tests
  (matrix/engine/vban_io/state/http, incl. the random-port VBAN loopback) + the moved codec tests;
  the appliance `test` job runs `tests/intercom_hub_provisioning.rs` (the static anchors). Expect a
  Rust TYPE mistake to surface at CI, not locally.
- **What the first compile actually caught (19.9.2026, three CI rounds — hand-audit these BEFORE the
  next new crate lands):** (1) `chunks_exact(N)` with a constant N → the Rust 1.98 deny lint
  `chunks_exact_to_as_chunks` (use `as_chunks::<N>().0.iter()` + `from_le_bytes(*c)`; bkshading.md
  lists the same trap); (2) an axum handler returning `Json<Arc<T>>` fails the `Handler` bound
  (E0277) unless serde's **`rc`** feature is on — serde has no `Serialize for Arc<T>` otherwise;
  (3) clippy `-D warnings` on the hub: a struct field only ever WRITTEN (`dead_code`), `if n == 0 {0}
  else {a / n}` (manual checked division → `a.checked_div(n).unwrap_or(0)`), and a fn with 8 params
  incl. `self` (`too_many_arguments`, cap 7 → group them in a borrowed `OutBlock<'_>` struct, not an
  `#[allow]`). A lib lint stops clippy before the bin/tests are linted, so each round can reveal a
  NEW layer — audit main.rs + tests with the same list rather than waiting for the next round.

## M3b — the phone PWA (served by the hub) + the LAN-HTTPS interkom front (issue 1345)

M3b is the CLIENT half of M3: the installable phone web app the hub serves at `/`, plus the second
dev1 nginx TLS site that fronts it so a phone gets a full PWA install + a secure context for
`getUserMedia`. It does NOT touch the M3a audio edge (Janus adapter / µ-law / config / converter /
setup-strih) — those are the sibling lane's files. Design: issue 1345 comment "Design (main,
19.9.2026)", Prístup 1.

### The embedded phone PWA — `intercom/web/` served by `intercom/hub/src/http.rs`

- **Embedded, self-contained** (the bkshading model): the hub serves every asset via
  `include_str!`/`include_bytes!` from `../../web/…` (relative to `intercom/hub/src/` →
  `intercom/web/`), so the binary is self-contained on strih-lx with no runtime file/CDN
  dependency. The M1 plain-text `/` index is REPLACED by the PWA index; `/api/version`,
  `/api/state`, `/ws` stay byte-identical. Each asset has a pure `*_asset()` fn + a
  `*_CONTENT_TYPE` const so the route tests (`intercom/hub/tests/webui_assets_1345.rs`) pin the
  payload + type without a server. `/sw.js` carries `Service-Worker-Allowed: /`. NO new crate.
- **The page contract** (`index.html` + `app.js`, Slovak UI): ONE big primary "Pripojiť" button is
  the single autoplay + `getUserMedia` gesture → it connects janus.js, attaches
  `janus.plugin.audiobridge`, joins room 1000 MUTED (`muted: true` in BOTH the `join` and the
  `configure` payload), and starts the room mix in an `<audio autoplay playsinline>`. The mic
  `<select>` is filled from `enumerateDevices()` (labels appear only after the first grant); the
  big "Mikrofón" toggle DEFAULTS OFF (`data-muted="true"`, `aria-pressed="false"`), turns red when
  ON, and calls `configure {muted:false}` / `replaceTracks` on a device change. The display name is
  typed once and remembered in `localStorage`.
- **Path-relative Janus WS** — `app.js` builds `wss://<location.host>/janus` (the TLS front proxies
  `/janus`), never a hard-coded host; a `?janus=<url>` query overrides it for a dev host. `janus.js`
  is init'd with `debug: false` so its `Janus.log/warn/error` are ALL no-ops → the library never
  writes to the console even on a connection failure. To avoid vendoring webrtc-adapter too, app.js
  passes a minimal `webRTCAdapter` shim (`{ browserDetails: {browser, version} }`, detected from the
  UA) — janus.js 1.x only reads `browserDetails` for per-browser branches; the stream-attach helpers
  live inside janus.js itself.
- **Console-clean discipline** — `app.js` NEVER calls `console.error`/`console.warn`; every hub /
  Janus / picture failure is surfaced as a status CHIP. The hub chip uses an `/api/state` FETCH
  poll (2 s), NOT a `/ws` connection, precisely so an unreachable hub logs no browser WS network
  error; Janus is only contacted on the button click; and the picture `<img src>` is set ONLY once
  the hub is reachable, so a bare 404 to a missing route never fires while the hub is down.
- **The MJPEG picture contract with M3c (issue 1347)** — the `<img id="interkom">` shows
  `/interkom.mjpeg`, which **M3c** serves. Until then (and whenever the hub is up but M3c is not),
  the `<img>` `error` handler shows the "Obraz zatiaľ nie je k dispozícii" placeholder and a 5 s
  timer retries. **Frame-age is deliberately NOT computed in M3b** — a plain `<img>` MJPEG does not
  fire a reliable per-frame `load` event, so M3b only distinguishes streaming vs not-available
  ("Obraz: beží" vs the placeholder); a precise "frozen picture" chip needs M3c's real stream
  semantics and is an M3c refinement.
- **Vendored janus.js** — `intercom/web/janus.js` is the official MIT `janus.js` from
  meetecho/janus-gateway at tag **v1.1.2** (matching the `janus` 1.1.2 package Ubuntu 26.04 ships),
  byte-for-byte, MIT header kept; the tag is recorded in `intercom/web/VENDORED.md`. Keep the
  vendored tag in lock-step with the deployed Janus version.
- **The icons** are generated by `intercom/web/gen-icons.py` (stdlib zlib+struct PNG writer, no
  dependency — the bkshading `gen-icons.py` pattern) as deterministic maskable-safe RGBA PNGs +
  `favicon.svg`; committed as small binaries the hub embeds.

### The LAN-HTTPS interkom front — the SECOND site rendered by `scripts/lib/shading-https.sh`

- The shading-https lib is GENERALISED: `shading_https_site_content <host> <upstream> [extra]
  [fronted] [served]` — an empty `$extra`/`$fronted`/`$served` reproduces the shading site
  BYTE-IDENTICALLY (a drift-guard test pins it), so `scripts/nginx/shading.newlevel.media.conf` is
  untouched. `interkom_https_site_content` = the same renderer with the interkom host
  (`interkom.newlevel.media`), upstream (`http://10.77.9.203:8790` = the hub) + the extra
  `location /janus { proxy_pass http://10.77.9.203:8188; … }` block from
  `interkom_https_janus_location` (HTTP/1.1 Upgrade passthrough + `proxy_read_timeout 3600s` for the
  persistent WS). The `/ws` upgrade for the hub is already carried by `location /`. The rendered
  site is committed at `scripts/nginx/interkom.newlevel.media.conf` and the python test
  (`tests/python/test_shading_https_install_808.py`) pins BOTH rendered files against the lib.
- `scripts/dev1-shading-https-install.sh` gains `--site interkom|shading` (default `shading`,
  existing behaviour unchanged). `--site interkom` adopts the interkom host/upstream/site-name +
  the `/janus` extra block (explicit `--hostname`/`--upstream` still override); the A record still
  points at dev1's LAN IP (dev1 is the nginx front for both sites).
- **Supervisor-only live steps** (NOT a lane): the DNS `interkom.newlevel.media` A record (→ dev1
  LAN IP), `certbot` DNS-01 issuance, and the install on dev1 are run by the supervisor with
  `scripts/dev1-shading-https-install.sh --site interkom --install` / `--check`, exactly like the
  shading site. The lane only renders + tests.

### Tier-0 (issue 557) — what verified M3b locally

- `cargo fmt --all --check` (parses the new http.rs + the integration test); `python3 -m pytest`
  on `test_intercom_webui_1345.py` + `test_shading_https_install_808.py`; `bash -n` +
  `shellcheck -S warning` on the two scripts; `node --check` on `app.js`/`sw.js`/`janus.js`; the
  doc-lazy grep on the touched `.rs`.
- **The real browser check (Playwright, zero builds):** `python3 -m http.server` on
  `intercom/web` + a small stub that returns 200 for `/api/state`+`/api/version`+`/interkom.mjpeg`
  (the happy path) → `browser_console_messages` is EMPTY (0 errors, 0 warnings) and the UI renders
  the version, the muted-default toggle, the "Hub: OK" chip and the picture. Gotcha inherited from
  bkshading: a python static test can't catch a JS/CSS runtime bug — run the browser check. Two
  more gotchas found here: (1) against a bare static server (no hub API) the ONLY console entries
  are the browser's own "Failed to load resource 404" network logs for the absent `/api/*` — there
  are ZERO JS exceptions / `console.error` / `console.warn`, i.e. the graceful-into-chips path; a
  clean console needs the hub API stubbed 200 (the real hub returns 200). (2) The service worker +
  HTTP cache serve a stale `app.js` across a same-origin re-run after an edit — verify a code change
  on a FRESH origin (a different port) or the console/DOM reflects the pre-edit JS.
- **At CI (first compile):** the `intercom-hub` job type-checks + runs the Rust route/asset unit
  tests. Expect a Rust TYPE mistake to surface at CI, not locally (the M1 first-compile list above
  still applies — axum `Json<Arc<T>>` needs serde `rc` (already on), `chunks_exact` deny lint, etc.).
