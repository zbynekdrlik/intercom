"""issue 1453 (design issuecomment-6250479059, Approach 1): the web intercom works for a phone on MOBILE
DATA, served from the strih box itself.

Owner 18.10.2026: the cameramen on mobile data could not reach `interkom-pp.newlevel.media` (a private
venue address), and the answer was "2" = reachable from mobile data, straight from the strih notebook,
no dev1. Pinned here (slices 1 + 2, the box side; the hub's TURN endpoints are pinned in
intercom/hub/tests/turn_ice_1453.rs, the PWA in intercom/tests/e2e/ice.spec.js):

  * the box fact STRIH_HTTPS_PUBLIC (yes on strih-snv + strih-pp, no on the stream role; `yes` needs an
    intercom site);
  * scripts/janus_turn_rest_cfg.py: the managed TURN REST block in janus.jcfg's nat section -- only the
    block is ever written or removed, a hand-set key is a conflict, the write keeps the file's mode;
  * setup-strih step 16i (`strih_interkom_public_install`): cloudflared from Cloudflare's repo with the
    signing key checked against its sha256 pin, the token written from CLOUDFLARE_TUNNEL_TOKEN (never
    printed, never in an argv), the unit written only on a difference and started only with a token, a
    missing token / TURN key = ONE WARN each, the hub drop-in + the Janus block only with a TURN key, a
    running hub never restarted;
  * verify-strih item 45's two new rows (`strih_interkom_public_grade_rows`);
  * the cross-language pins: the hub's TURN credential name + default path + route = the lib's.

Every bash run sources the libs under the caller's `set -euo pipefail`; values reach bash as ARGUMENTS,
never inside the script text. Fake apt-get / dpkg-query / systemctl / curl on PATH; the real python3 and
sha256sum. No root, no rig, no network.
"""
import hashlib
import importlib.util
import os
import re
import stat
import subprocess
import sys
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parent.parent.parent
SCRIPTS = REPO / "scripts"
LIB = SCRIPTS / "lib" / "strih-interkom-public.sh"
FACTS = SCRIPTS / "lib" / "strih-box-facts.sh"
PROVISION = SCRIPTS / "lib" / "strih-provision.sh"
SETUP = SCRIPTS / "setup-strih.sh"
VERIFY = SCRIPTS / "verify-strih.sh"
BOXES = SCRIPTS / "strih-boxes"
TOOL = SCRIPTS / "janus_turn_rest_cfg.py"
HUB_TURN = REPO / "intercom" / "hub" / "src" / "turn.rs"
HUB_HTTP = REPO / "intercom" / "hub" / "src" / "http.rs"
LEAKY = ("STRIH_LX_IP", "STRIH_LX_HOST", "STRIH_LX_DANTESYNC_ROLE", "STRIH_BOXES_DIR", "OBS_FLEET",
         "CLOUDFLARE_TUNNEL_TOKEN")
FAKE_TOKEN = "fake-tunnel-token-of-the-1453-test"
FAKE_KEY = b"fake cloudflare signing key of the issue 1453 test\n"
TURN_REST = "http://127.0.0.1:8790/api/turn-rest"

JANUS_JCFG = """general: {
\tconfigs_folder = "/etc/janus"
}

nat: {
\t#stun_server = "stun.voip.eutelia.it"
\tnice_debug = false
\t# The TURN REST API (draft-uberti), like the package's sample:
\t#turn_rest_api = "http://yourbackend.com/path/to/api"
\t#turn_rest_api_method = "GET"
\t#ice_enforce_list = "eth0"
}

media: {
\t#ipv6 = true
}
"""


def _tool():
    spec = importlib.util.spec_from_file_location("janus_turn_rest_cfg", TOOL)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# --- the fakes ---------------------------------------------------------------------------------------------

def _py(body):
    return "#!%s\n%s" % (sys.executable, body)


_FAKE_DPKG = _py("""import os, sys
pkg = sys.argv[-1]
try:
    inst = open(os.path.join(os.environ["FAKE_STATE"], "installed")).read().split()
except FileNotFoundError:
    inst = []
if pkg in inst:
    sys.stdout.write("install ok installed")
""")

