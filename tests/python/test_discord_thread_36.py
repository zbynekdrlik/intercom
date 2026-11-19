"""intercom#36: the Discord CLI may touch only the 'Interkom vypadky' thread and never pings anyone."""

import importlib.util
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location(
    "discord_thread", Path(__file__).resolve().parents[2] / "scripts" / "discord_thread.py"
)
dt = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(dt)


def test_only_the_intercom_thread_is_accepted():
    assert dt.check_channel("1573095803457769532") == "1573095803457769532"
    for other in ("1062019161523507311", "1561136322570231898", "", "1573095803457769533"):
        with pytest.raises(dt.DiscordError):
            dt.check_channel(other)


def test_main_refuses_another_channel_before_reading_the_token(monkeypatch, capsys):
    monkeypatch.delenv("DISCORD_BOT_TOKEN", raising=False)
    assert dt.main(["--channel", "1062019161523507311", "read"]) == 1
    assert "not the intercom thread" in capsys.readouterr().err


def test_main_without_a_token_fails_loudly(monkeypatch, capsys):
    monkeypatch.delenv("DISCORD_BOT_TOKEN", raising=False)
    assert dt.main(["read"]) == 1
    assert "DISCORD_BOT_TOKEN is not set" in capsys.readouterr().err


def test_post_body_never_pings_and_checks_the_reply_id():
    body = dt.post_body("ahoj", "123")
    assert body["allowed_mentions"] == {"parse": []}
    assert body["message_reference"]["message_id"] == "123"
    with pytest.raises(dt.DiscordError):
        dt.post_body("ahoj", "123/../../guilds")
    with pytest.raises(dt.DiscordError):
        dt.post_body("   ", None)
    with pytest.raises(dt.DiscordError):
        dt.post_body("x" * 2001, None)


def test_mentions_only_counts_a_tag_and_never_the_bot_itself():
    bot = dt.BOT_ID
    assert dt.mentions_bot({"author": {"id": "1"}, "mentions": [{"id": bot}]})
    assert dt.mentions_bot({"author": {"id": "1"}, "content": f"<@{bot}> pomoz"})
    assert not dt.mentions_bot({"author": {"id": "1"}, "content": "interkom zase nejde"})
    assert not dt.mentions_bot({"author": {"id": bot}, "mentions": [{"id": bot}]})


def test_context_is_what_came_after_the_bots_last_message():
    bot = dt.BOT_ID
    msgs = [
        {"id": "3", "author": {"id": "1"}},
        {"id": "1", "author": {"id": "1"}},
        {"id": "2", "author": {"id": bot}},
    ]
    assert [m["id"] for m in dt.since_last_bot(msgs)] == ["3"]
    assert [m["id"] for m in dt.since_last_bot([{"id": "5", "author": {"id": "1"}}])] == ["5"]


def test_react_never_lets_the_emoji_leave_the_thread(monkeypatch):
    seen = []
    monkeypatch.setattr(dt, "request", lambda method, path, token, body=None: seen.append(path))
    dt.cmd_react("t", dt.THREAD_ID, "123", "x/../../../guilds/1")
    assert seen[0].startswith(f"/channels/{dt.THREAD_ID}/messages/123/reactions/")
    assert "/../" not in seen[0] and seen[0].endswith("/@me")
