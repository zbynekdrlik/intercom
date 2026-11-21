//! The Leptos web UI (`ui/`, intercom#48), embedded at build time by `hub/build.rs` from
//! `ui/dist` (Trunk's output): the shell `index.html`, the script, the WASM module, the stylesheet.
//!
//! One app serves every page: the hub writes the page's name and its own version into the shell's
//! `<body>` (`data-page`, `data-version`) and the root component mounts that page. Phase A: only
//! the control listener (`levels_http`, page `levels`) serves it; the phone listener still serves
//! `web/` until phase B.

use axum::{
    http::header,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};

/// One embedded UI file.
#[derive(Debug)]
pub struct UiFile {
    /// The URL path (`/levels.css`).
    pub route: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/ui_files.rs"));

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The page names the shell takes (`ui/src/lib.rs` `Page::from_attr`).
pub const PAGE_LEVELS: &str = "levels";

/// Every embedded file but the shell, by its route.
pub fn ui_file(route: &str) -> Option<&'static UiFile> {
    UI_FILES
        .iter()
        .find(|f| f.route == route && f.route != "/index.html")
}

/// Every embedded file (the shell included).
pub fn ui_files() -> &'static [UiFile] {
    UI_FILES
}

/// The shell for `page`, with the hub's version.
pub fn ui_shell(page: &str) -> String {
    let shell = UI_FILES
        .iter()
        .find(|f| f.route == "/index.html")
        .expect("build.rs refuses a dist without index.html");
    String::from_utf8_lossy(shell.bytes)
        .replace("{{PAGE}}", page)
        .replace("{{VERSION}}", VERSION)
}

/// `page` at `/` plus every UI file at its own route, all `no-store` (fixed names, so a new hub
/// must never meet a cached script of the old one).
pub fn ui_routes<S: Clone + Send + Sync + 'static>(page: &'static str) -> Router<S> {
    let mut router = Router::new().route(
        "/",
        get(move || async move {
            tracing::debug!(page, "ui: shell served");
            ([(header::CACHE_CONTROL, "no-store")], Html(ui_shell(page)))
        }),
    );
    for f in UI_FILES.iter().filter(|f| f.route != "/index.html") {
        router = router.route(
            f.route,
            get(move || async move {
                (
                    [
                        (header::CONTENT_TYPE, f.content_type),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    f.bytes,
                )
                    .into_response()
            }),
        );
    }
    router
}