_FAKE_APT = _py("""import os, sys
st = os.environ["FAKE_STATE"]
a = sys.argv[1:]
open(os.path.join(st, "calls"), "a").write("apt-get " + " ".join(a) + "\\n")
if a and a[0] == "install":
    with open(os.path.join(st, "installed"), "a") as f:
        for x in a[1:]:
            if not x.startswith("-"):
                f.write(x + "\\n")
""")

_FAKE_SYSTEMCTL = _py("""import os, sys
st = os.environ["FAKE_STATE"]
a = [x for x in sys.argv[1:] if x != "--quiet"]
quiet = "--quiet" in sys.argv[1:]
open(os.path.join(st, "calls"), "a").write("systemctl " + " ".join(sys.argv[1:]) + "\\n")
def flag(kind, u):
    return os.path.join(st, "%s-%s" % (kind, u))
verb = a[0]
if verb == "is-enabled":
    if os.path.exists(flag("enabled", a[1])):
        print("enabled")
    else:
        print("disabled"); sys.exit(1)
elif verb == "is-active":
    if os.path.exists(flag("active", a[1])):
        quiet or print("active")
    else:
        quiet or print("inactive"); sys.exit(3)
elif verb == "enable":
    for u in a[1:]:
        open(flag("enabled", u), "w").close()
elif verb in ("start", "restart"):
    for u in a[1:]:
        open(flag("active", u), "w").close()
""")

# curl: `-o PATH URL` writes the fake signing key (or fails with the flag curl-key-fails); an http:// URL
# answers from $FAKE_STATE/http/<port>_<path with / -> _> = "<code>\\n<body>", absent = no answer; `-w`
# expands \\n and %{http_code} like curl.
_FAKE_CURL = _py("""import os, sys
st = os.environ["FAKE_STATE"]
a = sys.argv[1:]
open(os.path.join(st, "calls"), "a").write("curl " + " ".join(a) + "\\n")
url = a[-1]
if "-o" in a:
    if os.path.exists(os.path.join(st, "curl-key-fails")):
        sys.exit(22)
    open(a[a.index("-o") + 1], "wb").write(open(os.path.join(st, "key-bytes"), "rb").read())
    sys.exit(0)
rest = url.split("://", 1)[1]
hostport, _, path = rest.partition("/")
key = hostport.rsplit(":", 1)[-1] + "_" + path.replace("/", "_")
p = os.path.join(st, "http", key)
code, body = "000", ""
if os.path.exists(p):
    code, _, body = open(p).read().partition("\\n")
sys.stdout.write(body)
if "-w" in a:
    sys.stdout.write(a[a.index("-w") + 1].replace("\\\\n", "\\n").replace("%{http_code}", code))
sys.exit(0 if code != "000" else 7)
""")

FAKES = {"dpkg-query": _FAKE_DPKG, "apt-get": _FAKE_APT, "systemctl": _FAKE_SYSTEMCTL, "curl": _FAKE_CURL}


