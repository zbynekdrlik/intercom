#!/usr/bin/env python3
"""Deterministic generator for the Interkom phone PWA icons (issue 1345 M3b).

Pure stdlib (zlib + struct) PNG writer — NO new dependency (no Pillow). Draws a simple
headset/intercom glyph (a headband arc + two ear cups) on the panel's dark background so the
installed phone web app has a recognisable icon on the home screen. Emits:

  * icon-192.png / icon-512.png  — RGBA, opaque; the glyph sits inside the central ~60% so the
                                    icons are safe as a maskable icon (the crop keeps the glyph).
  * favicon.svg                  — the same glyph as scalable SVG.

Run from anywhere: it writes next to itself (intercom/web/). Committed as small binaries so the
hub embeds them via include_bytes! (self-contained, no runtime file dependency). Re-run after
changing the palette/glyph; the output is deterministic (fixed geometry + zlib level 9).

Mirrors bkshading/service/web/gen-icons.py (issue 1305) — same stdlib PNG writer + supersampling,
only the glyph differs (a headset, not a camera aperture).
"""
import os
import struct
import zlib

# Palette (matches style.css): bg #14171c, accent #4f9dff, ear cup #262c36, highlight #e7ecf2.
BG = (20, 23, 28)
BAND = (79, 157, 255)
CUP = (38, 44, 54)
HI = (231, 236, 242)

HERE = os.path.dirname(os.path.abspath(__file__))


def _sample(fx, fy, size):
    """Topmost glyph colour at point (fx, fy) on an `size`x`size` canvas (a headset glyph)."""
    cx = size / 2.0
    cy = 0.52 * size
    # Headband: an annulus (open at the bottom) centred a little above the middle.
    r_band_out = 0.30 * size
    r_band_in = 0.235 * size
    d = ((fx - cx) ** 2 + (fy - cy) ** 2) ** 0.5
    if r_band_in <= d <= r_band_out and fy <= cy:
        return BAND
    # Two ear cups: rounded rectangles at the band ends (left + right), just below the band centre.
    cup_w = 0.085 * size
    cup_h = 0.16 * size
    cup_cy = cy + 0.02 * size
    for sign in (-1.0, 1.0):
        ccx = cx + sign * 0.2675 * size
        if abs(fx - ccx) <= cup_w and abs(fy - cup_cy) <= cup_h:
            # a small highlight dot on each cup (top-inner corner)
            hx = ccx - sign * 0.03 * size
            hy = cup_cy - 0.07 * size
            if ((fx - hx) ** 2 + (fy - hy) ** 2) ** 0.5 <= 0.025 * size:
                return HI
            return CUP
    return BG


def _render_rgba(size):
    """Raw PNG scanlines (each prefixed with a 0 filter byte), RGBA, 3x3 supersampled edges."""
    ss = 3
    n = ss * ss
    raw = bytearray()
    for y in range(size):
        raw.append(0)  # filter type 0 (None)
        for x in range(size):
            r = g = b = 0
            for sy in range(ss):
                for sx in range(ss):
                    fx = x + (sx + 0.5) / ss
                    fy = y + (sy + 0.5) / ss
                    cr, cg, cb = _sample(fx, fy, size)
                    r += cr
                    g += cg
                    b += cb
            raw += bytes((round(r / n), round(g / n), round(b / n), 255))
    return bytes(raw)


def _chunk(tag, data):
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


def _write_png(path, size):
    raw = _render_rgba(size)
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit, colour type 6 (RGBA)
    idat = zlib.compress(raw, 9)
    png = b"\x89PNG\r\n\x1a\n" + _chunk(b"IHDR", ihdr) + _chunk(b"IDAT", idat) + _chunk(b"IEND", b"")
    with open(path, "wb") as fh:
        fh.write(png)
    print(f"wrote {path} ({len(png)} bytes)")


def _hex(c):
    return "#%02x%02x%02x" % c


def _write_svg(path):
    # viewBox 0..64, geometry scaled from the same fractions as the PNG glyph.
    band = (
        "M 12.8 34.56 A 19.2 19.2 0 0 1 51.2 34.56"
    )
    svg = (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" '
        'role="img" aria-label="Interkom">\n'
        f'  <rect width="64" height="64" rx="12" fill="{_hex(BG)}"/>\n'
        f'  <path d="{band}" fill="none" stroke="{_hex(BAND)}" stroke-width="4.16"/>\n'
        f'  <rect x="8.7" y="31.5" width="10.9" height="20.5" rx="4" fill="{_hex(CUP)}"/>\n'
        f'  <rect x="44.4" y="31.5" width="10.9" height="20.5" rx="4" fill="{_hex(CUP)}"/>\n'
        f'  <circle cx="16.3" cy="35.8" r="1.6" fill="{_hex(HI)}"/>\n'
        f'  <circle cx="47.7" cy="35.8" r="1.6" fill="{_hex(HI)}"/>\n'
        "</svg>\n"
    )
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(svg)
    print(f"wrote {path} ({len(svg)} bytes)")


def main():
    _write_png(os.path.join(HERE, "icon-192.png"), 192)
    _write_png(os.path.join(HERE, "icon-512.png"), 512)
    _write_svg(os.path.join(HERE, "favicon.svg"))


if __name__ == "__main__":
    main()
