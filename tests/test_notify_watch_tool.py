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