class Rig:
    """A temp root: fake binaries, every path the lib writes, the fake state."""

    def __init__(self, tmp):
        self.tmp = tmp
        self.bin = tmp / "bin"
        self.state = tmp / "state"
        self.units = tmp / "units"
        self.keyrings = tmp / "keyrings"
        self.apt = tmp / "apt"
        self.dropins = tmp / "intercom-hub.service.d"
        self.token = tmp / "cloudflared" / "interkom-tunnel.token"
        self.turn = tmp / "intercom-hub" / "turn.toml"
        self.jcfg = tmp / "janus" / "janus.jcfg"
        for d in (self.bin, self.state, self.state / "http", self.units):
            d.mkdir(parents=True, exist_ok=True)
        for name, body in FAKES.items():
            p = self.bin / name
            p.write_text(body)
            p.chmod(0o755)
        (self.state / "key-bytes").write_bytes(FAKE_KEY)

    def env(self, **extra):
        import pwd
        e = {k: v for k, v in os.environ.items() if k not in LEAKY}
        e.update(PATH="%s:%s" % (self.bin, os.environ["PATH"]), FAKE_STATE=str(self.state),
                 STRIH_PUBLIC_TOKEN_FILE=str(self.token), STRIH_PUBLIC_UNIT_DIR=str(self.units),
                 STRIH_PUBLIC_KEYRING_DIR=str(self.keyrings), STRIH_PUBLIC_APT_DIR=str(self.apt),
                 STRIH_PUBLIC_TURN_FILE=str(self.turn), STRIH_PUBLIC_HUB_DROPIN_DIR=str(self.dropins),
                 STRIH_PUBLIC_JANUS_JCFG=str(self.jcfg),
                 STRIH_PUBLIC_OWNER=pwd.getpwuid(os.getuid()).pw_name,
                 STRIH_PUBLIC_KEY_SHA256=hashlib.sha256(FAKE_KEY).hexdigest())
        e.update(extra)
        return e

    def calls(self):
        p = self.state / "calls"
        return p.read_text().splitlines() if p.exists() else []

    def clear_calls(self):
        (self.state / "calls").write_text("")

    def flag(self, name):
        (self.state / name).write_text("")

    def unflag(self, name):
        p = self.state / name
        if p.exists():
            p.unlink()

    def http(self, port, path, code, body=""):
        (self.state / "http" / ("%s_%s" % (port, path.lstrip("/").replace("/", "_")))).write_text(
            "%s\n%s" % (code, body))

    def place_token(self, mode=0o600):
        self.token.parent.mkdir(parents=True, exist_ok=True)
        self.token.write_text(FAKE_TOKEN + "\n")
        self.token.chmod(mode)

    def place_turn(self, mode=0o600):
        self.turn.parent.mkdir(parents=True, exist_ok=True)
        self.turn.write_text('key_id = "fake-key-id-1453"\nkey_api_token = "fake-turn-api-token-of-the-test"\n')
        self.turn.chmod(mode)

    def place_jcfg(self, text=JANUS_JCFG):
        self.jcfg.parent.mkdir(parents=True, exist_ok=True)
        self.jcfg.write_text(text)
        self.jcfg.chmod(0o644)


def _bash(script, *args, env=None):
    pre = "set -euo pipefail\n" + "".join('. "%s"\n' % s for s in (FACTS, LIB))
    e = env if env is not None else {k: v for k, v in os.environ.items() if k not in LEAKY}
    return subprocess.run(["bash", "-c", pre + script, "harness", *args],
                          capture_output=True, text=True, env=e, timeout=120)


def _install(rig, box="strih-snv", **extra):
    return _bash('strih_box_load "$1" || exit 9\nstrih_interkom_public_install', box, env=rig.env(**extra))


def _rows(rig, box="strih-snv", **extra):
    r = _bash('strih_box_load "$1" || exit 9\nstrih_interkom_public_grade_rows', box, env=rig.env(**extra))
    assert r.returncode == 0, r.stderr
    return [ln.split("|", 1) for ln in r.stdout.splitlines() if ln]


def _boxes_copy(tmp_path, box=None, old=None, new=None):
    d = tmp_path / "boxes"
    d.mkdir(exist_ok=True)
    for f in BOXES.glob("*.env"):
        (d / f.name).write_text(f.read_text())
    if box:
        f = d / ("%s.env" % box)
        body = f.read_text()
        assert body.count(old) == 1
        f.write_text(body.replace(old, new))
    return d


# --- the fact ----------------------------------------------------------------------------------------------

@pytest.mark.parametrize("box,want", [("strih-snv", "yes"), ("strih-pp", "yes"), ("stream-pp", "no")])
def test_the_fact_publishes_both_strih_boxes_and_never_the_stream_role(box, want):
    r = _bash('strih_box_load "$1" || exit 9\nstrih_lx_https_public', box)
    assert r.returncode == 0, r.stderr
    assert r.stdout == want


@pytest.mark.parametrize("box,old,new,word", [
    ("strih-snv", "STRIH_HTTPS_PUBLIC=yes\n", "STRIH_HTTPS_PUBLIC=maybe\n", "must be yes or no"),
    ("strih-snv", "STRIH_HTTPS_PUBLIC=yes\n", "STRIH_HTTPS_PUBLIC=YES\n", "must be yes or no"),
    ("stream-pp", "STRIH_HTTPS_PUBLIC=no\n", "STRIH_HTTPS_PUBLIC=yes\n", "needs an intercom site"),
])
def test_a_wrong_public_fact_refuses_by_name(tmp_path, box, old, new, word):
    d = _boxes_copy(tmp_path, box, old, new)
    e = {k: v for k, v in os.environ.items() if k not in LEAKY}
    e["STRIH_BOXES_DIR"] = str(d)
    r = _bash('strih_box_load "$1"', box, env=e)
    assert r.returncode != 0
    assert "STRIH_HTTPS_PUBLIC" in r.stderr and word in r.stderr, r.stderr


