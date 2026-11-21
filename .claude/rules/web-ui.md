---
paths:
  - "ui/**"
  - "hub/build.rs"
  - "hub/build_ui.rs"
  - "hub/src/ui_assets.rs"
  - "hub/tests/ui_embed_48.rs"
---

# The web UI on the house stack: Leptos CSR -> WASM, Trunk, served by the hub (intercom#48)

The house stack's Web UI row (owner 21.11.2026, `~/.claude/rules/house-stack.md`). Design: #48
comment "Design (21.11.2026)": one Leptos CSR crate for both pages, phase A the cutters' level
mixer (DONE), phase B the phone PWA (`web/app.js`) after intercom#42 phase 5 removes Janus. Until
then `web/app.js` is maintained as is and the phone listener (`:8790`) serves `web/`.

## Shape

- **`ui/` = crate `intercom-ui`, its own workspace** (root `exclude = ["tray", "ui"]`, its own
  `Cargo.lock`, version `0.0.0`). Why not a root member: the coverage gate (`cargo llvm-cov
  --workspace`) would count browser-only component code it never runs, and the Leptos tree would
  enter the hub's lockfile. The page shows the HUB's version, so a release bump never touches ui/.
- Versions: `leptos = 0.8.23` (`csr`), Trunk 0.22.2 (CI; dev1's `/opt/rust/cargo/bin/trunk` is
  0.22.1, fine), `wasm-bindgen = "=0.2.130"` pinned EXACTLY: Trunk's CLI and
  `wasm-bindgen-test-runner` must equal it (CI checks the CLI against `ui/Cargo.lock`).
- **One app, the page from the shell.** `ui/index.html`'s `<body data-page="{{PAGE}}"
  data-version="{{VERSION}}">`; the hub fills both (`ui_assets::ui_shell(page)`), `lib.rs`
  `start()` mounts `App`, which maps `data-page` (`Page::from_attr`: `levels`; anything else shows
  "Neznáma stránka"). A document WITHOUT `data-page` mounts nothing: the `wasm-bindgen-test` page
  runs the `#[wasm_bindgen(start)]` too.
- Modules: `logic.rs` (plain Rust, no DOM: API bodies, JS-exact number formats, meter, status, a
  strip's send state machine), `levels.rs` (components `LevelsPage` / `LevelsBoard` / `StripView`
  / `Meter`, the `Mixer` page state, `reduce` = one strip event on its state + signals),
  `net.rs` (`fetch` via web-sys: status + text, `Err` when nothing answered; never a console line).
- Trunk: `filehash = false` (fixed names `/intercom-ui.js`, `/intercom-ui_bg.wasm`,
  `/levels.css`; the hub serves every UI file `no-store`, so a new hub never meets a cached old
  script), `data-wasm-opt="0"` (no binaryen download; `opt-level = "z"` + LTO: ~1.5 MB wasm),
  `data-integrity="none"` on the rust link and `pattern_preload` = modulepreload of the JS only:
  Trunk's default WASM preload makes WebKit warn in the console (fohmixer's fix).

## Embedding (`hub/build.rs` + `hub/build_ui.rs`)

- `build.rs` embeds every file of `ui/dist` with `include_bytes!` (generated `UI_FILES` in
  `$OUT_DIR/ui_files.rs`, included by `hub/src/ui_assets.rs`). It PANICS (the build fails) when
  `dist` is missing, has no `index.html` with both placeholders, no `.wasm`, no `.js`, or a file of
  an unknown type; dotfiles (`.trunk-lock`) are skipped. `rerun-if-changed` on the dist dir and
  every file: a new `trunk build` re-embeds.
- So EVERY hub build needs `cd ui && trunk build --release` first: locally, in CI's `rust` and
  `coverage` jobs (`.claude/rules/release-ci.md`). `release.yml` refuses a hub binary without the
  shell (`grep -aF 'data-page="{{PAGE}}"'`).
- `ui_assets::ui_routes(page)`: `/` = the shell for that page, every other file at its route.
  `levels_http::control_router` merges it under the loopback Host guard.

## Tests (the three tiers)

- Host logic: `cd ui && cargo test` (`ui/tests/logic_48.rs`).
- Components: `wasm-bindgen-test` in headless Chrome (`ui/tests/levels_dom_48.rs`, `cfg(wasm32)`):
  the board's hooks, the mute toggle, the throttled send, the range's value, the root's page pick.
  Local on dev1: the 0.2.130 `wasm-bindgen-test-runner` from the wasm-bindgen release tarball and a
  chromedriver matching `google-chrome --version` (Chrome for Testing zip) in the scratchpad, then
  `CHROMEDRIVER=<it> CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test
  --target wasm32-unknown-unknown` with the runner on `PATH`. CI uses the runner image's
  `$CHROMEWEBDRIVER`.
- E2E: Playwright `levels` + `levels-webkit` on the real hub (`.claude/rules/strih-intercom-levels.md`).
- Embedding: `hub/tests/ui_embed_48.rs` (the refusals on scratch dirs, the generated source, the
  embedded files = `ui/dist` byte for byte, the shell), `levels_34.rs` (the served routes).

## Gotchas

- **A range's `min` / `max` before its value.** `prop:value` sets the value at once, clamped to the
  range the element has THEN (default 0..100: -3 would read 0). Keep `min`/`max` before
  `prop:value` in the `view!`; pinned by the wasm test (`slider.value() == "-3"`).
- **The page writes numbers the way JavaScript did**: `data-db` = `String(n)` (`-0` -> `0`,
  `-3` not `-3.0`), the dB line = `Math.round` (halves up) + `toFixed(1)` (`-0.0` -> `0.0`).
  `logic::js_num` / `js_round` / `fmt_db`; the E2E compares the attribute strings.
- Leptos 0.8 applies a signal change to the DOM asynchronously: a wasm test awaits
  `leptos::task::tick()` (twice) after a `set` or a dispatched event before reading the DOM.
- The range's position is its own `pos` signal (set at build and from the hub's values), never the
  shown `db`: a drag moves the range itself and must not be written back while it runs.
