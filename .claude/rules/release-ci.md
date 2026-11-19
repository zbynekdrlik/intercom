---
paths:
  - ".github/workflows/**"
  - "Cargo.toml"
  - "Cargo.lock"
  - "hub/tests/hub_unit_provisioning_1345.rs"
  - "tests/e2e/**"
---

# CI and the release camera-box pins (intercom#1)

## The contract

- `[workspace.package] version` is the one version. The first dev commit after every merge bumps
  it; CI's `version-check` fails on dev unless it is plain `X.Y.Z` strictly above main's.
- A main run: all gates -> `release-tag` pushes `vX.Y.Z` (refuses a tag already on another commit)
  -> `release.yml` (workflow_call only) publishes `intercom-hub-linux-amd64.tar.gz` + `.sha256`
  with the binary that run built. Flat members, exactly: `intercom-hub` (0755),
  `intercom-hub.service`, the four `intercom.*.toml`, `intercom-tray` (0755, intercom#34). A new
  TOML must be added to the shipped list in `release.yml` (the release fails when the repo's TOMLs
  differ from it).
- Published assets never change: both assets present = left alone, one = fail, none = publish
  (`overwrite_files: false` as a backstop). Only gh's literal `release not found` means "no
  release"; any other gh error fails the step.
- CI pins rustc (`toolchain:` on `dtolnay/rust-toolchain`, SHA-pinned) and the coverage gate
  (`--fail-under-lines 67`, measured 69.48 % on 2026-11-01; ratchet up only).
- **`tray/` is its own Cargo workspace** (root `exclude = ["tray"]`, its own `Cargo.lock`): its GTK
  tree (glib-macros -> proc-macro-crate) pins `toml_datetime =0.6.3`, which in one lockfile pulled
  the hub's `toml` from 0.8.23 down to 0.8.2. Keep `tray/Cargo.toml`'s `version` equal to the root
  one (its test `the_tray_version_equals_the_workspace_version` and the release both check it); the
  CI `tray` job (GTK + AppIndicator -dev packages, fmt / clippy / test / 80 % library coverage,
  release build) is a `release-tag` need.

## Gotchas

- A called workflow sees the CALLER's `github` context (`event_name` push, `ref` main). Tell the
  paths apart with `github.ref_type`, never `github.event_name == 'workflow_call'`.
- No `rust-toolchain.toml`: dev1's shared toolchain in `/opt/rust` is read-only, rustup cannot
  install a named channel there, and every local cargo call fails.
- `gh run view --log` / `--job --log` fail on dev1 (`/tmp/gh-cli-cache` permission denied, another
  account owns it). Read a job log with `gh api repos/zbynekdrlik/intercom/actions/jobs/<id>/logs`.
- `grep -q` at the end of a pipe under `pipefail` can SIGPIPE the writer (rc 141): grep a file or a
  here-string instead.
- Check a release by downloading it: `sha256sum -c`, `tar -tvzf`, `--version`, `--check-config` on
  every shipped TOML, and a probe crate with the camera-box pin line
  (`intercom-vban = { git = "...", tag = "vX.Y.Z" }`).

## Local test notes (dev1)

- **CI's E2E job runs in `mcr.microsoft.com/playwright:v<version>-noble`** (18.11.2026): the image
  tag must equal `@playwright/test` in `tests/e2e/package-lock.json`, so bump both together. The old
  `playwright install --with-deps` pulled ~120 MB from the Ubuntu mirror every run and timed the job
  out twice when the mirror served ~110 kB/s.
  A container job's `run:` steps default to `sh -e` (dash: `set -o pipefail` fails, exit 2), so
  the job sets `defaults.run.shell: bash` (`bash -eo pipefail`).
- E2E: `cd tests/e2e && npm ci && INTERKOM_E2E_PORT=18792 npx playwright test`. Port 8792 is held
  by another account. Playwright 1.49.1's WebKit (`webkit-2104`) is not in `/opt/ms-playwright`, so
  the three iPhone specs run on CI only (`playwright install` into this account is hook-blocked).
- Since intercom#34 the E2E run also starts the REAL hub (`start-levels-hub.js`, the `levels`
  project): build `cargo build --release -p intercom-hub` first, or set `INTERCOM_HUB_BIN`, also for
  a `--project=phone` run (Playwright starts every webServer). On CI the `web-e2e` job `needs: rust`
  and downloads its `intercom-hub-linux-amd64` artifact (chmod 0755). It uses the base port + 1..3.