# --- the Janus jcfg tool -----------------------------------------------------------------------------------

def test_set_puts_the_managed_block_inside_nat_and_touches_nothing_else():
    t = _tool()
    new = t.apply(JANUS_JCFG, TURN_REST)
    added = [ln for ln in new.splitlines() if ln not in JANUS_JCFG.splitlines()]
    assert added == ["\t" + t.BEGIN, '\tturn_rest_api = "%s"' % TURN_REST, '\tturn_rest_api_method = "GET"',
                     "\t" + t.END]
    nat = new[new.index("nat: {"):new.index("media: {")]
    assert 'turn_rest_api = "%s"' % TURN_REST in nat
    assert t.check(new, TURN_REST) == "ok"
    assert t.apply(new, TURN_REST) == new, "a second set changes nothing"
    assert t.apply(new, None) == JANUS_JCFG, "clear gives the package file back byte-identical"
    assert t.check(JANUS_JCFG, TURN_REST) == "absent"
    assert t.check(t.apply(JANUS_JCFG, "http://127.0.0.1:9/x"), TURN_REST) == "differs"


def test_a_hand_set_turn_rest_api_is_a_conflict_never_a_duplicate():
    t = _tool()
    hand = JANUS_JCFG.replace('\tnice_debug = false\n', '\tnice_debug = false\n\tturn_rest_api = "http://x/y"\n')
    with pytest.raises(t.JcfgError):
        t.apply(hand, TURN_REST)
    with pytest.raises(t.JcfgError):
        t.apply(hand.replace("turn_rest_api =", "turn_rest_api_method ="), None)


def test_a_file_without_a_nat_section_gets_one():
    t = _tool()
    base = 'general: {\n\tconfigs_folder = "/etc/janus"\n}\n'
    new = t.apply(base, TURN_REST)
    assert new.startswith(base) and "\nnat: {\n" in new and new.endswith("}\n")
    assert t.check(new, TURN_REST) == "ok"
    assert t.apply(base, None) == base


def test_an_unclosed_nat_section_or_a_half_block_is_refused():
    t = _tool()
    with pytest.raises(t.JcfgError):
        t.apply("nat: {\n\tnice_debug = false\n", TURN_REST)
    with pytest.raises(t.JcfgError):
        t.apply("nat: {\n\t%s\n\tturn_rest_api = \"x\"\n}\n" % t.BEGIN, None)
    with pytest.raises(t.JcfgError):
        t.apply(JANUS_JCFG, 'http://x/"y')


def test_the_cli_writes_atomically_keeps_the_mode_and_refuses_a_conflict(tmp_path):
    f = tmp_path / "janus.jcfg"
    f.write_text(JANUS_JCFG)
    f.chmod(0o640)
    run = lambda *a: subprocess.run([sys.executable, str(TOOL), "--file", str(f), *a],  # noqa: E731
                                    capture_output=True, text=True)
    r = run("--set", TURN_REST)
    assert r.returncode == 0 and r.stdout.strip() == "changed", r.stderr
    assert stat.S_IMODE(f.stat().st_mode) == 0o640
    assert run("--set", TURN_REST).stdout.strip() == "unchanged"
    r = run("--check", TURN_REST)
    assert r.returncode == 0 and r.stdout.strip() == "ok"
    assert run("--clear").stdout.strip() == "changed"
    assert f.read_text() == JANUS_JCFG
    assert run("--check", TURN_REST).returncode == 1
    f.write_text(JANUS_JCFG.replace("nice_debug = false", 'nice_debug = false\n\tturn_rest_api = "x"'))
    before = f.read_text()
    r = run("--set", TURN_REST)
    assert r.returncode == 2 and "by hand" in r.stderr
    assert f.read_text() == before
    assert not [p for p in tmp_path.iterdir() if p.name.startswith(".janus.jcfg.")], "no temp file left"


def test_the_tool_parses_the_shipped_janus_package_shape():
    """The Ubuntu janus 1.1.2 nat section carries comment lines with braces + quotes; none of them may
    open or close the section."""
    t = _tool()
    text = JANUS_JCFG.replace('\t#ice_enforce_list = "eth0"\n',
                              '\t# a comment with { braces } and "quotes"\n\t#ice_enforce_list = "eth0"\n')
    new = t.apply(text, TURN_REST)
    assert t.check(new, TURN_REST) == "ok"
    assert new.index(t.BEGIN) < new.index("media: {")


