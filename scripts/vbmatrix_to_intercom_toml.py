#!/usr/bin/env python3
"""Convert a VB-Audio Matrix settings XML into the strih-lx intercom hub's intercom.toml (issue 1345).

The Windows strih runs VB-Audio Matrix as a static N-1 intercom: a 216-cell routing grid over the
7 cambox VBAN streams, the 2 cutter mics + 2 stereo cans on the MiniFuse (ASIO32), the phones
(VAIO1), the line-3/4 mic (WIN1.IN), the local speakers (WIN1.OUT), an OBS program-monitor sink
(VASIO8) and the attenuated program references (fohabl/lv1). The strih-lx Linux hub replaces that
GUI matrix with a DECLARATIVE `intercom.toml` GENERATED from this XML by THIS converter, so the
routing (incl. the −8/−10 dB program refs + the small asymmetries) is reproduced with fidelity and
lives in git, never a GUI.

Slot → hub participant mapping (derived from the XML's own attributes, not a hard-coded table):

- VBAN slots (doc order = VBANStreamIn index): a slot with an ACTIVE VBANStreamOut (`status=1`) is a
  `cambox` (cam1..cam7, VBAN in+out to `camN.lan`); one without is a `program_ref` (fohabl/lv1/mbc,
  VBAN in only — the stream name minus the `-strih` suffix).
- `AMDevice` type 256 (the MiniFuse ASIO master) → `cutters`; type 4 (WDM out) → `speakers`; type 1
  (WDM in) → `line34`.
- an online `VAIOSlot` (VAIO1) → `phones`; an online `VASIOSlot` (VASIO8) → `program_monitor`.

Only the VBAN adapter is live in M1; every non-VBAN participant is declared with `adapter = "none"`
(its PipeWire/Janus I/O is M2/M3), so the engine computes its mix but nothing delivers it yet.

Usage:
    python3 scripts/vbmatrix_to_intercom_toml.py <vbmatrix.xml> > intercom/intercom.strih-lx.toml
"""

import sys
import xml.etree.ElementTree as ET

# The hub-wide config emitted into the `[hub]` table. The HTTP panel binds :8790 (the bkshading
# family), the single VBAN receive socket binds the standard :6980, 48 kHz, 256-frame blocks.
HUB = {
    "bind": "0.0.0.0:8790",
    "vban_bind": "0.0.0.0:6980",
    "sample_rate": 48000,
    "block_frames": 256,
}

# The Janus audiobridge edge config emitted into the `[janus]` table (issue 1345 M3a). The `phones`
# participant is carried over the Janus audiobridge room as a plain-RTP PCMU participant. The room
# SECRET is never inlined here — the hub reads it from the 0600 `room_secret_file` at start.
JANUS = {
    "api_url": "http://127.0.0.1:8088/janus",
    "room": 1000,
    "room_secret_file": "/etc/intercom-hub/janus-room.secret",
    "rtp_bind": "0.0.0.0:6990",
}

# The Interkom picture (MJPEG) config emitted into the `[video]` table (issue 1345 M3c). The hub
# receives the `STRIH-LX (interkom)` NDI source in LOW-bandwidth mode, decimates to `fps`, JPEG-encodes
# at `jpeg_quality`, and serves it as `multipart/x-mixed-replace` at `/interkom.mjpeg`. Until issue
# 1347 builds the `STRIH-LX (interkom)` NDI output, the supervisor points `ndi_source_name` at
# `CAM1 (usb)` (a config edit on the box, no code change / no re-generate).
VIDEO = {
    "ndi_source_name": "STRIH-LX (interkom)",
    "fps": 10,
    "jpeg_quality": 70,
    "enabled": True,
}

# Participant emit order: camboxes, then the program references, then the interface participants.
_ROLE_ORDER = {
    "cambox": 0,
    "program_ref": 1,
    "cutters": 2,
    "phones": 3,
    "speakers": 4,
    "line34": 5,
    "program_monitor": 6,
}

_STRIH_SUFFIX = "-strih"


def _strip_suffix(name):
    """`fohabl-strih` → `fohabl`; `cam1` → `cam1`."""
    return name[: -len(_STRIH_SUFFIX)] if name.endswith(_STRIH_SUFFIX) else name


def _cam_number(name):
    """The trailing integer of a `camN` name (for the emit order), else a large sentinel."""
    if name.startswith("cam") and name[3:].isdigit():
        return int(name[3:])
    return 10_000


