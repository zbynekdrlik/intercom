"""issue 1345 M1 — the VB-Matrix XML → intercom.toml converter.

`scripts/vbmatrix_to_intercom_toml.py` parses the live VB-Audio Matrix settings XML (the strih
Windows hub's static N-1 grid) into the strih-snv intercom hub's declarative `intercom.toml`. The
core assertions:

* PARITY — regenerating from the checked-in fixture XML reproduces the checked-in
  `intercom/intercom.strih-snv.toml` BYTE-FOR-BYTE (so the matrix can never silently drift from what
  the converter produces).
* the 10 VBAN inputs + 7 active VBAN outputs are all mapped;
* the mix-minus (N-1) invariant holds structurally — no `src == dst` point (a cambox never hears
  itself);
* the program references keep their fidelity — fohabl in1 → the cutters at −8 dB and NEVER to a
  cambox out.

Pure-Python, fixture-driven RED→GREEN (Tier-0 #557: no cargo). The pure decision core is imported
directly (the #1199/#1203 python-mirror precedent).
"""

import pathlib
import sys

import tomllib

_ROOT = pathlib.Path(__file__).resolve().parents[2]
_SCRIPTS = _ROOT / "scripts"
if str(_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS))

import vbmatrix_to_intercom_toml as conv  # noqa: E402

_FIXTURE = _ROOT / "intercom" / "tests" / "fixtures" / "vbmatrix-coconut-today.xml"
_CHECKED_IN = _ROOT / "intercom" / "intercom.strih-snv.toml"
_CAMS = {f"cam{i}" for i in range(1, 8)}


def _xml():
    return _FIXTURE.read_text()


def _model():
    return conv.build_model(_xml())


def test_parity_regenerates_checked_in_toml_byte_for_byte():
    generated = conv.convert(_xml())
    assert generated == _CHECKED_IN.read_text(), (
        "the converter output drifted from the checked-in intercom.strih-snv.toml — "
        "regenerate: python3 scripts/vbmatrix_to_intercom_toml.py "
        "intercom/tests/fixtures/vbmatrix-coconut-today.xml > intercom/intercom.strih-snv.toml"
    )


def test_ten_vban_inputs_seven_vban_outputs():
    _hub, parts, _points = _model()
    vin = [p for p in parts if p["adapter"] == "vban" and p.get("in_stream")]
    vout = [p for p in parts if p["adapter"] == "vban" and p.get("out_stream")]
    assert len(vin) == 10, [p["name"] for p in vin]
    assert len(vout) == 7, [p["name"] for p in vout]
    assert {p["name"] for p in vout} == _CAMS


def test_no_self_route_and_cam1_never_hears_itself():
    _hub, _parts, points = _model()
    assert not any(pt["src"] == "cam1" and pt["dst"] == "cam1" for pt in points)
    assert not any(pt["src"] == pt["dst"] for pt in points), "the N-1 invariant forbids self-routes"


def test_fohabl_program_ref_into_cutters_at_minus_8_never_to_cams():
    _hub, _parts, points = _model()
    to_cutters = [
        pt for pt in points if pt["src"] == "fohabl" and pt["in_ch"] == 1 and pt["dst"] == "cutters"
    ]
    assert to_cutters, "fohabl in1 must route into the cutters"
    assert all(pt["gain_db"] == -8.0 for pt in to_cutters), to_cutters
    assert not any(
        pt["src"] == "fohabl" and pt["dst"] in _CAMS for pt in points
    ), "program references must NEVER reach a cambox output"


def test_model_shape_and_roles():
    _hub, parts, points = _model()
    # issue 1439 phase 2: the 216-point grid minus VASIO8's 4 program points (fohabl/lv1 ch1/2): the hub
    # forwards nothing into the strih OBS, which takes the console's VBAN itself
    assert len(points) == 212
    names = {p["name"] for p in parts}
    for want in (
        "cam1",
        "cam7",
        "fohabl",
        "lv1",
        "mbc",
        "cutters",
        "phones",
        "speakers",
        "line34",
    ):
        assert want in names, f"missing participant {want}"
    # The cutters carry 2 mics in + two stereo cans out (4 ch); phones are 2-in / 2-out.
    by_name = {p["name"]: p for p in parts}
    assert by_name["cutters"]["out_channels"] == 4
    assert by_name["cutters"]["in_channels"] == 2
    assert by_name["cam1"]["out_channels"] == 2


