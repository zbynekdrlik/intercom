// The UI embedding of `hub/build.rs` (intercom#48), std only so the hub's tests can include it
// (`hub/tests/ui_embed_48.rs`, `#[path]`) and pin the refusals.
//
// The hub serves the Leptos UI Trunk built into `ui/dist` (`trunk build --release` in `ui/`). Every
// file there is compiled into the binary with `include_bytes!`, so the box needs no file at run
// time. The hub build FAILS when `dist` is missing or incomplete, when a UI source is newer than
// the built page (`check_fresh`: edited, never rebuilt), or when a dist file would take a route the
// hub serves itself: a hub without its page, with a stale one, or one that would panic at start
// never ships silently.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One file of `ui/dist` to embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistFile {
    /// The URL path it is served at (`/levels.css`).
    pub route: String,
    /// The absolute file to include.
    pub file: PathBuf,
    pub content_type: &'static str,
}

/// The placeholders the hub fills in the shell when it serves it.
pub const SHELL_PLACEHOLDERS: [&str; 2] = ["{{PAGE}}", "{{VERSION}}"];

/// Routes the hub's own handlers serve next to the UI files (`levels_http::control_router`): a dist
/// file there would be a duplicate route, which panics axum's router at start. `/api/` is a prefix.
pub const HUB_ROUTES: [&str; 2] = ["/", "/favicon.svg"];
pub const HUB_ROUTE_PREFIX: &str = "/api/";

/// The command that makes `ui/dist`.
pub const BUILD_HINT: &str = "build the UI first: `cd ui && trunk build --release` (intercom#48)";

/// The content type of a file Trunk writes, by extension; `None` = not a file the UI ships.
pub fn content_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1;
    Some(match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        _ => return None,
    })
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<DistFile>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("read {}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Trunk's own `.trunk-lock` and any other dotfile are not part of the page.
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let route = format!("{prefix}/{name}");
        if path.is_dir() {
            walk(&path, &route, out)?;
            continue;
        }
        if HUB_ROUTES.contains(&route.as_str()) || route.starts_with(HUB_ROUTE_PREFIX) {
            return Err(format!(
                "{}: the UI file's route {route} is one the hub serves itself (rename it in ui/)",
                path.display()
            ));
        }
        let Some(content_type) = content_type(&name) else {
            return Err(format!(
                "{}: a UI file with no known content type (add it to build_ui.rs)",
                path.display()
            ));
        };
        let file = path
            .canonicalize()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        out.push(DistFile {
            route,
            file,
            content_type,
        });
    }
    Ok(())
}

/// Every file of `dist`, or why it cannot be embedded: missing, no shell (`index.html` with both
/// placeholders), no WASM module, no script, or a file of an unknown type.
pub fn collect(dist: &Path) -> Result<Vec<DistFile>, String> {
    if !dist.is_dir() {
        return Err(format!("{} is missing: {BUILD_HINT}", dist.display()));
    }
    let mut files = Vec::new();
    walk(dist, "", &mut files)?;
    let Some(shell) = files.iter().find(|f| f.route == "/index.html") else {
        return Err(format!(
            "{} has no index.html: {BUILD_HINT}",
            dist.display()
        ));
    };
    let html = std::fs::read_to_string(&shell.file)
        .map_err(|e| format!("{}: {e}", shell.file.display()))?;
    for p in SHELL_PLACEHOLDERS {
        if !html.contains(p) {
            return Err(format!(
                "{}: the shell lacks {p} (ui/index.html's <body> must carry it)",
                shell.file.display()
            ));
        }
    }
    for (ext, what) in [(".wasm", "WASM module"), (".js", "script")] {
        if !files.iter().any(|f| f.route.ends_with(ext)) {
            return Err(format!(
                "{} has no {what} ({ext}): {BUILD_HINT}",
                dist.display()
            ));
        }
    }
    Ok(files)
}

/// The Rust source the hub includes: `pub static UI_FILES: &[UiFile]`.
pub fn render(files: &[DistFile]) -> String {
    let mut out = String::from(
        "// @generated by hub/build.rs from ui/dist (intercom#48).\n\
         pub static UI_FILES: &[UiFile] = &[\n",
    );
    for f in files {
        writeln!(
            out,
            "    UiFile {{ route: {:?}, content_type: {:?}, bytes: include_bytes!({:?}) }},",
            f.route,
            f.content_type,
            f.file.display().to_string()
        )
        .expect("write to a String");
    }
    out.push_str("];\n");
    out
}

/// The files under `ui/` that make the page: `src/**`, `index.html`, every top-level `*.css`,
/// `Cargo.toml`, `Trunk.toml` (sorted). Not the tests, `dist/` or `target/`.
pub fn ui_sources(ui: &Path) -> Result<Vec<PathBuf>, String> {
    fn rs_tree(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|e| format!("read {}: {e}", dir.display()))?
                .path();
            if path.is_dir() {
                rs_tree(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    rs_tree(&ui.join("src"), &mut out)?;
    let entries = std::fs::read_dir(ui).map_err(|e| format!("read {}: {e}", ui.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("read {}: {e}", ui.display()))?
            .path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let wanted = match name.as_deref() {
            Some("index.html" | "Cargo.toml" | "Trunk.toml") => true,
            Some(n) => n.ends_with(".css"),
            None => false,
        };
        if wanted && path.is_file() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// The page's sources (for `rerun-if-changed`), or why `ui/dist` is stale: a source newer than
/// `dist/index.html` (Trunk writes it last) means the UI was edited and never rebuilt, so the hub
/// would embed the OLD page.
pub fn check_fresh(ui: &Path) -> Result<Vec<PathBuf>, String> {
    let shell = ui.join("dist/index.html");
    let built = std::fs::metadata(&shell)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{}: {e}: {BUILD_HINT}", shell.display()))?;
    let sources = ui_sources(ui)?;
    for src in &sources {
        let changed = std::fs::metadata(src)
            .and_then(|m| m.modified())
            .map_err(|e| format!("{}: {e}", src.display()))?;
        if changed > built {
            let rel = src.strip_prefix(ui).unwrap_or(src);
            return Err(format!(
                "ui/dist is stale: ui/{} changed after the last UI build ({}): {BUILD_HINT}",
                rel.display(),
                shell.display()
            ));
        }
    }
    Ok(sources)
}