def build_model(xml_text):
    """Parse the VB-Matrix XML into (hub, participants, points).

    `participants` is an ordered list of dicts (name/role/adapter[/host/in_stream/out_stream]/
    in_channels/out_channels); `points` is the routing list in the XML's document order, each a dict
    (src/in_ch/dst/out_ch/gain_db/mute) with slot names resolved to participant names. Raises on any
    unmapped slot or malformed element.
    """
    root = ET.fromstring(xml_text)

    # VBAN streams, keyed by index.
    vban_in = {}  # index -> (name, ip)
    vban_out = {}  # index -> (name, status)
    for e in root.iter("VBANStreamIn"):
        vban_in[int(e.get("index"))] = (e.get("name"), e.get("ip"))
    for e in root.iter("VBANStreamOut"):
        vban_out[int(e.get("index"))] = (e.get("name"), e.get("status"))

    slot_to_part = {}  # slot uniq -> participant name
    participants = {}  # name -> dict (channels filled after the points are read)

    def add(name, role, adapter, order_key, host=None, in_stream=None, out_stream=None):
        p = {
            "name": name,
            "role": role,
            "adapter": adapter,
            "order": (_ROLE_ORDER[role], order_key, name),
        }
        if host is not None:
            p["host"] = host
        if in_stream is not None:
            p["in_stream"] = in_stream
        if out_stream is not None:
            p["out_stream"] = out_stream
        participants[name] = p

    # VBAN slots (doc order = stream index).
    for idx, slot in enumerate(root.iter("VBANSlot"), start=1):
        if idx not in vban_in:
            raise ValueError(f"VBANSlot #{idx} ({slot.get('uniq')}) has no matching VBANStreamIn")
        stream_name, ip = vban_in[idx]
        part = _strip_suffix(stream_name)
        out = vban_out.get(idx)
        active_out = out is not None and out[1] == "1"
        if active_out and stream_name.startswith("cam"):
            add(
                part,
                "cambox",
                "vban",
                _cam_number(part),
                host=ip,
                in_stream=stream_name,
                out_stream=out[0],
            )
        else:
            add(part, "program_ref", "vban", idx, host=ip, in_stream=stream_name)
        slot_to_part[slot.get("uniq")] = part

    # MiniFuse / speakers / line-3-4 (AMDevice) by type.
    for e in root.iter("AMDevice"):
        uniq = e.get("uniq")
        dtype = e.get("type")
        if dtype == "256":
            add("cutters", "cutters", "none", 0)
            slot_to_part[uniq] = "cutters"
        elif dtype == "4":
            add("speakers", "speakers", "none", 0)
            slot_to_part[uniq] = "speakers"
        elif dtype == "1":
            add("line34", "line34", "none", 0)
            slot_to_part[uniq] = "line34"

    # Phones (the OBS/VDO.Ninja side, VAIO1) — the first online VAIOSlot. Since issue 1345 M3a the
    # phones participant is carried over the Janus audiobridge (adapter `janus`), not VB-Matrix/VDO;
    # its `[janus]` config table is emitted by render().
    for e in root.iter("VAIOSlot"):
        if e.get("online") == "1":
            add("phones", "phones", "janus", 0)
            slot_to_part[e.get("uniq")] = "phones"
            break

    # Program monitor (OBS program-audio sink, VASIO8) — the first online VASIOSlot.
    for e in root.iter("VASIOSlot"):
        if e.get("online") == "1":
            add("program_monitor", "program_monitor", "none", 0)
            slot_to_part[e.get("uniq")] = "program_monitor"
            break

    # Points (routing grid), in document order.
    points = []
    for e in root.iter("Point"):
        si, so = e.get("slotin"), e.get("slotout")
        if si not in slot_to_part:
            raise ValueError(f"point slotin '{si}' has no participant")
        if so not in slot_to_part:
            raise ValueError(f"point slotout '{so}' has no participant")
        points.append(
            {
                "src": slot_to_part[si],
                "in_ch": int(e.get("in")),
                "dst": slot_to_part[so],
                "out_ch": int(e.get("out")),
                "gain_db": float(e.get("dBGain")),
                # A point is muted ONLY when it explicitly says so (`mute='1'`); an absent/`'0'`
                # attribute means routed (never treat a missing attribute as MUTED — that would
                # silently DROP the point, inverting the converter's fail-loud ethos).
                "mute": e.get("mute") == "1",
            }
        )

    # Channel counts = the max channel index each participant is routed on.
    for p in participants.values():
        p["in_channels"] = 0
        p["out_channels"] = 0
    for pt in points:
        s = participants[pt["src"]]
        d = participants[pt["dst"]]
        s["in_channels"] = max(s["in_channels"], pt["in_ch"])
        d["out_channels"] = max(d["out_channels"], pt["out_ch"])

    ordered = sorted(participants.values(), key=lambda p: p["order"])
    for p in ordered:
        del p["order"]
    return HUB, ordered, points


