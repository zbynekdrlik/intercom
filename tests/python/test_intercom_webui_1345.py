#!/usr/bin/env python3
"""Interkom phone PWA structural tests (issue 1345 M3b).

The strih-lx intercom hub serves an installable phone web app (PWA) at `/` — a cameraman opens ONE
link, taps "Pripojiť" (the single autoplay + getUserMedia gesture), immediately hears the intercom
(WebRTC via a vendored janus.js → Janus audiobridge) and sees the Interkom picture (MJPEG from the
hub, M3c / issue 1347), and can rarely pick a mic and unmute it.

These stdlib-only structural tests run in the `python-tests` CI job (no browser, no Rust toolchain).
They pin the PWA CONTRACT so the embedded assets can't silently regress:
 - every asset file exists;
 - the manifest is an installable standalone app with 192/512 + maskable icons;
 - the service worker is a pure passthrough (NO Cache Storage API — a cached intercom client is a
   broken one);
 - the page has EXACTLY ONE primary connect button (the gesture), a mic <select>, a mic toggle that
   DEFAULTS TO MUTED, and the <img> that shows the /interkom.mjpeg picture;
 - app.js joins the room MUTED (`muted: true`), talks to Janus over a PATH-RELATIVE `/janus` WS URL
   (derived from location.host, never a hard-coded host), and NEVER logs to the console on a handled
   failure (browser-console-zero-errors — failures become status chips);
 - the vendored janus.js keeps its MIT licence header and its pinned tag is recorded in VENDORED.md;
 - NO `localhost` / `127.0.0.1` appears anywhere in the served assets.

A python static test CANNOT catch a JS/CSS runtime bug — the real browser check is a Playwright run
against the hub-served page (a Tier-0 lane runs it against `python3 -m http.server intercom/web`).
Runnable directly (`python3 tests/python/test_intercom_webui_1345.py`) or under pytest.
"""
import json
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, "..", ".."))
WEB = os.path.join(REPO, "intercom", "web")

INDEX = os.path.join(WEB, "index.html")
APP_JS = os.path.join(WEB, "app.js")
STYLE = os.path.join(WEB, "style.css")
MANIFEST = os.path.join(WEB, "manifest.webmanifest")
SW = os.path.join(WEB, "sw.js")
GEN_ICONS = os.path.join(WEB, "gen-icons.py")
ICON_192 = os.path.join(WEB, "icon-192.png")
ICON_512 = os.path.join(WEB, "icon-512.png")
FAVICON = os.path.join(WEB, "favicon.svg")
JANUS = os.path.join(WEB, "janus.js")
VENDORED = os.path.join(WEB, "VENDORED.md")

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


def _read(p):
    with open(p, encoding="utf-8") as f:
        return f.read()


def test_all_assets_exist():
    for p in (INDEX, APP_JS, STYLE, MANIFEST, SW, GEN_ICONS, ICON_192, ICON_512, FAVICON,
              JANUS, VENDORED):
        assert os.path.isfile(p), "missing PWA asset: %s" % p


def test_icons_are_png_and_favicon_is_svg():
    for p in (ICON_192, ICON_512):
        with open(p, "rb") as f:
            assert f.read(8) == PNG_MAGIC, "%s must be a PNG" % p
    assert "<svg" in _read(FAVICON), "favicon must be an SVG"


def test_manifest_is_installable_standalone():
    m = json.loads(_read(MANIFEST))
    assert m["display"] == "standalone"
    assert m["start_url"] == "/"
    assert m["scope"] == "/"
    assert m.get("name"), "manifest needs a name"
    srcs = [i.get("src") for i in m["icons"]]
    assert "/icon-192.png" in srcs, "192 icon listed"
    assert "/icon-512.png" in srcs, "512 icon listed"
    assert any("maskable" in (i.get("purpose") or "") for i in m["icons"]), (
        "a maskable icon entry is present (install-quality icon on Android/Chrome)"
    )


def test_service_worker_is_passthrough_no_cache():
    sw = _read(SW)
    assert "caches" not in sw, "sw.js must NOT use the Cache Storage API (server-truth intercom)"
    assert "fetch(event.request)" in sw, "sw.js must be a pure network passthrough"


def test_index_links_manifest_and_serves_pwa_icons():
    html = _read(INDEX)
    assert 'rel="manifest"' in html and "/manifest.webmanifest" in html
    assert "/icon-192.png" in html
    assert 'name="theme-color"' in html