def test_phones_is_a_janus_participant():
    # issue 1345 M3a: the phones participant is carried over the Janus audiobridge (adapter `janus`),
    # replacing VDO.Ninja; it is the ONLY janus participant, and only on the phones role.
    _hub, parts, _points = _model()
    by_name = {p["name"]: p for p in parts}
    assert by_name["phones"]["adapter"] == "janus"
    janus = [p for p in parts if p["adapter"] == "janus"]
    assert [p["name"] for p in janus] == ["phones"], janus
    # The phones leg keeps its 2-in / 2-out shape (the adapter up/down-mixes mono<->stereo).
    assert by_name["phones"]["in_channels"] == 2
    assert by_name["phones"]["out_channels"] == 2


def test_janus_table_emitted_with_defaults_and_no_inlined_secret():
    # issue 1345 M3a: the converter emits the [janus] table with defaults; the room SECRET is never
    # inlined (the hub reads it from the 0600 room_secret_file at start).
    data = tomllib.loads(conv.convert(_xml()))
    j = data["janus"]
    assert j["api_url"] == "http://127.0.0.1:8088/janus"
    assert j["room"] == 1000
    assert j["room_secret_file"] == "/etc/intercom-hub/janus-room.secret"
    assert j["rtp_bind"] == "0.0.0.0:6990"
    # issue 1345 (25.9.2026): the phones leg is Opus 48 kHz with in-band FEC (PCMU stays selectable).
    assert j["codec"] == "opus"
    assert "secret" not in j, "the room secret must NEVER be inlined in the TOML"


def test_generated_toml_parses_into_the_matrix_shape():
    data = tomllib.loads(conv.convert(_xml()))
    assert data["hub"]["sample_rate"] == 48000
    assert data["hub"]["vban_bind"] == "0.0.0.0:6980"
    _hub, parts, points = _model()
    assert len(data["participant"]) == len(parts)
    assert len(data["point"]) == len(points) == 212
    # A spot-check that a −8 dB program-ref point survives the render→parse round-trip.
    fohabl_cutters = [
        pt
        for pt in data["point"]
        if pt["src"] == "fohabl" and pt.get("in_ch") == 1 and pt["dst"] == "cutters"
    ]
    assert fohabl_cutters and all(pt["gain_db"] == -8.0 for pt in fohabl_cutters)


# --- issue 1344: the local PipeWire program-audio graph (VB-Matrix replacement) -----------------


def test_vasio8_is_not_carried_into_the_hub():
    """issue 1439 phase 2 (owner ROZHODNUTE 22.10.2026, addendum 6338457001: "interkom-hub ma byt separatna
    vec, nema nic preposielat do strih obs"): VASIO8 was the OBS `ASIO zvuk` program capture. The strih OBS
    now receives the console's VBAN itself (the obs-vban source on UDP 6981), so the converter emits no
    `program_out` participant and drops every point into VASIO8."""
    _hub, parts, points = _model()
    names = {p["name"] for p in parts}
    assert "program_out" not in names and "program_monitor" not in names
    assert not any(p["role"] == "program_out" for p in parts)
    assert not any(pt["dst"] == "program_out" for pt in points)
    # the program references keep their channel counts from their remaining (cutters / speakers) points
    by_name = {p["name"]: p for p in parts}
    assert (by_name["fohabl"]["in_channels"], by_name["lv1"]["in_channels"]) == (4, 2)


def test_cutters_becomes_a_pipewire_talkback_capture():
    """The MiniFuse (AMDevice type 256) carries the operator TALKBACK mic — the converter maps the
    cutters participant to a pipewire capture input, not the old adapter=none."""
    _hub, parts, _points = _model()
    cutters = next(p for p in parts if p["name"] == "cutters")
    assert cutters["adapter"] == "pipewire"
    assert cutters.get("pipewire_source"), "cutters needs a pipewire_source (the MiniFuse capture node)"
    assert "MiniFuse" in cutters["pipewire_source"]


