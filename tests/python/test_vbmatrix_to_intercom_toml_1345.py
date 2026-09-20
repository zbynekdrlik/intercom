"""issue 1345 M1 — the VB-Matrix XML → intercom.toml converter.

`scripts/vbmatrix_to_intercom_toml.py` parses the live VB-Audio Matrix settings XML (the strih
Windows hub's static N-1 grid) into the strih-lx intercom hub's declarative `intercom.toml`. The
core assertions:

* PARITY — regenerating from the checked-in fixture XML reproduces the checked-in
  `intercom/intercom.strih-lx.toml` BYTE-FOR-BYTE (so the matrix can never silently drift from what
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
_CHECKED_IN = _ROOT / "intercom" / "intercom.strih-lx.toml"
_CAMS = {f"cam{i}" for i in range(1, 8)}


def _xml():
    return _FIXTURE.read_text()


def _model():
    return conv.build_model(_xml())


def test_parity_regenerates_checked_in_toml_byte_for_byte():
    generated = conv.convert(_xml())
    assert generated == _CHECKED_IN.read_text(), (
        "the converter output drifted from the checked-in intercom.strih-lx.toml — "
        "regenerate: python3 scripts/vbmatrix_to_intercom_toml.py "
        "intercom/tests/fixtures/vbmatrix-coconut-today.xml > intercom/intercom.strih-lx.toml"
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
    assert len(points) == 216
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
        "program_monitor",
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
    assert "secret" not in j, "the room secret must NEVER be inlined in the TOML"


def test_generated_toml_parses_into_the_matrix_shape():
    data = tomllib.loads(conv.convert(_xml()))
    assert data["hub"]["sample_rate"] == 48000
    assert data["hub"]["vban_bind"] == "0.0.0.0:6980"
    _hub, parts, points = _model()
    assert len(data["participant"]) == len(parts)
    assert len(data["point"]) == len(points) == 216
    # A spot-check that a −8 dB program-ref point survives the render→parse round-trip.
    fohabl_cutters = [
        pt
        for pt in data["point"]
        if pt["src"] == "fohabl" and pt.get("in_ch") == 1 and pt["dst"] == "cutters"
    ]
    assert fohabl_cutters and all(pt["gain_db"] == -8.0 for pt in fohabl_cutters)


# --- issue 1344: the local PipeWire program-audio graph (VB-Matrix replacement) -----------------


def test_vasio8_becomes_a_pipewire_program_out_sink():
    """VASIO8 is the OBS `ASIO zvuk` program capture — the converter maps it to a `program_out`
    pipewire sink (not the old M1 `program_monitor`), targeting the `strih-program` null sink."""
    _hub, parts, _points = _model()
    names = {p["name"] for p in parts}
    assert "program_out" in names, "VASIO8 must map to a program_out participant"
    assert "program_monitor" not in names, "VASIO8 is the OBS program capture, not a monitor"
    po = next(p for p in parts if p["name"] == "program_out")
    assert po["role"] == "program_out"
    assert po["adapter"] == "pipewire"
    assert po["pipewire_target"] == "strih-program"


def test_program_out_source_streams_are_the_vasio8_feeds():
    """The program mix OBS captures = the VBAN streams routed into VASIO8 = fohabl-strih + lv1-strih
    (the VBAN64 slot = VBANStreamIn index 9 = lv1-strih), both live named sources."""
    _hub, parts, _points = _model()
    po = next(p for p in parts if p["name"] == "program_out")
    assert po["source_streams"] == ["fohabl-strih", "lv1-strih"], po["source_streams"]


def test_cutters_becomes_a_pipewire_talkback_capture():
    """The MiniFuse (AMDevice type 256) carries the operator TALKBACK mic — the converter maps the
    cutters participant to a pipewire capture input, not the old adapter=none."""
    _hub, parts, _points = _model()
    cutters = next(p for p in parts if p["name"] == "cutters")
    assert cutters["adapter"] == "pipewire"
    assert cutters.get("pipewire_source"), "cutters needs a pipewire_source (the MiniFuse capture node)"
    assert "MiniFuse" in cutters["pipewire_source"]


def test_generated_toml_emits_the_pipewire_fields():
    text = conv.convert(_xml())
    assert 'adapter = "pipewire"' in text
    assert 'pipewire_target = "strih-program"' in text
    assert 'source_streams = ["fohabl-strih", "lv1-strih"]' in text
    assert "pipewire_source = " in text
    # No stale program_monitor participant survives.
    data = tomllib.loads(text)
    assert not any(p["name"] == "program_monitor" for p in data["participant"])