# --- setup-strih step 16i ----------------------------------------------------------------------------------

def test_a_fresh_box_installs_cloudflared_and_the_unit_and_warns_twice(tmp_path):
    rig = Rig(tmp_path)
    r = _install(rig)
    assert r.returncode == 0, r.stdout + r.stderr
    calls = rig.calls()
    assert any(c.startswith("curl ") and "cloudflare-public-v2.gpg" in c for c in calls)
    assert "apt-get update" in calls
    assert "apt-get install -y --no-upgrade cloudflared" in calls
    assert (rig.keyrings / "cloudflare-public-v2.gpg").read_bytes() == FAKE_KEY
    lst = (rig.apt / "cloudflared.list").read_text()
    assert lst == "deb [signed-by=%s] https://pkg.cloudflare.com/cloudflared any main\n" % (
        rig.keyrings / "cloudflare-public-v2.gpg")
    unit = (rig.units / "cloudflared-interkom.service").read_text()
    assert "--token-file %d/tunnel.token" in unit
    assert "LoadCredential=tunnel.token:%s" % rig.token in unit
    assert "systemctl enable cloudflared-interkom.service" in calls
    assert not [c for c in calls if re.match(r"systemctl (start|restart) ", c)], calls
    warns = [ln for ln in r.stderr.splitlines() if "WARN" in ln]
    assert len(warns) == 2, r.stderr
    assert str(rig.token) in warns[0] and "strih_interkom_public_token_store" in warns[0]
    assert str(rig.turn) in warns[1] and "key_id + key_api_token" in warns[1]
    assert not rig.dropins.exists()


def test_a_key_that_does_not_match_the_pin_is_refused_before_apt(tmp_path):
    rig = Rig(tmp_path)
    (rig.state / "key-bytes").write_bytes(b"a different key\n")
    r = _install(rig)
    assert r.returncode == 1
    assert "is not the pinned" in r.stderr
    assert not [c for c in rig.calls() if c.startswith("apt-get")]
    assert not (rig.keyrings / "cloudflare-public-v2.gpg").exists()
    assert not (rig.units / "cloudflared-interkom.service").exists()


def test_the_real_pin_is_the_key_read_on_18_10_2026():
    text = LIB.read_text()
    assert 'STRIH_PUBLIC_KEY_PIN="1bd95f4082b320d541bee351560fc2765aa9f9cd8efa4c9e32135e63f252721d"' in text
    assert 'STRIH_PUBLIC_KEY_URL="https://pkg.cloudflare.com/cloudflare-public-v2.gpg"' in text


def test_the_token_from_the_environment_starts_the_tunnel_and_is_never_printed(tmp_path):
    rig = Rig(tmp_path)
    r = _install(rig, CLOUDFLARE_TUNNEL_TOKEN=FAKE_TOKEN)
    assert r.returncode == 0, r.stdout + r.stderr
    assert rig.token.read_text() == FAKE_TOKEN + "\n"
    assert stat.S_IMODE(rig.token.stat().st_mode) == 0o600
    assert "systemctl start cloudflared-interkom.service" in rig.calls()
    assert FAKE_TOKEN not in r.stdout + r.stderr
    assert FAKE_TOKEN not in "\n".join(rig.calls()), "the token never reaches an argv"
    assert FAKE_TOKEN not in (rig.units / "cloudflared-interkom.service").read_text()
    assert len([ln for ln in r.stderr.splitlines() if "WARN" in ln]) == 1, "only the TURN key warns"


def test_a_second_run_changes_nothing_and_a_new_token_restarts(tmp_path):
    rig = Rig(tmp_path)
    assert _install(rig, CLOUDFLARE_TUNNEL_TOKEN=FAKE_TOKEN).returncode == 0
    rig.clear_calls()
    r = _install(rig, CLOUDFLARE_TUNNEL_TOKEN=FAKE_TOKEN)
    assert r.returncode == 0, r.stderr
    calls = rig.calls()
    assert not [c for c in calls if c.startswith(("apt-get", "curl"))], calls
    assert not [c for c in calls if re.match(r"systemctl (start|restart|daemon-reload|enable)", c)], calls
    assert "unchanged" in r.stdout
    rig.clear_calls()
    r = _install(rig, CLOUDFLARE_TUNNEL_TOKEN=FAKE_TOKEN + "-rotated")
    assert r.returncode == 0, r.stderr
    assert "systemctl restart cloudflared-interkom.service" in rig.calls()


