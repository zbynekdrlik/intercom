"""issue 1457 (owner 21.10.2026): every PP intercom level at 0 dB.

"preco to je take komplikovane ze vsade milion inych hlasitosti ked vsade by to malo byt na 0dB ... sluchatka
v camboxoch pocuju dobre nahlas strihaca ale pripojeny cez web interkom-pp.newlevel.media pocuju potichu
strihaca", then "tak nie ma to byt na 0db a ja pridam strihaca na zvukovke!!!".

Read live the same day: intercom.strih-pp.toml carried cutters -> cameras +6 dB, cutters -> phones 0 dB and
lv1 -> cutters -8 dB, and the hub's PipeWire node for the cutters' ears (the Yamaha MG-XU output) sat at
volume 0.40. Now:
  * every point of the PP routing files is unity (no gain_db other than 0);
  * setup-strih step 13 sets every PipeWire node the hub plays to or records from (each pipewire_target /
    pipewire_source of the box's routing TOML) to volume 1.0, unmuted, in the operator's session;
  * verify-strih item 9 grades those nodes (FAIL off unity or muted, NOTE when the node or the session is
    not there).
The loudness is set once at the source, on the cutter's sound card.
"""

import json
import os
import subprocess
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent
LIB = REPO / "scripts" / "lib" / "strih-provision.sh"
SETUP = REPO / "scripts" / "setup-strih.sh"
VERIFY = REPO / "scripts" / "verify-strih.sh"
PP_TOMLS = [REPO / "intercom" / "intercom.strih-pp.toml", REPO / "intercom" / "intercom.stream-pp.toml"]
ME = os.environ.get("USER") or "nobody"

YAMAHA_OUT = "alsa_output.usb-Yamaha_Corporation_MG-XU-00.analog-stereo"
YAMAHA_IN = "alsa_input.usb-Yamaha_Corporation_MG-XU-00.analog-stereo"

_FAKE_WPCTL = r'''#!/bin/bash
echo "wpctl $*" >> "$FAKE_PW/calls"
case "$1" in
  get-volume) cat "$FAKE_PW/vol-$2" 2>/dev/null || { echo "Translate ID error: $2" >&2; exit 1; } ;;
  set-volume) [ -e "$FAKE_PW/set-fails" ] && exit 1
              m=""; grep -q MUTED "$FAKE_PW/vol-$2" 2>/dev/null && m=" [MUTED]"
              printf 'Volume: %.2f%s\n' "$3" "$m" > "$FAKE_PW/vol-$2" ;;
  set-mute) [ "$3" = 0 ] && sed -i 's/ \[MUTED\]//' "$FAKE_PW/vol-$2" ;;
esac
'''
_FAKE_PWDUMP = '#!/bin/bash\n[ -e "$FAKE_PW/pwdump-fails" ] && exit 1\ncat "$FAKE_PW/pwdump.json"\n'


