# Handover: intercom (2026-11-01)

## Status

Split from `zbynekdrlik/camera-box` on 2026-11-01 with history (ticket airuleset#1280). Inventory: camera-box#1487 section B. The split covers `intercom/` at the repository root, the hub service, Janus and routing scripts, four `strih-intercom*` rules and the hub and PWA tests.

The tree does not build yet. The root `Cargo.toml` workspace stayed in camera-box, and the crates use `version.workspace = true`. First work: add a workspace, CI and a release that publishes a hub binary with its TOML files.

camera-box needs two things from you before it can drop `intercom/`: a version tag for `intercom-vban`, and a published hub release artifact with its TOML files. Until both exist, its copy stays frozen. Keep the wire format and the `/api/state` fields stable.

## Machines

- dev1: this project's account, with no sudo.
- The strih boxes run the hub. camera-box installs your pinned release through `setup-strih.sh` steps 13, 14 and 16i. You do not deploy to them yourself.

## What else acts on the machines

- camera-box provisions the boxes, fetches the hub artifact and runs `verify-strih.sh` items 9, 16, 17 and 45.
- The cambox client `src/intercom.rs` and `src/intercom_target.rs` stay in camera-box.
- dstack owns all Dante code. Your link to it covers the hub's Dante device (camera-box#1485, moved here by the owner's decision on dstack#20).

## How to run a test

`cargo test` in the crates after the workspace exists. Hub provisioning test: `tests/intercom_hub_provisioning.rs`. Browser tests: `tests/e2e`, Playwright 1.49.1, chromium. Python tests: `tests/python`.

## Site facts

Private, never in this repository: `~/.config/intercom/`. The TOML files for strih and stream boxes are in this repository and were public in camera-box. Never commit a room secret or a TURN secret.

## Runners

None registered. camera-box CI built this code before. Ask airuleset if a hub test needs real boxes.

## Backlog

Owner-requested:

- Handle the intercom as a separate process and project (2026-11-01).
- The hub owns its Dante device (camera-box#1485).

Must decide with camera-box (camera-box#1487): `src/drm_output_audio_match.rs`, compiled by the hub from the appliance tree, and the installer in `scripts/lib/strih-intercom-hub-unit.sh`.

Proposed by a session, not yet asked for by the owner: nothing.

## Knowledge left in other projects

- camera-box: `audio-clock-map.md`, the session's memory, camera-box#1487 and #1485.
- dstack: dstack#20 and `docs/design/2026-10-30-shared-dante-stack.md`.
