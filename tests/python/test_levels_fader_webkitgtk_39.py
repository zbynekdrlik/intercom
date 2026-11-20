"""intercom#38/#39 (live 20.11.2026): the tray's mix window is WebKitGTK 2.52, which ignores
`writing-mode: vertical-lr` on a range input and draws each fader as a tiny horizontal slider.
Playwright's WebKit and Chromium both honour it, so no browser test sees the bug; an Xvfb run of the
real tray showed three variants: writing-mode = horizontal (broken), `appearance: slider-vertical`
= vertical but deprecated in Chromium (a console warning), a -90deg rotated range = vertical
everywhere. The page must use the rotated range and never depend on the other two."""

import re
from pathlib import Path

WEB = Path(__file__).resolve().parents[2] / "web"


def _rule(css: str, selector: str) -> str:
    m = re.search(re.escape(selector) + r"\s*\{([^}]*)\}", css)
    assert m, f"no CSS rule for {selector}"
    return m.group(1)


def test_the_fader_never_relies_on_writing_mode_or_slider_vertical():
    css = (WEB / "levels.css").read_text()
    assert "writing-mode" not in css, "WebKitGTK draws a writing-mode range horizontally"
    assert "slider-vertical" not in css, "deprecated in Chromium: a console warning"


def test_the_fader_is_a_rotated_range_inside_an_upright_box():
    css = (WEB / "levels.css").read_text()
    html = (WEB / "levels.html").read_text()
    assert re.search(
        r'<div class="strip-fader">\s*<input class="strip-slider" data-role="slider" type="range"', html
    ), "the range sits in its own upright box"
    box = _rule(css, ".strip-fader")
    assert "position: relative" in box
    slider = _rule(css, ".strip-slider")
    assert "rotate(-90deg)" in slider, "the range is turned so its max is at the top"
    assert "position: absolute" in slider
