#!/usr/bin/env python3
"""Stub intercom hub for the phone-PWA Playwright E2E (issue 1345, the 25.9.2026 phone UX rework).

Serves the REAL embedded client (`intercom/web/*`) exactly the way the hub's `http.rs` does
(`{{VERSION}}` substituted in `/`), plus stub answers for the hub API the page reads:

- `/api/version` -> {"version": "<--version>"}
- `/api/state`   -> a minimal hub state with two participants
- `/interkom.mjpeg` -> a REAL endless `multipart/x-mixed-replace; boundary=frame` stream of JPEG
  parts, framed byte-for-byte like the hub's `http.rs::mjpeg_part` and sent HTTP/1.1 chunked like
  the hub (axum), cycling the three committed frames in `fixtures/picture-{0,1,2}.jpg` (a dark
  320x180 frame whose centre band is red / green / blue) at `MJPEG_FPS` until the client leaves.
  It must be the real multipart shape: issue 1379 was a service worker that swallowed exactly this
  endless response in WebKit, and a single still image never exercises that path. The fixtures were
  made once with PIL (`Image.new("RGB", (320, 180), (30, 34, 42))` + a filled centre rectangle,
  quality 90, 4:4:4); stdlib Python has no JPEG encoder, so they are committed files.
- `/__test/legacy-sw/on` | `/__test/legacy-sw/off` -> switch `/sw.js` to the pre-issue-1379 worker
  (`LEGACY_SW`, proxies every request) and back, for the worker-upgrade test.
- `/janus.js`    -> the REAL vendored janus.js. The external Janus SERVER is faked inside the page
  by the spec's init script `fake-janus-server.js` (a WebSocket double with a real in-page
  RTCPeerConnection), so the page's own code AND the library run unmodified.

Standard library only. `--port` picks the port; `/__health` is the Playwright readiness URL.
"""
import argparse
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
WEB = os.path.normpath(os.path.join(HERE, "..", "..", "web"))

STATIC = {
    "/app.js": ("app.js", "text/javascript; charset=utf-8"),
    "/style.css": ("style.css", "text/css; charset=utf-8"),
    "/manifest.webmanifest": ("manifest.webmanifest", "application/manifest+json"),
    "/sw.js": ("sw.js", "text/javascript; charset=utf-8"),
    "/icon-192.png": ("icon-192.png", "image/png"),
    "/icon-512.png": ("icon-512.png", "image/png"),
    "/favicon.svg": ("favicon.svg", "image/svg+xml"),
}


MJPEG_FPS = 10
FIXTURES = os.path.join(HERE, "fixtures")


def picture_frames():
    """The committed JPEG frames the MJPEG stream cycles through, in order."""
    names = sorted(n for n in os.listdir(FIXTURES) if n.startswith("picture-") and n.endswith(".jpg"))
    frames = []
    for name in names:
        with open(os.path.join(FIXTURES, name), "rb") as f:
            frames.append(f.read())
    if len(frames) < 2:
        raise SystemExit(f"stub_hub: need at least two picture-*.jpg frames in {FIXTURES}, found {names}")
    return frames


def mjpeg_part(jpeg):
    """One multipart part, the same bytes as the hub's `http.rs::mjpeg_part`."""
    header = f"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {len(jpeg)}\r\n\r\n"
    return header.encode("ascii") + jpeg + b"\r\n"


# The interkom worker as the hubs shipped it before issue 1379: it answered EVERY request with
# respondWith, the endless picture stream included. The upgrade test serves it first (switched on
# with `/__test/legacy-sw/on`) to put the page in the state every installed phone was in.
LEGACY_SW = b"""\
"use strict";
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (event) => {
  event.respondWith(fetch(event.request));
});
"""


def make_handler(version):
    frames = picture_frames()
    serve = {"legacy_sw": False}

    class Handler(BaseHTTPRequestHandler):
        # HTTP/1.1 like the hub (axum): every fixed response carries Content-Length, the MJPEG
        # stream is chunked.
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *args):  # keep the Playwright output clean
            pass

        def _send(self, code, ctype, body):
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            if self.path == "/sw.js":
                self.send_header("Service-Worker-Allowed", "/")
            self.end_headers()
            self.wfile.write(body)

        def _stream_mjpeg(self):
            self.send_response(200)
            self.send_header("Content-Type", "multipart/x-mixed-replace; boundary=frame")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            n = 0
            try:
                while True:
                    part = mjpeg_part(frames[n % len(frames)])
                    self.wfile.write(f"{len(part):X}\r\n".encode("ascii") + part + b"\r\n")
                    self.wfile.flush()
                    n += 1
                    time.sleep(1.0 / MJPEG_FPS)
            except OSError:
                self.close_connection = True  # the page left or replaced the picture

        def do_GET(self):
            path = self.path.split("?", 1)[0]
            if path == "/__health":
                return self._send(200, "text/plain", b"ok")
            if path in ("/__test/legacy-sw/on", "/__test/legacy-sw/off"):
                serve["legacy_sw"] = path.endswith("/on")
                return self._send(200, "text/plain", b"ok")
            if path == "/sw.js" and serve["legacy_sw"]:
                return self._send(200, "text/javascript; charset=utf-8", LEGACY_SW)
            if path == "/":
                with open(os.path.join(WEB, "index.html"), encoding="utf-8") as f:
                    html = f.read().replace("{{VERSION}}", version)
                return self._send(200, "text/html; charset=utf-8", html.encode("utf-8"))
            if path == "/janus.js":
                with open(os.path.join(WEB, "janus.js"), "rb") as f:
                    return self._send(200, "text/javascript; charset=utf-8", f.read())
            if path == "/api/version":
                return self._send(200, "application/json", json.dumps({"version": version}).encode())
            if path == "/api/state":
                state = {"participants": [{"name": "cam1"}, {"name": "phones"}]}
                return self._send(200, "application/json", json.dumps(state).encode())
            if path == "/interkom.mjpeg":
                return self._stream_mjpeg()
            if path in STATIC:
                name, ctype = STATIC[path]
                with open(os.path.join(WEB, name), "rb") as f:
                    return self._send(200, ctype, f.read())
            return self._send(404, "text/plain", b"not found")

    return Handler


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=8792)
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--version", default="1.7.0-dev.999")
    args = ap.parse_args()
    server = ThreadingHTTPServer((args.bind, args.port), make_handler(args.version))
    server.serve_forever()


if __name__ == "__main__":
    main()