def test_the_token_store_takes_stdin_and_refuses_a_malformed_token(tmp_path):
    rig = Rig(tmp_path)
    store = 'strih_box_load "$1" || exit 9\nprintf "%s\\n" "$2" | strih_interkom_public_token_store'
    r = _bash(store, "strih-snv", "not a token", env=rig.env())
    assert r.returncode == 1 and "malformed" in r.stderr
    assert "not a token" not in r.stderr
    assert not rig.token.exists()
    r = _bash(store, "strih-snv", "short-token", env=rig.env())
    assert r.returncode == 1 and not rig.token.exists(), "under 20 characters is no tunnel token"
    r = _bash(store, "strih-snv", FAKE_TOKEN, env=rig.env())
    assert r.returncode == 0 and r.stdout == "written", r.stderr
    assert stat.S_IMODE(rig.token.stat().st_mode) == 0o600
    r = _bash(store, "strih-snv", FAKE_TOKEN, env=rig.env())
    assert r.stdout == "unchanged"
    r = _bash(store, "strih-snv", "tést-with-a-non-ascii-letter-1453", env=rig.env())
    assert r.returncode == 1, "a non-ASCII letter is refused in every locale"


def test_the_runbook_store_command_works_with_the_lib_alone_and_the_token_on_stdin(tmp_path):
    """The supervisor pipes the token over ssh stdin into the store with only this lib sourced."""
    rig = Rig(tmp_path)
    r = subprocess.run(["bash", "-c", 'set -euo pipefail; . "$1"; strih_interkom_public_token_store', "h", str(LIB)],
                       input=FAKE_TOKEN + "\n", capture_output=True, text=True, env=rig.env(), timeout=60)
    assert r.returncode == 0 and r.stdout == "written", r.stderr
    assert rig.token.read_text() == FAKE_TOKEN + "\n"
    assert FAKE_TOKEN not in r.stdout + r.stderr


def test_a_turn_key_wires_the_hub_and_janus_and_never_restarts_a_running_hub(tmp_path):
    rig = Rig(tmp_path)
    rig.place_token()
    rig.place_turn(mode=0o644)
    rig.place_jcfg()
    rig.flag("active-intercom-hub")
    rig.flag("active-janus")
    r = _install(rig)
    assert r.returncode == 0, r.stdout + r.stderr
    assert stat.S_IMODE(rig.turn.stat().st_mode) == 0o600
    dropin = (rig.dropins / "30-turn-credential.conf").read_text()
    assert "LoadCredential=turn.toml:%s" % rig.turn in dropin
    assert _tool().check(rig.jcfg.read_text(), TURN_REST) == "ok"
    calls = rig.calls()
    assert "systemctl daemon-reload" in calls
    assert "systemctl restart janus" in calls
    assert not [c for c in calls if "intercom-hub" in c and re.match(r"systemctl (start|restart|stop)", c)], calls
    assert "systemctl restart intercom-hub" in r.stdout, "the restart the supervisor owes is named"
    assert not [ln for ln in r.stderr.splitlines() if "WARN" in ln]
    rig.clear_calls()
    r = _install(rig)
    assert r.returncode == 0, r.stderr
    assert "systemctl restart janus" not in rig.calls(), "an unchanged jcfg never restarts Janus"


def test_a_removed_turn_key_unwires_the_hub_and_janus(tmp_path):
    rig = Rig(tmp_path)
    rig.place_token()
    rig.place_turn()
    rig.place_jcfg()
    assert _install(rig).returncode == 0
    rig.turn.unlink()
    r = _install(rig)
    assert r.returncode == 0, r.stderr
    assert not (rig.dropins / "30-turn-credential.conf").exists()
    assert rig.jcfg.read_text() == JANUS_JCFG


def test_a_hand_set_janus_key_fails_the_step_and_leaves_the_file(tmp_path):
    rig = Rig(tmp_path)
    rig.place_token()
    rig.place_turn()
    hand = JANUS_JCFG.replace("nice_debug = false", 'nice_debug = false\n\tturn_rest_api = "x"')
    rig.place_jcfg(hand)
    r = _install(rig)
    assert r.returncode == 1
    assert "by hand" in r.stderr
    assert rig.jcfg.read_text() == hand