def test_minifuse_capture_node_is_the_real_uppercase_arturia_alsa_name():
    # issue 1344 follow-up (20.9.2026 live diagnosis, issuecomment-5751113173): the real ALSA node
    # name on the box is uppercase "ARTURIA" (`wpctl status` / `pactl list sources`), NOT the
    # title-case "Arturia" the converter used to emit — a case-sensitive PipeWire node-name mismatch
    # the hub reads as a missing device.
    _hub, parts, _points = _model()
    cutters = next(p for p in parts if p["name"] == "cutters")
    assert cutters["pipewire_source"] == "alsa_input.usb-ARTURIA_MiniFuse_4-00.pro-input-0",         cutters["pipewire_source"]


def test_generated_toml_emits_the_pipewire_fields():
    text = conv.convert(_xml())
    assert 'adapter = "pipewire"' in text
    # issue 1439 phase 2: no strih-program sink and no program source streams (no program_out)
    assert 'pipewire_target = "strih-program"' not in text
    assert "source_streams" not in text
    assert "pipewire_source = " in text
    # No stale program_monitor participant survives.
    data = tomllib.loads(text)
    assert not any(p["name"] == "program_monitor" for p in data["participant"])


# --- 24.9.2026 production fix: the operator's headphones + the talkback level -------------------

_MINIFUSE_PLAYBACK = "alsa_output.usb-ARTURIA_MiniFuse_4-00.pro-output-0"


def test_cutters_play_their_n1_mix_to_the_minifuse_outputs():
    """On Linux the cutters participant was capture-only, so the operator heard NOTHING. The converter
    now gives it the MiniFuse playback node + an explicit AUX channel map for its 4 output channels
    (the pro-audio node's ports are AUX0..AUX5; an unmapped 4-ch stream defaults to FL/FR/RL/RR)."""
    _hub, parts, _points = _model()
    cutters = next(p for p in parts if p["name"] == "cutters")
    assert cutters["pipewire_source"] == "alsa_input.usb-ARTURIA_MiniFuse_4-00.pro-input-0"
    assert cutters["pipewire_target"] == _MINIFUSE_PLAYBACK
    assert cutters["out_channels"] == 4
    assert cutters["pipewire_channel_map"] == "AUX0,AUX1,AUX2,AUX3"
    text = conv.convert(_xml())
    assert f'pipewire_target = "{_MINIFUSE_PLAYBACK}"' in text
    assert 'pipewire_channel_map = "AUX0,AUX1,AUX2,AUX3"' in text


def test_talkback_makeup_gain_is_one_named_constant_on_cutters_to_phones_and_cams():
    """Every cutters -> phones / cutters -> camN point carries the ONE converter constant (never a
    hand-edited TOML); every other point keeps its VB-Matrix gain. Owner ruling 24.9.2026 ("nemal si
    menit hlasitosti na kamerach"): the makeup is 0 dB -- the talkback level is set on the MiniFuse
    preamp, never by the hub."""
    assert conv.TALKBACK_MAKEUP_DB == 0.0
    _hub, _parts, points = _model()
    talkback = [
        pt for pt in points if pt["src"] == "cutters" and (pt["dst"] == "phones" or pt["dst"] in _CAMS)
    ]
    assert talkback, "the cutters route to the phones and the cams"
    assert {pt["dst"] for pt in talkback} == _CAMS | {"phones"}
    assert all(pt["gain_db"] == conv.TALKBACK_MAKEUP_DB for pt in talkback), talkback
    # Nothing else moved: the program refs keep -8/-10 dB, the cam -> cutters points stay unity.
    others = [pt for pt in points if pt not in talkback]
    assert all(pt["gain_db"] in (0.0, -8.0, -10.0) for pt in others)
    text = conv.convert(_xml())
    assert "gain_db = 12.0" not in text
