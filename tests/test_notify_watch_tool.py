"""Unit coverage for the bundled `notify/watch.py` event watcher."""

from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

TOOL = (
    Path(__file__).resolve().parents[1]
    / "crates"
    / "clud-bin"
    / "assets"
    / "tools"
    / "notify"
    / "watch.py"
)


def _load():
    spec = importlib.util.spec_from_file_location("notify_watch", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


watch = _load()


@pytest.fixture
def base(tmp_path, monkeypatch):
    monkeypatch.setenv("CLUD_WATCH_DIR", str(tmp_path))
    return watch.state_dir()


def test_emit_dedupes_by_key_across_reloads(base) -> None:
    data = watch.load_state(base)
    assert watch.emit(base, data, "pr-merged", "o/r#1", "x", "pr-merged:o/r#1:abc")
    watch.save_state(base, data)
    data = watch.load_state(base)
    assert watch.emit(base, data, "pr-merged", "o/r#1", "x", "pr-merged:o/r#1:abc") is None
    assert [e["seq"] for e in watch.read_events(base)] == [1]


def test_seen_set_is_bounded(base) -> None:
    data = watch.load_state(base)
    data["seen"] = [f"k{i}" for i in range(watch.SEEN_LIMIT + 50)]
    watch.save_state(base, data)
    assert len(watch.load_state(base)["seen"]) == watch.SEEN_LIMIT


def test_since_prints_only_newer_events_and_cursor(base, capsys) -> None:
    data = watch.load_state(base)
    for i in range(3):
        watch.emit(base, data, "error", "s", "d", f"k{i}")
    assert watch.main(["since", "1"]) == 0
    lines = capsys.readouterr().out.splitlines()
    assert [line.split()[0] for line in lines[:-1]] == ["#2", "#3"]
    assert lines[-1] == "cursor 3"


def test_parse_pty_running_stops_at_api_table() -> None:
    text = "SESSION PID\nsess-1-10 (x) 12 2h /a\n\nAPI SESSION STATUS\nsess-9-1 terminated /tmp\n"
    assert watch.parse_pty_running(text) == {"sess-1-10"}


def test_session_verdict_reads_latest_sealed_turn() -> None:
    record = {
        "state": "idle",
        "turns": [{"id": "t1", "state": "completed", "disposition": "exit_0"}],
        "events": [{"turn_id": "t1", "data": {"result": "all done"}}],
    }
    assert watch.session_verdict(record) == ("t1", "exit_0", "all done")
    record["turns"][0]["state"] = "running"
    assert watch.session_verdict(record) is None


def _pr(**kw):
    pr = {
        "state": "OPEN",
        "merged": False,
        "headRefOid": "h" * 40,
        "mergeQueueEntry": None,
        "commits": {"nodes": []},
    }
    pr.update(kw)
    return pr


def kinds(evs):
    return [e[0] for e in evs]


def test_pr_transitions_queue_eject_merge_and_timeout() -> None:
    entry = {"state": "QUEUED", "enqueuedAt": "2026-01-01T00:00:00Z"}
    now = watch.datetime.fromisoformat("2026-01-01T00:10:00+00:00").timestamp()
    assert kinds(watch.pr_transitions("o/r#1", _pr(mergeQueueEntry=entry), {}, now, 3600)) == [
        "pr-queued"
    ]
    late = now + 3600
    assert kinds(watch.pr_transitions("o/r#1", _pr(mergeQueueEntry=entry), {}, late, 3600)) == [
        "pr-queued",
        "checks-timed-out",
    ]
    ejected = watch.pr_transitions("o/r#1", _pr(), {"enqueued_at": entry["enqueuedAt"]}, now, 3600)
    assert kinds(ejected) == ["pr-ejected"]
    merged = watch.pr_transitions(
        "o/r#1", _pr(merged=True, mergeCommit={"oid": "abc"}), {}, now, 3600
    )
    assert merged[0][2] == "pr-merged:o/r#1:abc"


def test_pr_failed_keyed_by_head_sha() -> None:
    pr = _pr(commits={"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]})
    (event,) = watch.pr_transitions("o/r#2", pr, {}, 0, 3600)
    assert event[0] == "pr-failed"
    assert event[2].endswith("h" * 40)


def test_unknown_session_and_dead_daemon_are_error_events(base, monkeypatch) -> None:
    def down():
        raise watch.DaemonDownError("connection refused")

    monkeypatch.setattr(watch, "daemon_info", down)
    data = watch.load_state(base)
    data["sessions"]["sess-x"] = {}
    _, errored = watch.poll_sessions(base, data)
    assert errored
    (event,) = watch.read_events(base)
    assert event["kind"] == "error"
    assert "connection refused" in event["detail"]


def test_bad_pr_subject_is_rejected() -> None:
    with pytest.raises(ValueError, match="OWNER/REPO#N"):
        watch.parse_pr_subject("not-a-pr")


def test_missing_telegram_credentials_is_a_loud_error(base) -> None:
    data = watch.load_state(base)
    assert watch.deliver(base, data, ("", "")) is True
    assert watch.read_events(base)[0]["subject"] == "config"


def _session_daemon(monkeypatch, record):
    monkeypatch.setattr(watch, "daemon_info", lambda: ("http://x", "t"))
    monkeypatch.setattr(watch, "pty_running_ids", set)
    monkeypatch.setattr(watch, "fetch_session", lambda *_: record)


def _running(cursor=5):
    return {
        "state": "running",
        "next_event_cursor": cursor,
        "updated_at_ms": cursor,
        "turns": [{"id": "t1", "state": "running"}],
        "events": [{"data": {"text": "waiting for crates.io zccache 1.15.12"}}],
    }


def _advance_clock(monkeypatch, minutes):
    now = watch.time.time() + minutes * 60
    monkeypatch.setattr(watch.time, "time", lambda: now)


def test_frozen_session_emits_exactly_one_stall(base, monkeypatch) -> None:
    _session_daemon(monkeypatch, _running())
    data = watch.load_state(base)
    data["sessions"]["sess-s"] = {"label": "agent"}
    watch.poll_sessions(base, data, 10)
    assert watch.read_events(base) == []
    _advance_clock(monkeypatch, 11)
    for _ in range(3):
        watch.poll_sessions(base, data, 10)
    (event,) = watch.read_events(base)
    assert event["kind"] == "session-stalled"
    assert event["minutes_idle"] >= 10
    assert "crates.io zccache" in event["detail"]


def test_stall_fires_again_after_progress_resumes(base, monkeypatch) -> None:
    record = _running()
    _session_daemon(monkeypatch, record)
    data = watch.load_state(base)
    data["sessions"]["sess-s"] = {}
    watch.poll_sessions(base, data, 10)
    _advance_clock(monkeypatch, 11)
    watch.poll_sessions(base, data, 10)
    record.update(next_event_cursor=9, updated_at_ms=9)
    watch.poll_sessions(base, data, 10)
    assert len(watch.read_events(base)) == 1
    _advance_clock(monkeypatch, 25)
    watch.poll_sessions(base, data, 10)
    events = watch.read_events(base)
    assert [e["kind"] for e in events] == ["session-stalled"] * 2
    assert events[0]["key"] != events[1]["key"]


def _finished(state, disposition):
    return {
        "state": "terminated",
        "turns": [{"id": "t1", "state": state, "disposition": disposition}],
    }


@pytest.mark.parametrize(
    ("state", "disposition", "kind", "phrase"),
    [
        ("killed", "terminal_kill", "session-terminated", "aborted"),
        ("failed", "exit_1", "session-failed", "did not succeed"),
        ("completed", "exit_0", "session-sealed", "completed successfully"),
    ],
)
def test_terminal_outcomes_are_distinct(base, monkeypatch, state, disposition, kind, phrase):
    _session_daemon(monkeypatch, _finished(state, disposition))
    data = watch.load_state(base)
    data["sessions"]["sess-k"] = {}
    watch.poll_sessions(base, data)
    (event,) = watch.read_events(base)
    assert event["kind"] == kind
    assert event["ok"] is (kind == "session-sealed")
    assert event["disposition"] == disposition
    assert phrase in event["detail"]
    assert "sess-k" not in data["sessions"]


def test_cursor_pages_over_new_kinds(base, capsys) -> None:
    data = watch.load_state(base)
    watch.emit(base, data, "session-stalled", "s", "d", "k1", minutes_idle=11.0)
    watch.emit(base, data, "session-terminated", "s", "d", "k2", ok=False)
    watch.emit(base, data, "session-sealed", "s", "d", "k3", ok=True)
    assert watch.main(["since", "1", "--json"]) == 0
    lines = [watch.json.loads(x) for x in capsys.readouterr().out.splitlines()]
    assert [e.get("kind") for e in lines[:-1]] == ["session-terminated", "session-sealed"]
    assert lines[0]["ok"] is False
    assert lines[-1] == {"cursor": 3}
