"""The event watcher (owner 22.11.2026): what counts as an anomaly between two hub snapshots."""

import importlib.util
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "event_watch", Path(__file__).resolve().parents[2] / "scripts" / "event_watch.py"
)
ew = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ew)


def snap(**kw):
    base = {"lost_ticks": 0, "participants": [{"name": "cam1", "underruns": 0, "overruns": 0}]}
    base.update(kw)
    return base


def test_a_quiet_hub_is_no_anomaly():
    assert ew.anomalies(snap(), snap()) == []


def test_lost_ticks_and_an_underrun_burst_are_anomalies():
    cur = snap(lost_ticks=3, participants=[{"name": "cam1", "underruns": 50, "overruns": 0}])
    found = ew.anomalies(snap(), cur)
    assert any("lost_ticks" in f for f in found) and any("underruns +50" in f for f in found)


def test_a_few_underruns_are_not():
    cur = snap(participants=[{"name": "cam1", "underruns": 5, "overruns": 0}])
    assert ew.anomalies(snap(), cur) == []


def test_the_janus_leg_and_the_webrtc_relay():
    prev = snap(participants=[{"name": "phones", "janus": {"joined": True, "rejoin_count": 0}}],
                webrtc={"turn": {"state": "allocated", "failures": 0}, "phones": []})
    cur = snap(participants=[{"name": "phones", "janus": {"joined": False, "rejoin_count": 1}}],
               webrtc={"turn": {"state": "backoff", "failures": 1, "last_error": "x"},
                       "phones": [{"display": "Kamera 2", "ice_state": "failed"}]})
    found = " | ".join(ew.anomalies(prev, cur))
    for want in ("left the room", "rejoin 0 -> 1", "allocated -> backoff", "failures 0 -> 1", "Kamera 2 ice failed"):
        assert want in found


def test_the_fetch_names_its_own_user_agent(monkeypatch):
    # Cloudflare 403s urllib's default User-Agent: every poll then looked like a dead hub.
    seen = {}

    class Resp:
        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

        def read(self):
            return b'{"lost_ticks": 0}'

    def fake_urlopen(req, timeout):
        seen["ua"] = req.get_header("User-agent")
        return Resp()

    monkeypatch.setattr(ew.urllib.request, "urlopen", fake_urlopen)
    assert ew.fetch("https://x") == {"lost_ticks": 0}
    assert seen["ua"] and "Python-urllib" not in seen["ua"]
