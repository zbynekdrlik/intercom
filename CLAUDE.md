# intercom

The strih/stream intercom: the mix-minus hub (`hub/`, crate `intercom-hub`), the shared VBAN codec
(`vban/`, crate `intercom-vban`), the phone PWA (`web/`, embedded in the hub binary) and the routing
TOMLs (`intercom.*.toml`). Split from camera-box on 2026-11-01; start with `docs/HANDOVER.md`.

## Release contract with camera-box

- One workspace version (`Cargo.toml` `[workspace.package]`); every dev->main merge tags `vX.Y.Z`.
  After every merge the FIRST dev commit bumps it (CI's version gate fails otherwise).
- A published release never changes: a re-run leaves existing assets alone, and there is no
  tag-push trigger. CI pins the compiler (`toolchain:` in `ci.yml`).
- That tag pins camera-box's `intercom-vban` git dependency and the hub release
  `intercom-hub-linux-amd64.tar.gz` (+ `.sha256`): binary, `intercom-hub.service`, the four TOMLs.
- camera-box provisions the boxes (`setup-strih`, `verify-strih` and their `scripts/lib` glue).
  Never change the cambox VBAN wire format, hub UDP 6980 vs OBS 6981 or the `/api/state` fields
  without telling camera-box first.

## Local test notes

- `cargo test --workspace`; `python3 -m pytest -q tests/python`.
- E2E: `cd tests/e2e && npm ci && INTERKOM_E2E_PORT=18792 npx playwright test` (dev1's 8792 is taken
  by another account; WebKit specs need `webkit-2104`, which dev1's `/opt/ms-playwright` lacks, so
  they run on CI).

## Playbook router

- hub mix / VBAN / Janus / egress -> `.claude/rules/strih-intercom.md`, `strih-intercom-hub-mix.md`
- HDMI audio leg -> `.claude/rules/strih-intercom-hdmi-leg.md`
- card (ALSA) leg -> `.claude/rules/strih-intercom-alsa-leg.md`
