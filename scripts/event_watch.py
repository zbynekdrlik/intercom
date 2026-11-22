#!/usr/bin/env python3
"""Watch both strih hubs during an event (owner 22.11.2026: on an event day be available and assist
actively). Polls each hub's public /api/state every POLL seconds and EXITS (printing why) on the first
anomaly, so a background run wakes the session; exit 0 = anomaly found, 3 = the time bound passed.

An anomaly, compared with the previous poll of the same box:
- the box stops answering (two polls in a row) after it answered;
- `lost_ticks` grows (a block lost on every output);
- a participant's `underruns` or `overruns` grows by more than BURST in one poll;
- the phones' Janus leg leaves `joined` or its `rejoin_count` grows;
- the in-hub WebRTC relay is not `allocated` or its `failures` grow;
- a WebRTC phone's ICE state turns `failed` / `disconnected`.
A muted cambox sends nothing, so a stale cambox stream is NOT an anomaly.

Stdlib only.
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request

BOXES = {"strih-snv": "https://interkom.newlevel.media", "strih-pp": "https://interkom-pp.newlevel.media"}
BURST = 20


def fetch(base: str) -> dict | None:
    try:
        # Cloudflare answers urllib's default User-Agent with 403 (checked 22.11.2026).
        req = urllib.request.Request(base + "/api/state", headers={"User-Agent": "intercom-event-watch/1"})
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read())
    except (urllib.error.URLError, TimeoutError, ValueError, OSError):
        return None


def anomalies(prev: dict, cur: dict) -> list[str]:
    """What changed for the worse between two snapshots of one hub."""
    out = []
    if cur.get("lost_ticks", 0) > prev.get("lost_ticks", 0):
        out.append(f"lost_ticks {prev.get('lost_ticks')} -> {cur.get('lost_ticks')}")
    before = {p["name"]: p for p in prev.get("participants", [])}
    for p in cur.get("participants", []):
        b = before.get(p["name"])
        if not b:
            continue
        for k in ("underruns", "overruns"):
            grew = (p.get(k) or 0) - (b.get(k) or 0)
            if grew > BURST:
                out.append(f"{p['name']} {k} +{grew}")
        j, bj = p.get("janus"), b.get("janus")
        if j and bj:
            if bj.get("joined") and not j.get("joined"):
                out.append(f"{p['name']} janus leg left the room")
            if (j.get("rejoin_count") or 0) > (bj.get("rejoin_count") or 0):
                out.append(f"{p['name']} janus rejoin {bj.get('rejoin_count')} -> {j.get('rejoin_count')}")
    w, bw = cur.get("webrtc"), prev.get("webrtc")
    if w:
        t, bt = w.get("turn") or {}, (bw or {}).get("turn") or {}
        if t and t.get("state") != "allocated" and bt.get("state") == "allocated":
            out.append(f"webrtc turn {bt.get('state')} -> {t.get('state')} ({t.get('last_error')})")
        if (t.get("failures") or 0) > (bt.get("failures") or 0):
            out.append(f"webrtc turn failures {bt.get('failures')} -> {t.get('failures')} ({t.get('last_error')})")
        for ph in w.get("phones", []):
            if ph.get("ice_state") in ("failed", "disconnected"):
                out.append(f"webrtc phone {ph.get('display')} ice {ph.get('ice_state')}")
    return out


def main() -> int:
    poll = int(os.environ.get("POLL", "30"))
    end = time.monotonic() + int(os.environ.get("MAX_S", "21600"))
    last: dict[str, dict] = {}
    misses: dict[str, int] = {}
    while time.monotonic() < end:
        for box, base in BOXES.items():
            cur = fetch(base)
            if cur is None:
                misses[box] = misses.get(box, 0) + 1
                if box in last and misses[box] == 2:
                    print(f"{time.strftime('%F %T')} {box}: the hub stopped answering ({base})")
                    return 0
                continue
            misses[box] = 0
            if box in last:
                found = anomalies(last[box], cur)
                if found:
                    print(f"{time.strftime('%F %T')} {box}: " + "; ".join(found))
                    return 0
            last[box] = cur
        time.sleep(poll)
    print(f"{time.strftime('%F %T')} no anomaly; answering: {sorted(last)}")
    return 3


if __name__ == "__main__":
    sys.exit(main())
