#!/usr/bin/env python3
"""Stub intercom hub for the phone-PWA Playwright E2E (issue 1345, the 25.9.2026 phone UX rework).

Serves the REAL embedded client (`intercom/web/*`) exactly the way the hub's `http.rs` does
(`{{VERSION}}` substituted in `/`), plus stub answers for the hub API the page reads:

- `/api/version` -> {"version": "<--version>"}
- `/api/state`   -> a minimal hub state with two participants
- `/interkom.mjpeg` -> a small generated PNG (the page shows it in an <img>; the format does not
  matter to the browser, only that the route answers 200 with an image)
- `/janus.js`    -> the test double `fake-janus.js` (the real Janus is an external network service
  the test cannot run; the double answers the audiobridge join/configure and hands the page a
  fake-device audio track, so the page's own code runs unmodified). `--real-janus` serves the
  vendored library instead, for a manual run against a live Janus.

Standard library only. `--port` picks the port; `/__health` is the Playwright readiness URL.
"""
import argparse
import json
import os
import struct
import zlib
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


def picture_png(width=320, height=180):
    """A deterministic 16:9 test picture: a dark frame with a lighter centre band."""
    rows = []
    for y in range(height):
        row = bytearray([0])  # filter byte: none
        for x in range(width):
            band = height // 3 <= y < 2 * height // 3 and width // 4 <= x < 3 * width // 4
            row += bytes((90, 140, 200) if band else (30, 34, 42))
        rows.append(bytes(row))
    raw = b"".join(rows)

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))


def make_handler(version, real_janus):
    picture = picture_png()

    class Handler(BaseHTTPRequestHandler):
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

        def do_GET(self):
            path = self.path.split("?", 1)[0]
            if path == "/__health":
                return self._send(200, "text/plain", b"ok")
            if path == "/":
                with open(os.path.join(WEB, "index.html"), encoding="utf-8") as f:
                    html = f.read().replace("{{VERSION}}", version)
                return self._send(200, "text/html; charset=utf-8", html.encode("utf-8"))
            if path == "/janus.js":
                src = os.path.join(WEB, "janus.js") if real_janus else os.path.join(HERE, "fake-janus.js")
                with open(src, "rb") as f:
                    return self._send(200, "text/javascript; charset=utf-8", f.read())
            if path == "/api/version":
                return self._send(200, "application/json", json.dumps({"version": version}).encode())
            if path == "/api/state":
                state = {"participants": [{"name": "cam1"}, {"name": "phones"}]}
                return self._send(200, "application/json", json.dumps(state).encode())
            if path == "/interkom.mjpeg":
                return self._send(200, "image/png", picture)
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
    ap.add_argument("--version", default="0.0.0-e2e")
    ap.add_argument("--real-janus", action="store_true", help="serve the vendored janus.js")
    args = ap.parse_args()
    server = ThreadingHTTPServer((args.bind, args.port), make_handler(args.version, args.real_janus))
    server.serve_forever()


if __name__ == "__main__":
    main()