def test_a_box_that_publishes_nothing_installs_nothing_and_unwires_turn(tmp_path):
    rig = Rig(tmp_path)
    d = _boxes_copy(tmp_path, "strih-pp", "STRIH_HTTPS_PUBLIC=yes\n", "STRIH_HTTPS_PUBLIC=no\n")
    rig.dropins.mkdir(parents=True)
    (rig.dropins / "30-turn-credential.conf").write_text("x\n")
    rig.place_jcfg(_tool().apply(JANUS_JCFG, TURN_REST))
    r = _install(rig, box="strih-pp", STRIH_BOXES_DIR=str(d))
    assert r.returncode == 0, r.stderr
    assert "publishes nothing" in r.stdout
    assert not [c for c in rig.calls() if c.startswith(("apt-get", "curl"))]
    assert not (rig.units / "cloudflared-interkom.service").exists()
    assert not (rig.dropins / "30-turn-credential.conf").exists()
    assert rig.jcfg.read_text() == JANUS_JCFG


def test_the_unit_holds_no_token_and_runs_unprivileged():
    r = _bash("strih_public_unit_text")
    assert r.returncode == 0, r.stderr
    u = r.stdout
    assert "ExecStart=/usr/bin/cloudflared tunnel --no-autoupdate --metrics 127.0.0.1:20241 run " \
           "--token-file %d/tunnel.token" in u
    assert "ConditionPathExists=/etc/cloudflared/interkom-tunnel.token" in u
    assert "LoadCredential=tunnel.token:/etc/cloudflared/interkom-tunnel.token" in u
    for line in ("DynamicUser=yes", "NoNewPrivileges=yes", "Type=notify", "WantedBy=multi-user.target"):
        assert line in u
    assert "--token " not in u, "never the token on the command line"


# --- verify-strih item 45, the two new rows ----------------------------------------------------------------

def _healthy(rig):
    rig.place_jcfg()
    assert _install(rig, CLOUDFLARE_TUNNEL_TOKEN=FAKE_TOKEN).returncode == 0
    rig.place_turn()
    assert _install(rig).returncode == 0
    rig.flag("active-cloudflared-interkom.service")
    rig.http(20241, "/ready", 200, '{"status":200,"readyConnections":4,"connectorId":"x"}')
    rig.http(8790, "/api/turn-rest", 200,
             '{"username":"u","password":"p","ttl":86400,"uris":["turn:turn.cloudflare.com:3478?transport=udp"]}')


def test_a_healthy_box_passes_both_rows(tmp_path):
    rig = Rig(tmp_path)
    _healthy(rig)
    rows = _rows(rig)
    assert [st for st, _ in rows] == ["OK", "OK"], rows
    assert rows[0][1].startswith("(https-tunnel)") and "4 connection" in rows[0][1]
    assert rows[1][1].startswith("(https-turn)")


@pytest.mark.parametrize("break_it,tag", [
    (lambda rig: rig.token.unlink(), "(https-tunnel)"),
    (lambda rig: rig.token.chmod(0o644), "(https-tunnel)"),
    (lambda rig: (rig.units / "cloudflared-interkom.service").write_text("# hand edit\n"), "(https-tunnel)"),
    (lambda rig: rig.unflag("enabled-cloudflared-interkom.service"), "(https-tunnel)"),
    (lambda rig: rig.unflag("active-cloudflared-interkom.service"), "(https-tunnel)"),
    (lambda rig: rig.http(20241, "/ready", 503, '{"status":503,"readyConnections":0}'), "(https-tunnel)"),
    (lambda rig: rig.http(20241, "/ready", 200, '{"status":200,"readyConnections":0}'), "(https-tunnel)"),
    (lambda rig: rig.turn.unlink(), "(https-turn)"),
    (lambda rig: rig.turn.chmod(0o640), "(https-turn)"),
    (lambda rig: (rig.dropins / "30-turn-credential.conf").unlink(), "(https-turn)"),
    (lambda rig: rig.jcfg.write_text(JANUS_JCFG), "(https-turn)"),
    (lambda rig: rig.http(8790, "/api/turn-rest", 404, "TURN not configured: no TURN key file"), "(https-turn)"),
    (lambda rig: rig.http(8790, "/api/turn-rest", 200, '{"uris":[]}'), "(https-turn)"),
])
def test_each_row_fails_by_name(tmp_path, break_it, tag):
    rig = Rig(tmp_path)
    _healthy(rig)
    break_it(rig)
    rows = _rows(rig)
    bad = [t for st, t in rows if st == "FAIL"]
    assert len(bad) == 1, rows
    assert bad[0].startswith(tag), bad