def _node(nid, name):
    return {"id": nid, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": name}}}


class Pw:
    """A fake operator PipeWire session: pw-dump lists nodes, wpctl reads and writes their volume."""

    def __init__(self, tmp_path):
        self.dir = tmp_path / "pw"
        self.dir.mkdir()
        self.bin = tmp_path / "pwbin"
        self.bin.mkdir()
        for name, text in (("wpctl", _FAKE_WPCTL), ("pw-dump", _FAKE_PWDUMP)):
            (self.bin / name).write_text(text)
            (self.bin / name).chmod(0o755)
        self.toml = REPO / "intercom" / "intercom.strih-pp.toml"

    def nodes(self, **vols):
        """nodes(out='Volume: 0.40', ...) -> pw-dump lists each given node; absent keys are not listed."""
        names = {"out": (61, YAMAHA_OUT), "inp": (62, YAMAHA_IN), "prog": (63, "strih-program")}
        objs = [{"id": 1, "type": "PipeWire:Interface:Core", "info": {}}]
        for key, text in vols.items():
            nid, name = names[key]
            objs.append(_node(nid, name))
            (self.dir / ("vol-%d" % nid)).write_text(text + "\n")
        (self.dir / "pwdump.json").write_text(json.dumps(objs))

    def vol(self, nid):
        return (self.dir / ("vol-%d" % nid)).read_text().strip()

    def calls(self):
        p = self.dir / "calls"
        return p.read_text().splitlines() if p.exists() else []

    def run(self, body):
        script = ('set -euo pipefail\n. "$1"\n'
                  'ok() { echo "PASS $1"; }\nbad() { echo "FAIL $1"; }\nnote() { echo "NOTE $1"; }\n'
                  + body + '\necho END')
        env = dict(os.environ, FAKE_PW=str(self.dir), STRIH_HUB_PW_EUID="1000",
                   PATH=str(self.bin) + os.pathsep + os.environ["PATH"])
        r = subprocess.run(["bash", "-c", script, "harness", str(LIB), ME, str(self.toml)],
                           capture_output=True, text=True, env=env, timeout=60)
        assert r.returncode == 0, r.stdout + r.stderr
        lines = r.stdout.strip().splitlines()
        assert lines and lines[-1] == "END", r.stdout + r.stderr
        return lines[:-1]


# --- the routing files ---------------------------------------------------------------------------------

def test_every_pp_hub_point_is_unity():
    for f in PP_TOMLS:
        m = tomllib.loads(f.read_text())
        off = [(p["src"], p["dst"], p["out_ch"], p["gain_db"]) for p in m["point"] if p.get("gain_db", 0.0) != 0.0]
        assert not off, "%s: %s" % (f.name, off)


def test_the_cutter_reaches_the_cameras_and_the_phones_at_the_same_level():
    m = tomllib.loads(PP_TOMLS[0].read_text())
    gains = {(p["dst"], p.get("gain_db", 0.0)) for p in m["point"] if p["src"] == "cutters"}
    assert {g for _, g in gains} == {0.0}, gains
    assert {"cam1", "cam4", "cam6", "cam7", "phones"} <= {d for d, _ in gains}


# --- the hub's PipeWire nodes: the names --------------------------------------------------------------

def test_the_hub_nodes_are_every_pipewire_target_and_source(tmp_path):
    pw = Pw(tmp_path)
    out = pw.run('strih_intercom_hub_pw_nodes "$3"')
    assert out == sorted([YAMAHA_IN, YAMAHA_OUT, "strih-program"]), out


# --- setup: every node to 1.0, unmuted --------------------------------------------------------------------

def test_setup_sets_an_off_level_node_to_unity_and_leaves_a_unity_one_alone(tmp_path):
    """The 21.10.2026 strih-pp state: the cutters' ears at 0.40, the rest at 1.00."""
    pw = Pw(tmp_path)
    pw.nodes(out="Volume: 0.40", inp="Volume: 1.00", prog="Volume: 1.00")
    out = pw.run('strih_intercom_hub_pw_unity_apply "$2" "$3"')
    assert "wpctl set-volume 61 1.0" in pw.calls(), pw.calls()
    assert not [c for c in pw.calls() if c.startswith("wpctl set-volume 62") or c.startswith("wpctl set-volume 63")]
    assert pw.vol(61) == "Volume: 1.00"
    assert any(YAMAHA_OUT in ln and "0.40" in ln for ln in out), out


def test_setup_unmutes_a_muted_node(tmp_path):
    pw = Pw(tmp_path)
    pw.nodes(out="Volume: 1.00 [MUTED]", inp="Volume: 1.00", prog="Volume: 1.00")
    pw.run('strih_intercom_hub_pw_unity_apply "$2" "$3"')
    assert "wpctl set-mute 61 0" in pw.calls(), pw.calls()
    assert pw.vol(61) == "Volume: 1.00"


def test_setup_names_a_node_that_is_not_there_and_goes_on(tmp_path):
    pw = Pw(tmp_path)
    pw.nodes(out="Volume: 0.40", prog="Volume: 1.00")  # the Yamaha input not listed
    out = pw.run('strih_intercom_hub_pw_unity_apply "$2" "$3"')
    assert any(YAMAHA_IN in ln and "not" in ln for ln in out), out
    assert pw.vol(61) == "Volume: 1.00"


def test_setup_without_the_operator_session_warns_and_returns_0(tmp_path):
    pw = Pw(tmp_path)
    pw.nodes(out="Volume: 0.40")
    (pw.dir / "pwdump-fails").write_text("")
    out = pw.run('strih_intercom_hub_pw_unity_apply "$2" "$3"')
    assert any("WARN" in ln for ln in out), out
    assert not [c for c in pw.calls() if c.startswith("wpctl set-")]


def test_a_failed_set_is_a_warning(tmp_path):
    pw = Pw(tmp_path)
    pw.nodes(out="Volume: 0.40", inp="Volume: 1.00", prog="Volume: 1.00")
    (pw.dir / "set-fails").write_text("")
    out = pw.run('strih_intercom_hub_pw_unity_apply "$2" "$3"')
    assert any("WARN" in ln and YAMAHA_OUT in ln for ln in out), out


# --- verify: each node graded ----------------------------------------------------------------------------

def test_verify_passes_unity_nodes(tmp_path):
    pw = Pw(tmp_path)
    pw.nodes(out="Volume: 1.00", inp="Volume: 1.00", prog="Volume: 1.00")
    rows = pw.run('strih_intercom_hub_pw_unity_grade "$2" "$3"')
    assert len(rows) == 3 and all(r.startswith("PASS (hub-level)") for r in rows), rows


def test_verify_fails_a_node_off_unity_or_muted(tmp_path):
    pw = Pw(tmp_path)
    pw.nodes(out="Volume: 0.40", inp="Volume: 1.00 [MUTED]", prog="Volume: 1.00")
    rows = pw.run('strih_intercom_hub_pw_unity_grade "$2" "$3"')
    fails = [r for r in rows if r.startswith("FAIL")]
    assert len(fails) == 2, rows
    assert any(YAMAHA_OUT in r and "0.40" in r and "step 13" in r for r in fails), rows
    assert any(YAMAHA_IN in r and "muted" in r for r in fails), rows


def test_verify_notes_an_absent_node_and_an_unreadable_session(tmp_path):
    pw = Pw(tmp_path)
    pw.nodes(out="Volume: 1.00", prog="Volume: 1.00")
    rows = pw.run('strih_intercom_hub_pw_unity_grade "$2" "$3"')
    assert [r for r in rows if r.startswith("NOTE") and YAMAHA_IN in r], rows
    (pw.dir / "pwdump-fails").write_text("")
    rows = pw.run('strih_intercom_hub_pw_unity_grade "$2" "$3"')
    assert len(rows) == 1 and rows[0].startswith("NOTE (hub-level)"), rows


# --- the wiring ---------------------------------------------------------------------------------------

def test_setup_and_verify_call_it_with_the_deployed_routing_file():
    s = SETUP.read_text()
    v = VERIFY.read_text()
    assert len(s.splitlines()) <= 1000 and len(v.splitlines()) <= 1000
    install = s.index('install -Dm644 "${HERE}/../$(strih_lx_intercom_config)" /etc/intercom-hub/intercom.toml')
    apply = s.index('strih_intercom_hub_pw_unity_apply "$DESKTOP_USER" /etc/intercom-hub/intercom.toml')
    assert s.index("step 13 ") < install < apply < s.index("step 14 ")
    grade = v.index('strih_intercom_hub_pw_unity_grade "${STRIH_LX_USER:-newlevel}" /etc/intercom-hub/intercom.toml')
    assert v.index("strih_intercom_hub_hdmi_leg_grade") < grade
