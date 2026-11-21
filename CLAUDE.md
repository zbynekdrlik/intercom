# intercom

The strih/stream intercom: the mix-minus hub (`hub/`, crate `intercom-hub`), the shared VBAN codec
(`vban/`, crate `intercom-vban`), the phone PWA (`web/`, embedded in the hub binary), the Leptos web UI
(`ui/`, crate `intercom-ui`: the cutters' level page since intercom#48) and the routing TOMLs (`intercom.*.toml`). Split from camera-box on 2026-11-01; start with `docs/HANDOVER.md`.

## Release contract with camera-box

- One workspace version (`Cargo.toml` `[workspace.package]`); every dev->main merge tags `vX.Y.Z`.
  After every merge the FIRST dev commit bumps it (CI's version gate fails otherwise), in three
  files: root `Cargo.toml`, `tray/Cargo.toml` and `tray/Cargo.lock` (the tray is its own workspace).
- That tag pins camera-box's `intercom-vban` git dependency and the hub release
  `intercom-hub-linux-amd64.tar.gz` (+ `.sha256`): binary, `intercom-hub.service`, the four TOMLs.
- camera-box provisions the boxes (`setup-strih`, `verify-strih` and their `scripts/lib` glue).
  Never change the cambox VBAN wire format, hub UDP 6980 vs OBS 6981 or the `/api/state` fields
  without telling camera-box first.

## Playbook router

- CI / release / version / local test notes -> `.claude/rules/release-ci.md`
- hub mix / VBAN / Janus / egress -> `.claude/rules/strih-intercom.md`, `strih-intercom-hub-mix.md`
- HDMI audio leg -> `.claude/rules/strih-intercom-hdmi-leg.md`
- the cutters' level sliders (loopback `:8791`, `[[level]]`, `levels_file`) -> `.claude/rules/strih-intercom-levels.md`
- the Leptos web UI (`ui/`, Trunk -> `ui/dist`, embedded by `hub/build.rs`) -> `.claude/rules/web-ui.md`
- card (ALSA) leg -> `.claude/rules/strih-intercom-alsa-leg.md`
- the hub's Dante device (dstack dlink) -> `.claude/rules/strih-intercom-dlink.md`
- the cameramen's Discord thread (`scripts/discord_thread.py`, mention-only, owner's rule) -> `.claude/rules/discord-thread.md`
