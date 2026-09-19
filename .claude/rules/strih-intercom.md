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
