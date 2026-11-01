# Handover: intercom (2026-11-01)

## Status

Split from `zbynekdrlik/camera-box` on 2026-11-01 with history (ticket airuleset#1280). Inventory: camera-box#1487 section B. The split covers `intercom/` at the repository root, the hub service, Janus and routing scripts, four `strih-intercom*` rules and the hub and PWA tests.

Since intercom#1 the tree builds as a root workspace (`vban`, `hub`) with CI, and every dev->main merge tags `vX.Y.Z` and publishes `intercom-hub-linux-amd64.tar.gz` + `.sha256` (binary, unit, the four TOMLs). camera-box pins that tag for `intercom-vban` and the hub. The `setup-strih` / `verify-strih` glue (`scripts/lib/strih-intercom-hub-unit.sh`, `strih-interkom-public.sh`, `janus_turn_rest_cfg.py`, the nginx conf) stayed with camera-box: it sources camera-box libs. Keep the wire format and the `/api/state` fields stable.

## Machines

- dev1: this project's account, with no sudo.
- The strih boxes run the hub. camera-box installs your pinned release through `setup-strih.sh` steps 13, 14 and 16i. You do not deploy to them yourself.

## What else acts on the machines

- camera-box provisions the boxes, fetches the hub artifact and runs `verify-strih.sh` items 9, 16, 17 and 45.
- The cambox client `src/intercom.rs` and `src/intercom_target.rs` stay in camera-box.
- dstack owns all Dante code. Your link to it covers the hub's Dante device (camera-box#1485, moved here by the owner's decision on dstack#20).

## How to run a test

`cargo test --workspace`. Hub unit anchors: `hub/tests/hub_unit_provisioning_1345.rs`. Browser tests: `tests/e2e`, Playwright 1.49.1, chromium. Python tests: `tests/python`.

## Site facts

Private, never in this repository: `~/.config/intercom/`. The TOML files for strih and stream boxes are in this repository and were public in camera-box. Never commit a room secret or a TURN secret.

## Runners

None registered. camera-box CI built this code before. Ask airuleset if a hub test needs real boxes.

## Backlog

Owner-requested:

- Handle the intercom as a separate process and project (2026-11-01).
- The hub owns its Dante device (camera-box#1485).

Decided with camera-box (intercom#1): the hub carries its own `hub/src/drm_output_audio_match.rs`; the unit installer stays in camera-box.

Proposed by a session, not yet asked for by the owner: nothing.

## Knowledge left in other projects

- camera-box: `audio-clock-map.md`, the session's memory, camera-box#1487 and #1485.
- dstack: dstack#20 and `docs/design/2026-10-30-shared-dante-stack.md`.
