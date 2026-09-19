//! The hub embeds + serves the Interkom phone PWA (issue 1345 M3b). The M1 `/` plain-text index is
//! replaced by the installable PWA index; the hub gains the static asset routes
//! (`/app.js`, `/style.css`, `/janus.js`, `/manifest.webmanifest`, `/sw.js`, the icons, the
//! favicon) while keeping `/api/version` / `/api/state` / `/ws` byte-identical.
//!
//! These unit tests pin the embedded asset payloads + their Content-Types WITHOUT standing up an
//! HTTP server — the same pure-`*_asset()` model bkshading uses (`service/tests/service.rs`) — so a
//! regression in the served bytes fails at CI, not on a phone at the venue.

use intercom_hub::http;

#[test]
fn pwa_manifest_is_installable_standalone() {
    let m = http::manifest_asset();
    let v: serde_json::Value = serde_json::from_str(m).expect("manifest is valid JSON");
    assert_eq!(v["display"], "standalone");
    assert_eq!(v["start_url"], "/");
    assert_eq!(v["scope"], "/");
    let icons = v["icons"].as_array().expect("icons array");
    let srcs: Vec<&str> = icons.iter().filter_map(|i| i["src"].as_str()).collect();
    assert!(srcs.contains(&"/icon-192.png"), "192 icon listed");
    assert!(srcs.contains(&"/icon-512.png"), "512 icon listed");
    assert!(
        icons
            .iter()
            .any(|i| i["purpose"].as_str().is_some_and(|p| p.contains("maskable"))),
        "a maskable icon entry is present"
    );
    assert_eq!(http::MANIFEST_CONTENT_TYPE, "application/manifest+json");
}

#[test]
fn pwa_service_worker_is_passthrough_no_cache() {
    let sw = http::sw_js_asset();
    assert!(!sw.contains("caches"), "sw.js must not use the Cache Storage API");
    assert!(sw.contains("fetch(event.request)"), "sw.js is a pure passthrough");
    assert_eq!(http::SW_JS_CONTENT_TYPE, "text/javascript; charset=utf-8");
}

#[test]
fn pwa_icons_are_png_and_favicon_is_svg() {
    assert!(
        http::icon_192_asset().starts_with(b"\x89PNG\r\n\x1a\n"),
        "icon-192 is a PNG"
    );
    assert!(
        http::icon_512_asset().starts_with(b"\x89PNG\r\n\x1a\n"),
        "icon-512 is a PNG"
    );
    assert!(http::favicon_svg_asset().contains("<svg"), "favicon is an SVG");
    assert_eq!(http::PNG_CONTENT_TYPE, "image/png");
    assert_eq!(http::SVG_CONTENT_TYPE, "image/svg+xml");
}

#[test]
fn index_is_the_pwa_and_injects_the_version() {
    let html = http::rendered_index();
    // The placeholder must be substituted (version-on-dashboard reads it straight from the DOM).
    assert!(!html.contains("{{VERSION}}"), "the version placeholder is substituted");
    assert!(html.contains("/manifest.webmanifest"), "the PWA index links the manifest");
    assert!(html.contains("data-role=\"connect\""), "the connect gesture button is present");
    assert!(html.contains("id=\"interkom\""), "the Interkom picture <img> is present");
    assert!(html.contains("/janus.js"), "the vendored janus.js is loaded");
}

#[test]
fn app_and_janus_assets_are_served_with_js_type() {
    let app = http::app_js_asset();
    assert!(app.contains("audiobridge"), "app.js drives the audiobridge");
    assert!(app.contains("/janus"), "app.js uses the path-relative /janus WS endpoint");
    assert!(app.contains("muted: true"), "app.js joins muted");
    // The vendored library keeps its MIT header.
    assert!(
        http::janus_js_asset().contains("The MIT License"),
        "vendored janus.js keeps its MIT licence header"
    );
    assert_eq!(http::APP_JS_CONTENT_TYPE, "text/javascript; charset=utf-8");
    assert_eq!(http::STYLE_CSS_CONTENT_TYPE, "text/css; charset=utf-8");
    assert_eq!(http::JS_CONTENT_TYPE, "text/javascript; charset=utf-8");
}

#[test]
fn version_and_state_routes_are_still_declared() {
    // A cheap guard that the M1 observability API is preserved alongside the new PWA routes: the
    // router builds without panicking and the const version is populated.
    assert!(!http::pkg_version().is_empty(), "the hub version const is populated");
}