def _fmt_gain(g):
    """Canonical gain string: `-8.0`, `-10.0`, `-8.25` — always at least one decimal, no trailing 0s."""
    s = f"{g:.4f}".rstrip("0").rstrip(".")
    if "." not in s:
        s += ".0"
    return s


def render(model):
    """Render (hub, participants, points) into the intercom.toml text (deterministic)."""
    hub, participants, points = model
    out = []
    out.append("# strih-lx intercom hub routing — GENERATED from the VB-Matrix XML (issue 1345 M1).")
    out.append("# DO NOT EDIT BY HAND. Regenerate:")
    out.append(
        "#   python3 scripts/vbmatrix_to_intercom_toml.py "
        "intercom/tests/fixtures/vbmatrix-coconut-today.xml > intercom/intercom.strih-lx.toml"
    )
    out.append("# The byte-for-byte parity is pinned by tests/python/test_vbmatrix_to_intercom_toml_1345.py.")
    out.append("")
    out.append("[hub]")
    out.append(f'bind = "{hub["bind"]}"')
    out.append(f'vban_bind = "{hub["vban_bind"]}"')
    out.append(f'sample_rate = {hub["sample_rate"]}')
    out.append(f'block_frames = {hub["block_frames"]}')
    out.append("")

    # The Janus audiobridge edge (issue 1345 M3a) — the phones participant's plain-RTP PCMU leg. The
    # room secret is NEVER inlined; the hub reads it from `room_secret_file` (0600) at start.
    out.append("[janus]")
    out.append(f'api_url = "{JANUS["api_url"]}"')
    out.append(f'room = {JANUS["room"]}')
    out.append(f'room_secret_file = "{JANUS["room_secret_file"]}"')
    out.append(f'rtp_bind = "{JANUS["rtp_bind"]}"')
    out.append("")

    # The Interkom picture (issue 1345 M3c) — the NDI low-bandwidth → JPEG → /interkom.mjpeg pipe.
    out.append("[video]")
    out.append(f'ndi_source_name = "{VIDEO["ndi_source_name"]}"')
    out.append(f'fps = {VIDEO["fps"]}')
    out.append(f'jpeg_quality = {VIDEO["jpeg_quality"]}')
    out.append(f'enabled = {"true" if VIDEO["enabled"] else "false"}')
    out.append("")

    for p in participants:
        out.append("[[participant]]")
        out.append(f'name = "{p["name"]}"')
        out.append(f'role = "{p["role"]}"')
        out.append(f'adapter = "{p["adapter"]}"')
        if "host" in p:
            out.append(f'host = "{p["host"]}"')
        if "in_stream" in p:
            out.append(f'in_stream = "{p["in_stream"]}"')
        if "out_stream" in p:
            out.append(f'out_stream = "{p["out_stream"]}"')
        out.append(f'in_channels = {p["in_channels"]}')
        out.append(f'out_channels = {p["out_channels"]}')
        out.append("")

    for pt in points:
        out.append("[[point]]")
        out.append(f'src = "{pt["src"]}"')
        out.append(f'in_ch = {pt["in_ch"]}')
        out.append(f'dst = "{pt["dst"]}"')
        out.append(f'out_ch = {pt["out_ch"]}')
        if pt["gain_db"] != 0.0:
            out.append(f"gain_db = {_fmt_gain(pt['gain_db'])}")
        if pt["mute"]:
            out.append("mute = true")
        out.append("")

    return "\n".join(out) + "\n"


def convert(xml_text):
    """XML text → intercom.toml text."""
    return render(build_model(xml_text))


def main(argv):
    if len(argv) != 2:
        sys.stderr.write(f"usage: {argv[0]} <vbmatrix.xml>\n")
        return 2
    with open(argv[1], "r", encoding="utf-8") as fh:
        xml_text = fh.read()
    sys.stdout.write(convert(xml_text))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
