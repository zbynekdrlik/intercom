//! intercom#48 phase A: the Leptos UI embedded in the hub binary.
//!
//! `hub/build.rs` embeds `ui/dist` (Trunk's output) and FAILS the build when it is missing or
//! incomplete, never a silent fallback; its logic (`hub/build_ui.rs`) is included here and pinned
//! on scratch directories. The embedded files themselves are read through `ui_assets`.

#[path = "../build_ui.rs"]
#[allow(dead_code)]
mod build_ui;

use std::fs;
use std::path::Path;

use intercom_hub::ui_assets::{ui_file, ui_files, ui_shell, PAGE_LEVELS};

const SHELL: &str = r#"<html><body data-page="{{PAGE}}" data-version="{{VERSION}}"></body></html>"#;

fn dist_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

fn complete() -> Vec<(&'static str, &'static str)> {
    vec![
        ("index.html", SHELL),
        ("intercom-ui.js", "import"),
        ("intercom-ui_bg.wasm", "\0asm"),
        ("levels.css", ".strip{}"),
        (".trunk-lock", ""),
    ]
}

#[test]
fn a_missing_dist_fails_the_build_with_the_command_to_run() {
    let dir = tempfile::tempdir().unwrap();
    let err = build_ui::collect(&dir.path().join("dist")).unwrap_err();
    assert!(err.contains("is missing"), "{err}");
    assert!(err.contains("trunk build --release"), "{err}");
}

#[test]
fn an_incomplete_dist_fails_the_build() {
    for (drop, want) in [
        ("index.html", "no index.html"),
        ("intercom-ui_bg.wasm", "no WASM module"),
        ("intercom-ui.js", "no script"),
    ] {
        let files: Vec<_> = complete().into_iter().filter(|(n, _)| *n != drop).collect();
        let dir = dist_with(&files);
        let err = build_ui::collect(dir.path()).unwrap_err();
        assert!(err.contains(want), "without {drop}: {err}");
    }
    for placeholder in ["{{PAGE}}", "{{VERSION}}"] {
        let shell = SHELL.replace(placeholder, "x");
        let mut files = complete();
        files[0] = ("index.html", Box::leak(shell.into_boxed_str()));
        let dir = dist_with(&files);
        let err = build_ui::collect(dir.path()).unwrap_err();
        assert!(err.contains(placeholder), "{err}");
    }
    let mut files = complete();
    files.push(("notes.txt", "?"));
    let dir = dist_with(&files);
    let err = build_ui::collect(dir.path()).unwrap_err();
    assert!(err.contains("no known content type"), "{err}");
}

#[test]
fn a_complete_dist_is_embedded_file_by_file() {
    let mut files = complete();
    files.push(("icons/icon.svg", "<svg/>"));
    let dir = dist_with(&files);
    let got = build_ui::collect(dir.path()).unwrap();
    let routes: Vec<(&str, &str)> = got
        .iter()
        .map(|f| (f.route.as_str(), f.content_type))
        .collect();
    assert_eq!(
        routes,
        [
            ("/icons/icon.svg", "image/svg+xml"),
            ("/index.html", "text/html; charset=utf-8"),
            ("/intercom-ui.js", "text/javascript; charset=utf-8"),
            ("/intercom-ui_bg.wasm", "application/wasm"),
            ("/levels.css", "text/css; charset=utf-8"),
        ],
        "sorted, dotfiles left out"
    );
    assert!(got.iter().all(|f| f.file.is_absolute()));
    let src = build_ui::render(&got);
    assert!(src.starts_with("// @generated"));
    assert!(src.contains("pub static UI_FILES: &[UiFile] = &["));
    assert!(src.contains(r#"route: "/intercom-ui_bg.wasm", content_type: "application/wasm""#));
    let wasm = got.iter().find(|f| f.route.ends_with(".wasm")).unwrap();
    assert!(src.contains(&format!(
        "include_bytes!({:?})",
        wasm.file.display().to_string()
    )));
}

#[test]
fn content_types() {
    assert_eq!(build_ui::content_type("a.wasm"), Some("application/wasm"));
    assert_eq!(
        build_ui::content_type("x.webmanifest"),
        Some("application/manifest+json")
    );
    assert_eq!(build_ui::content_type("noext"), None);
    assert_eq!(build_ui::content_type("a.exe"), None);
}

#[test]
fn the_hub_carries_the_trunk_build() {
    let routes: Vec<&str> = ui_files().iter().map(|f| f.route).collect();
    for want in [
        "/index.html",
        "/intercom-ui.js",
        "/intercom-ui_bg.wasm",
        "/levels.css",
    ] {
        assert!(routes.contains(&want), "{want} in {routes:?}");
    }
    let wasm = ui_file("/intercom-ui_bg.wasm").unwrap();
    assert_eq!(&wasm.bytes[..4], b"\0asm", "a real WASM module");
    assert_eq!(wasm.content_type, "application/wasm");
    let js = std::str::from_utf8(ui_file("/intercom-ui.js").unwrap().bytes).unwrap();
    assert!(js.contains("intercom-ui_bg.wasm") || js.contains("_bg.wasm"));
    assert!(
        ui_file("/index.html").is_none(),
        "the shell only at / with its page"
    );
    // The embedded files are the checkout's ui/dist, byte for byte.
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui/dist");
    for f in ui_files() {
        let on_disk = fs::read(dist.join(f.route.trim_start_matches('/'))).unwrap();
        assert_eq!(on_disk, f.bytes, "{}", f.route);
    }
}

#[test]
fn the_shell_names_its_page_and_the_hub_version() {
    let html = ui_shell(PAGE_LEVELS);
    assert!(!html.contains("{{PAGE}}") && !html.contains("{{VERSION}}"));
    assert!(html.contains(r#"data-page="levels""#), "{html}");
    assert!(html.contains(&format!(r#"data-version="{}""#, env!("CARGO_PKG_VERSION"))));
    assert!(html.contains(r#"lang="sk""#));
    assert!(html.contains("/intercom-ui.js"));
    assert!(html.contains("/levels.css"));
    assert!(
        !html.contains(r#"rel="preload" href="/intercom-ui_bg.wasm""#),
        "no WASM preload: WebKit warns about it in the console"
    );
}
