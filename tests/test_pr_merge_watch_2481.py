"""#2481: a merge-group run from an earlier queue attempt must not eject a re-enqueued PR."""

from __future__ import annotations

import importlib.util
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "crates" / "clud-bin" / "assets" / "tools" / "github" / "pr_merge_watch.py"
spec = importlib.util.spec_from_file_location("pr_merge_watch_2481", SCRIPT)
assert spec is not None
assert spec.loader is not None
watcher = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = watcher
spec.loader.exec_module(watcher)

ENQUEUED = datetime(2026, 10, 31, 22, 30, tzinfo=timezone.utc)


def test_runs_before_the_entry_are_stale() -> None:
    runs = [
        {"id": 1, "created_at": "2026-10-31T22:03:00Z"},
        {"id": 2, "created_at": "2026-10-31T22:31:00Z"},
    ]
    assert [r["id"] for r in watcher.current_attempt_runs(runs, ENQUEUED)] == [2]


def test_without_an_entry_time_every_run_counts() -> None:
    runs = [{"id": 1, "created_at": "2026-10-31T22:03:00Z"}, {"id": 2}]
    assert [r["id"] for r in watcher.current_attempt_runs(runs, None)] == [1, 2]


def test_queue_state_reads_enqueued_at() -> None:
    data = {"data": {"repository": {"pullRequest": {
        "state": "OPEN", "baseRefName": "main", "autoMergeRequest": None,
        "mergeQueueEntry": {"state": "AWAITING_CHECKS", "position": 1,
                            "enqueuedAt": "2026-10-31T22:30:00Z", "headCommit": {"oid": "abc"}},
        "timelineItems": {"nodes": []},
    }}}}
    state = watcher.parse_queue_state(data)
    assert state is not None
    assert state.enqueued_at == ENQUEUED
