"""--enqueue-when-ready: a watch that submits a green, unqueued PR to the merge queue."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "crates" / "clud-bin" / "assets" / "tools" / "github" / "pr_merge_watch.py"
spec = importlib.util.spec_from_file_location("pr_merge_watch_enqueue", SCRIPT)
assert spec is not None
assert spec.loader is not None
watcher = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = watcher
spec.loader.exec_module(watcher)

HEAD = "a" * 40


def _ready(*, draft=False, status="CLEAN", entry=None, checks=None):
    nodes = checks if checks is not None else [
        {"name": "CI OK", "status": "COMPLETED", "conclusion": "SUCCESS", "isRequired": True},
    ]
    return {"data": {"repository": {"pullRequest": {
        "id": "PR_1", "state": "OPEN", "isDraft": draft, "mergeStateStatus": status,
        "headRefOid": HEAD, "mergeQueueEntry": entry,
        "commits": {"nodes": [{"commit": {"oid": HEAD, "statusCheckRollup": {"contexts": {
            "pageInfo": {"hasNextPage": False}, "nodes": nodes}}}}]},
    }}}}


class FakeApi:
    def __init__(self, ready):
        self.ready = ready
        self.mutations = []

    def __call__(self, *args):
        query = next(a for a in args if a.startswith("query="))
        if "enqueuePullRequest" in query:
            self.mutations.append(args)
            entry = {"position": 2, "state": "QUEUED"}
            return {"data": {"enqueuePullRequest": {"mergeQueueEntry": entry}}}
        return self.ready


def _seen():
    return watcher.MergeFollow(started=watcher._utc_now())


def _run(monkeypatch, ready, seen=None):
    api = FakeApi(ready)
    monkeypatch.setattr(watcher, "gh_json", api)
    seen = seen or _seen()
    return watcher.maybe_enqueue("o/r", 7, seen), api, seen


def test_enqueue_on_green(monkeypatch, capsys) -> None:
    done, api, seen = _run(monkeypatch, _ready())
    assert done
    assert seen.enqueues == 1
    assert len(api.mutations) == 1
    assert f"sha={HEAD}" in api.mutations[0]
    assert not any("jump" in a for a in api.mutations[0])
    assert "ENQUEUED-BY-WATCH  #7 position=2" in capsys.readouterr().out


def test_no_op_when_already_queued(monkeypatch) -> None:
    done, api, _ = _run(monkeypatch, _ready(entry={"position": 1, "state": "QUEUED"}))
    assert not done
    assert api.mutations == []


def test_refuses_failing_required_check(monkeypatch, capsys) -> None:
    checks = [{"name": "CI OK", "status": "COMPLETED", "conclusion": "FAILURE", "isRequired": True}]
    done, api, _ = _run(monkeypatch, _ready(checks=checks))
    assert not done
    assert api.mutations == []
    assert "FAILURE" in capsys.readouterr().out


def test_refuses_pending_required_check(monkeypatch) -> None:
    checks = [{"name": "CI OK", "status": "IN_PROGRESS", "conclusion": None, "isRequired": True}]
    done, api, _ = _run(monkeypatch, _ready(checks=checks))
    assert not done
    assert api.mutations == []


def test_refuses_draft(monkeypatch) -> None:
    done, api, _ = _run(monkeypatch, _ready(draft=True))
    assert not done
    assert api.mutations == []


def test_refuses_blocked_and_dirty(monkeypatch) -> None:
    for status in ("BLOCKED", "DIRTY"):
        done, api, _ = _run(monkeypatch, _ready(status=status))
        assert not done
        assert api.mutations == []


def test_reenqueue_after_drop_is_bounded(monkeypatch, capsys) -> None:
    api = FakeApi(_ready())
    monkeypatch.setattr(watcher, "gh_json", api)
    monkeypatch.setattr(watcher, "MAX_ENQUEUES", 2)
    seen = _seen()
    assert watcher.maybe_enqueue("o/r", 7, seen)
    assert watcher.maybe_enqueue("o/r", 7, seen)  # entry dropped, PR still green
    assert not watcher.maybe_enqueue("o/r", 7, seen)
    out = capsys.readouterr().out
    assert len(api.mutations) == 2
    assert "re-enqueue after drop" in out
    assert "ENQUEUE-CAP" in out


def test_poll_enqueues_only_when_flag_set(monkeypatch) -> None:
    queue = watcher.QueueState(state="OPEN", base_ref="main", auto_merge=False, entry_state=None,
                               position=None, queue_sha=None, removal_reason=None, removed_at=None)
    monkeypatch.setattr(watcher, "fetch_queue_state", lambda repo, pr: queue)
    calls = []
    monkeypatch.setattr(watcher, "maybe_enqueue", lambda *a: calls.append(a) or True)
    monkeypatch.setattr(watcher, "EXPLICIT_MERGE_WAIT", True)
    monkeypatch.setattr(watcher, "ENQUEUE_WHEN_READY", False)
    watcher._poll_queue("o/r", 7, _seen(), None)
    assert calls == []
    monkeypatch.setattr(watcher, "ENQUEUE_WHEN_READY", True)
    seen = _seen()
    assert watcher._poll_queue("o/r", 7, seen, None) is None
    assert calls
    assert seen.enqueued
