//! The intercom hub's web UI (intercom#48): one Leptos CSR app for the hub's pages.
//!
//! The hub writes the page's name and its own version into the shell's `<body>` (`data-page`,
//! `data-version`, see `hub/src/ui_assets.rs`); [`start`] mounts the page that names. Phase A has
//! one page, the cutters' level mixer (`levels`, the loopback control listener `:8791`); the phone
//! PWA stays `web/app.js` until phase B.
//!
//! - [`logic`]: plain Rust (formats, the meter, the status line, a strip's send state machine),
//!   tested on the host with `cargo test`.
//! - [`levels`]: the mixer's components and their wiring to `fetch` (`net`), tested in a browser
//!   with `wasm-bindgen-test` and end to end by `tests/e2e/levels.spec.js`.

pub mod levels;
pub mod logic;
mod net;

use leptos::prelude::*;
use wasm_bindgen::prelude::wasm_bindgen;

/// The pages the hub serves from this app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// The cutters' level mixer (intercom#34/#38).
    Levels,
}

impl Page {
    /// The page a shell's `data-page` names; `None` for anything else.
    pub fn from_attr(value: &str) -> Option<Page> {
        match value {
            "levels" => Some(Page::Levels),
            _ => None,
        }
    }
}

/// The shell's `<body>` attribute, when there is a document.
fn body_attr(name: &str) -> Option<String> {
    web_sys::window()?.document()?.body()?.get_attribute(name)
}

/// The WASM entry point. A document without `data-page` (the `wasm-bindgen-test` runner) mounts
/// nothing; the tests mount their own components.
#[wasm_bindgen(start)]
pub fn start() {
    let Some(page) = body_attr("data-page") else {
        return;
    };
    console_error_panic_hook::set_once();
    let version = body_attr("data-version").unwrap_or_default();
    leptos::mount::mount_to_body(move || view! { <App page=page version=version /> });
}

/// The root: the page the shell names.
#[component]
pub fn App(page: String, version: String) -> impl IntoView {
    match Page::from_attr(&page) {
        Some(Page::Levels) => view! { <levels::LevelsPage version=version /> }.into_any(),
        None => view! {
            <p class="status" data-role="status" data-state="bad">
                "Neznáma stránka"
            </p>
        }
        .into_any(),
    }
}