def test_a_hub_that_answers_turn_not_configured_names_the_reason(tmp_path):
    rig = Rig(tmp_path)
    _healthy(rig)
    rig.http(8790, "/api/turn-rest", 404, "TURN not configured: no TURN key file at /etc/intercom-hub/turn.toml")
    bad = [t for st, t in _rows(rig) if st == "FAIL"]
    assert "404" in bad[0] and "TURN not configured" in bad[0] and "restart intercom-hub" in bad[0]


def test_a_box_that_publishes_nothing_gets_two_notes(tmp_path):
    rig = Rig(tmp_path)
    d = _boxes_copy(tmp_path, "strih-pp", "STRIH_HTTPS_PUBLIC=yes\n", "STRIH_HTTPS_PUBLIC=no\n")
    rows = _rows(rig, box="strih-pp", STRIH_BOXES_DIR=str(d))
    assert [st for st, _ in rows] == ["NOTE", "NOTE"], rows


def test_the_report_prints_through_the_callers_functions(tmp_path):
    rig = Rig(tmp_path)
    _healthy(rig)
    r = _bash('ok() { echo "PASS $1"; }; bad() { echo "FAIL $1"; }; note() { echo "NOTE $1"; }\n'
              'strih_box_load "$1" || exit 9\nstrih_interkom_public_grade_report; echo "RC=$?"', "strih-snv",
              env=rig.env())
    assert r.returncode == 0, r.stderr
    assert r.stdout.count("PASS ") == 2 and r.stdout.strip().endswith("RC=0"), r.stdout


# --- the wiring + the cross-language pins ------------------------------------------------------------------

def test_setup_16i_and_verify_45_run_the_public_path_within_the_line_budget():
    setup = SETUP.read_text()
    verify = VERIFY.read_text()
    i = setup.index('step "16i"')
    body = setup[i:setup.index("\nstep ", i + 1)]
    assert body.index("strih_https_install") < body.index("strih_interkom_public_install")
    j = verify.index("# 45) issue 1399")
    item = verify[j:verify.index("\n", verify.index("\n", j) + 1)]
    assert item.index("strih_https_grade_report") < item.index("strih_interkom_public_grade_report")
    assert len(setup.splitlines()) <= 1000 and len(verify.splitlines()) <= 1000


def test_the_provision_lib_sources_the_public_lib():
    text = PROVISION.read_text()
    assert re.search(r'(?m)^  \. "\$\(dirname "\$\{BASH_SOURCE\[0\]\}"\)/strih-interkom-public\.sh"$', text)


def test_the_hub_reads_the_same_credential_name_path_and_route():
    turn = HUB_TURN.read_text()
    assert 'pub const TURN_CONFIG_CRED: &str = "turn.toml";' in turn
    assert 'pub const TURN_CONFIG_DEFAULT: &str = "/etc/intercom-hub/turn.toml";' in turn
    assert '.route("/api/turn-rest", get(api_turn_rest))' in HUB_HTTP.read_text()
    r = _bash('printf "%s|%s|%s" "$STRIH_PUBLIC_TURN_CRED" "$(strih_public_turn_file)" "$(strih_public_turn_rest_url)"')
    assert r.stdout == "turn.toml|/etc/intercom-hub/turn.toml|%s" % TURN_REST, r.stdout + r.stderr


def test_the_lib_never_echoes_the_token():
    """The token value is never printed: no echo / printf of the variables that hold it."""
    code = "\n".join(ln for ln in LIB.read_text().splitlines() if not ln.lstrip().startswith("#"))
    for bad in ('echo "$tok"', 'echo "${tok}"', 'echo "$CLOUDFLARE_TUNNEL_TOKEN"', "set -x"):
        assert bad not in code, bad
    assert code.count("$CLOUDFLARE_TUNNEL_TOKEN") == 1, "read in one place only: piped into the store"