def test_index_has_exactly_one_primary_connect_button():
    html = _read(INDEX)
    # EXACTLY ONE big primary action (the autoplay/getUserMedia gesture).
    primary = re.findall(r'class="[^"]*\bbtn-primary\b[^"]*"', html)
    assert len(primary) == 1, "expected exactly ONE primary connect button, found %d" % len(primary)
    connect = re.findall(r'data-role="connect"', html)
    assert len(connect) == 1, "expected exactly ONE data-role=connect element"
    # The Slovak connect label the cameraman taps.
    assert "Pripojiť" in html


def test_index_has_mic_select_and_muted_default_toggle():
    html = _read(INDEX)
    assert 'data-role="mic-select"' in html, "a mic <select> is present"
    assert "<select" in html
    # The mic toggle DEFAULTS TO MUTED (off).
    toggle = re.search(r'data-role="mic-toggle"[^>]*>', html)
    assert toggle, "a mic toggle is present"
    tag = re.search(r'<button[^>]*data-role="mic-toggle"[^>]*>', html).group(0)
    assert 'data-muted="true"' in tag, "the mic toggle must default to MUTED"
    assert 'aria-pressed="false"' in tag, "the mic toggle is not pressed (muted) by default"


def test_index_has_interkom_mjpeg_picture():
    html = _read(INDEX)
    assert 'id="interkom"' in html, "the Interkom <img> is present"
    # The <img> src OR the app.js src assignment targets the MJPEG route.
    assert "/interkom.mjpeg" in html or "/interkom.mjpeg" in _read(APP_JS), (
        "the picture must target /interkom.mjpeg (M3c contract)"
    )
    assert '<audio' in html and "playsinline" in html, "a playsinline autoplay <audio> sink"


def test_app_js_joins_muted():
    js = _read(APP_JS)
    # The join/configure payload must carry muted:true so the phone joins SILENT.
    assert re.search(r"muted:\s*true", js), "app.js must join/configure with muted: true"
    assert re.search(r'request:\s*"join"', js), "app.js sends an audiobridge join"
    assert "audiobridge" in js, "app.js attaches the audiobridge plugin"


def test_app_js_uses_path_relative_janus_ws():
    js = _read(APP_JS)
    # PATH-RELATIVE: the /janus WS URL is derived from location.host, never a hard-coded host:port.
    assert "location.host" in js, "the janus WS URL is built from location.host"
    assert "/janus" in js, "the janus WS endpoint path is /janus"
    assert re.search(r'wss?:', js), "the WS scheme is chosen from location.protocol"
    # no hard-coded IP:port for janus.
    assert not re.search(r"wss?://\d+\.\d+\.\d+\.\d+", js), "no hard-coded janus host in app.js"


def test_app_js_never_logs_console_error_on_failure():
    js = _read(APP_JS)
    assert "console.error" not in js, "handled failures must become chips, never console.error"
    assert "console.warn" not in js, "handled failures must become chips, never console.warn"
    # Failures are surfaced as chips instead.
    assert "chip(" in js, "app.js surfaces state via chips"


def test_no_localhost_in_served_assets():
    for p in (INDEX, APP_JS, STYLE, MANIFEST, SW):
        body = _read(p)
        assert "localhost" not in body, "%s must not reference localhost" % p
        assert "127.0.0.1" not in body, "%s must not reference 127.0.0.1" % p


def test_vendored_janus_keeps_mit_header_and_pinned_tag():
    janus = _read(JANUS)
    assert "The MIT License" in janus, "the vendored janus.js MUST keep its MIT licence header"
    assert "Meetecho" in janus, "the janus.js copyright header is intact"
    vend = _read(VENDORED)
    assert "janus.js" in vend
    assert "MIT" in vend, "VENDORED.md records the MIT licence"
    # The pinned tag must be recorded AND must match the Janus version the OS ships (1.1.2).
    assert "v1.1.2" in vend, "VENDORED.md must record the pinned janus-gateway tag (v1.1.2)"
    assert "meetecho/janus-gateway" in vend, "VENDORED.md names the upstream repo"


if __name__ == "__main__":
    import traceback

    fns = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    failed = 0
    for fn in fns:
        try:
            fn()
            print("ok   %s" % fn.__name__)
        except Exception:
            failed += 1
            print("FAIL %s" % fn.__name__)
            traceback.print_exc()
    raise SystemExit(1 if failed else 0)
