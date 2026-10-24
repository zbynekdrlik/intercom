#!/usr/bin/env python3
"""Convert a VB-Audio Matrix settings XML into the strih-snv intercom hub's intercom.toml (issue 1345).

The Windows strih runs VB-Audio Matrix as a static N-1 intercom: a 216-cell routing grid over the
7 cambox VBAN streams, the 2 cutter mics + 2 stereo cans on the MiniFuse (ASIO32), the phones
(VAIO1), the line-3/4 mic (WIN1.IN), the local speakers (WIN1.OUT), an OBS program-monitor sink
(VASIO8) and the attenuated program references (fohabl/lv1). The strih-snv Linux hub replaces that
GUI matrix with a DECLARATIVE `intercom.toml` GENERATED from this XML by THIS converter, so the
routing (incl. the −8/−10 dB program refs + the small asymmetries) is reproduced with fidelity and
lives in git, never a GUI.

Slot → hub participant mapping (derived from the XML's own attributes, not a hard-coded table):

- VBAN slots (doc order = VBANStreamIn index): a slot with an ACTIVE VBANStreamOut (`status=1`) is a
  `cambox` (cam1..cam7, VBAN in+out to `camN.lan`); one without is a `program_ref` (fohabl/lv1/mbc,
  VBAN in only — the stream name minus the `-strih` suffix).
- `AMDevice` type 256 (the MiniFuse ASIO master) → `cutters`; type 4 (WDM out) → `speakers`; type 1
  (WDM in) → `line34`.
- an online `VAIOSlot` (VAIO1) → `phones`.
- an online `VASIOSlot` (VASIO8, the Windows OBS `ASIO zvuk` program capture) → NOTHING: issue 1439
  phase 2 (owner ROZHODNUTE 22.10.2026, "interkom-hub ma byt separatna vec, nema nic preposielat do
  strih obs") -- the strih OBS receives the console's VBAN program itself (the obs-vban source on UDP
  6981), so the hub carries no `program_out` and every point into VASIO8 is dropped (issue 1344's
  `program_out` pipewire sink is gone from this matrix; the hub still supports one for strih-pp).

Adapters: VBAN legs are live; the `phones` participant is `janus` (M3a); the `cutters` (MiniFuse)
open the card directly through ALSA (`alsa`, issue 1438: arecord for the operator talkback mic,
aplay for their own N-1 bus to the cans, on `hw:CARD=M4,DEV=0`, no PipeWire node; issue 1344 had
them on two `pw-cat` children). The remaining interface participants (speakers/line34) stay
`adapter = "none"` until a later local-monitor lane wires them.

Rollback of issue 1438 (by hand on the box, never here): the cutters back on `adapter = "pipewire"`
with `pipewire_source = "alsa_input.usb-ARTURIA_MiniFuse_4-00.pro-input-0"`, `pipewire_target =
"alsa_output.usb-ARTURIA_MiniFuse_4-00.pro-output-0"` and `pipewire_channel_map =
"AUX0,AUX1,AUX2,AUX3"` (the hub keeps the pipewire adapter).

Usage:
    python3 scripts/vbmatrix_to_intercom_toml.py <vbmatrix.xml> > intercom/intercom.strih-snv.toml
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
# participant is carried over the Janus audiobridge room as a plain-RTP participant. The room
# SECRET is never inlined here — the hub reads it from the 0600 `room_secret_file` at start.
# `codec` = "opus" (issue 1345, 25.9.2026): Opus 48 kHz mono, 20 ms, in-band FEC, sent on the hub's
# own 20 ms clock; "pcmu" (G.711 µ-law) stays selectable.
JANUS = {
    "api_url": "http://127.0.0.1:8088/janus",
    "room": 1000,
    "room_secret_file": "/etc/intercom-hub/janus-room.secret",
    "rtp_bind": "0.0.0.0:6990",
    "codec": "opus",
}

# The Interkom picture (MJPEG) config emitted into the `[video]` table (issue 1345 M3c). The hub
# receives the `STRIH-SNV (interkom)` NDI source in LOW-bandwidth mode, decimates to `fps`, JPEG-encodes
# at `jpeg_quality`, and serves it as `multipart/x-mixed-replace` at `/interkom.mjpeg`. Until issue
# 1347 builds the `STRIH-SNV (interkom)` NDI output, the supervisor points `ndi_source_name` at
# `CAM1 (usb)` (a config edit on the box, no code change / no re-generate).
VIDEO = {
    "ndi_source_name": "STRIH-SNV (interkom)",
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
}

_STRIH_SUFFIX = "-strih"

# issue 1438: the cutters' card, opened directly through ALSA by the hub (no PipeWire node). Read live
# on strih-snv 23.10.2026 (`/proc/asound/cards`, `/proc/asound/M4/stream0`): the MiniFuse 4 is card ID
# `M4`, device 0; its hw PCM opens with exactly 6 channels on both sides (playback S32_LE or S16_LE,
# capture S32_LE only -- the hub picks the format from the card itself). The card ID, never the card
# index, which moves when USB re-enumerates.
_MINIFUSE_ALSA_CARD = "M4"
_MINIFUSE_ALSA_DEVICE = 0
_MINIFUSE_HW_CHANNELS = 6
# One hub block (256 frames, 5.33 ms) per ALSA period, like the hub's HDMI leg.
_MINIFUSE_ALSA_PERIOD_FRAMES = 256

# issue 1345: the talkback makeup gain, in dB, on every cutters -> phones and cutters -> camN point.
# ONE constant, added to the VB-Matrix point gain. Never hand-edit the generated TOML to tune it.
# Owner ruling 24.9.2026 ("nemal si menit hlasitosti na kamerach"): 0 dB -- the +12 dB tried that
# day put noise into the cameramen's headsets; the talkback level is set on the MiniFuse preamp.
TALKBACK_MAKEUP_DB = 0.0

# issue 1461: every ingress resampler leg's FIXED target depth in ms (the hub rounds it up to whole
# 256-frame blocks and reads it at start only, never adapting it): a cambox 3 blocks (16 ms), a
# program feed 7 blocks (37.33 ms: the FOH sender's largest measured gap 27.6 ms + a block + half
# a burst), the cutters' card capture 8 blocks (42.67 ms: two of the card's 1024-frame bursts).
TARGET_MS_CAMBOX = "16.0"
TARGET_MS_PROGRAM = "37.33"
TARGET_MS_CARD = "42.67"

_TALKBACK_DST_ROLES = ("cambox", "phones")


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
            # The MiniFuse ASIO master carries the operator TALKBACK mic and the cans: the hub opens
            # the card itself through ALSA (issue 1438; its maps are filled in below, from the
            # channels the points use).
            add("cutters", "cutters", "alsa", 0)
            participants["cutters"]["alsa_card"] = _MINIFUSE_ALSA_CARD
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

    # VASIO8 (the Windows OBS `ASIO zvuk` program capture) -- the first online VASIOSlot -- is NOT
    # carried into the hub (issue 1439 phase 2): the strih OBS takes the console's VBAN program itself,
    # so its points are dropped below (`None` = a known slot with no hub participant).
    for e in root.iter("VASIOSlot"):
        if e.get("online") == "1":
            slot_to_part[e.get("uniq")] = None
            break

    # Points (routing grid), in document order.
    points = []
    for e in root.iter("Point"):
        si, so = e.get("slotin"), e.get("slotout")
        if slot_to_part.get(si, "") is None or slot_to_part.get(so, "") is None:
            continue  # a point into / out of VASIO8 (issue 1439 phase 2)
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

    # issue 1345: the talkback makeup gain on every cutters -> phones / camN point (added to the
    # VB-Matrix gain, which is unity for those points on the fixture).
    for pt in points:
        if pt["src"] == "cutters" and participants[pt["dst"]]["role"] in _TALKBACK_DST_ROLES:
            pt["gain_db"] = pt["gain_db"] + TALKBACK_MAKEUP_DB

    # Channel counts = the max channel index each participant is routed on.
    for p in participants.values():
        p["in_channels"] = 0
        p["out_channels"] = 0
    for pt in points:
        s = participants[pt["src"]]
        d = participants[pt["dst"]]
        s["in_channels"] = max(s["in_channels"], pt["in_ch"])
        d["out_channels"] = max(d["out_channels"], pt["out_ch"])

    # issue 1461: the fixed target of every ingress resampler leg.
    for p in participants.values():
        if p["role"] == "cambox":
            p["target_ms"] = TARGET_MS_CAMBOX
        elif p["adapter"] == "vban":
            p["target_ms"] = TARGET_MS_PROGRAM
        elif p["name"] == "cutters" and p["in_channels"] > 0:
            p["target_ms"] = TARGET_MS_CARD

    # issue 1438: the cutters' card leg. The mics are the MiniFuse's first hw inputs, the cans its
    # first hw outputs (issue 1345: the operator hears the N-1 mix in the headphones), on the card's
    # 6 channels each way.
    cutters = participants.get("cutters")
    if cutters is not None and cutters["adapter"] == "alsa":
        cutters["alsa_device"] = _MINIFUSE_ALSA_DEVICE
        cutters["alsa_period_frames"] = _MINIFUSE_ALSA_PERIOD_FRAMES
        if cutters["out_channels"] > 0:
            cutters["alsa_out_channels"] = _MINIFUSE_HW_CHANNELS
            cutters["alsa_channel_map"] = ",".join(
                str(i + 1) for i in range(cutters["out_channels"])
            )
        if cutters["in_channels"] > 0:
            cutters["alsa_in_channels"] = _MINIFUSE_HW_CHANNELS
            cutters["alsa_in_channel_map"] = ",".join(
                str(i + 1) for i in range(cutters["in_channels"])
            )

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
    out.append("# strih-snv intercom hub routing — GENERATED from the VB-Matrix XML (issue 1345 M1).")
    out.append("# DO NOT EDIT BY HAND. Regenerate:")
    out.append(
        "#   python3 scripts/vbmatrix_to_intercom_toml.py "
        "intercom/tests/fixtures/vbmatrix-coconut-today.xml > intercom/intercom.strih-snv.toml"
    )
    out.append("# The byte-for-byte parity is pinned by tests/python/test_vbmatrix_to_intercom_toml_1345.py.")
    out.append("")
    out.append("[hub]")
    out.append(f'bind = "{hub["bind"]}"')
    out.append(f'vban_bind = "{hub["vban_bind"]}"')
    out.append(f'sample_rate = {hub["sample_rate"]}')
    out.append(f'block_frames = {hub["block_frames"]}')
    out.append("")

    # The Janus audiobridge edge (issue 1345 M3a) — the phones participant's plain-RTP leg. The
    # room secret is NEVER inlined; the hub reads it from `room_secret_file` (0600) at start.
    out.append("[janus]")
    out.append(f'api_url = "{JANUS["api_url"]}"')
    out.append(f'room = {JANUS["room"]}')
    out.append(f'room_secret_file = "{JANUS["room_secret_file"]}"')
    out.append(f'rtp_bind = "{JANUS["rtp_bind"]}"')
    out.append(f'codec = "{JANUS["codec"]}"')
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
        if "pipewire_target" in p:
            out.append(f'pipewire_target = "{p["pipewire_target"]}"')
        if "pipewire_source" in p:
            out.append(f'pipewire_source = "{p["pipewire_source"]}"')
        if "pipewire_channel_map" in p:
            out.append(f'pipewire_channel_map = "{p["pipewire_channel_map"]}"')
        if "alsa_card" in p:
            out.append(f'alsa_card = "{p["alsa_card"]}"')
        for key in ("alsa_device", "alsa_period_frames", "alsa_out_channels", "alsa_in_channels"):
            if key in p:
                out.append(f"{key} = {p[key]}")
        for key in ("alsa_channel_map", "alsa_in_channel_map"):
            if key in p:
                out.append(f'{key} = "{p[key]}"')
        if "source_streams" in p:
            joined = ", ".join(f'"{s}"' for s in p["source_streams"])
            out.append(f"source_streams = [{joined}]")
        if "target_ms" in p:
            out.append(f'target_ms = {p["target_ms"]}')
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
