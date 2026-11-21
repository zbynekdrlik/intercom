// Embeds the Leptos UI (`ui/dist`, built by Trunk) into the hub binary (intercom#48). A missing
// or incomplete `ui/dist` FAILS the build: never a hub without its page, never a silent fallback.
// The logic lives in build_ui.rs so the hub's tests pin it (hub/tests/ui_embed_48.rs).

#[path = "build_ui.rs"]
mod build_ui;

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let dist = manifest.join("../ui/dist");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_ui.rs");
    // A directory: cargo rescans every file in it, so a new `trunk build` re-embeds.
    println!("cargo:rerun-if-changed={}", dist.display());
    let files = match build_ui::collect(&dist) {
        Ok(files) => files,
        Err(e) => panic!("intercom-hub: cannot embed the web UI: {e}"),
    };
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.file.display());
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets it")).join("ui_files.rs");
    std::fs::write(&out, build_ui::render(&files))
        .unwrap_or_else(|e| panic!("intercom-hub: write {}: {e}", out.display()));
}
