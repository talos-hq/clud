"""--land-when-ready: queue vs direct strategy detection and both landing paths."""

from __future__ import annotations

import importlib.util
import sys
from datetime import timedelta
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "crates" / "clud-bin" / "assets" / "tools" / "github" / "pr_merge_watch.py"
spec = importlib.util.spec_from_file_location("pr_merge_watch_land", SCRIPT)
assert spec is not None
assert spec.loader is not None
w = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = w
spec.loader.exec_module(w)

HEAD = "a" * 40
MOVED = "b" * 40
MERGE_SHA = "c" * 40
QUEUE_RULESET = {"id": 11, "name": "main", "target": "branch", "enforcement": "active",
                 "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
                 "rules": [{"type": "merge_queue"}, {"type": "pull_request"}]}
PLAIN_RULESET = {**QUEUE_RULESET, "id": 12, "name": "protect", "rules": [{"type": "deletion"}]}


def _pr(*, draft=False, status="CLEAN", mergeable="MERGEABLE", head=HEAD, checks=None):
    nodes = checks if checks is not None else [
        {"name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "isRequired": True}]
    return {"data": {"repository": {"pullRequest": {
        "id": "PR_1", "state": "OPEN", "isDraft": draft, "mergeable": mergeable,
        "mergeStateStatus": status, "headRefOid": head, "baseRefName": "main",
        "mergeQueueEntry": None,
        "commits": {"nodes": [{"commit": {"oid": head, "statusCheckRollup": {"contexts": {
            "pageInfo": {"hasNextPage": False}, "nodes": nodes}}}}]}}}}}


class Api:
    """Stub for gh_json keyed on the request shape; records every call."""

    def __init__(self, *, rulesets=(), queue=False, repo=None, ready=None, merge_ok=True,
                 rulesets_readable=True):
        self.rulesets = list(rulesets)
        self.queue = queue
        self.repo = repo or {"default_branch": "main", "allow_squash_merge": True,
                             "allow_merge_commit": True, "allow_rebase_merge": True}
        self.ready = list(ready or [_pr()])
        self.merge_ok = merge_ok
        self.readable = rulesets_readable
        self.calls: list[tuple] = []

    def __call__(self, *args):
        self.calls.append(args)
        joined = " ".join(args)
        if "--method" in args and "/merge" in joined:
            return {"merged": True, "sha": MERGE_SHA} if self.merge_ok else None
        if "query=" in joined:
            if "mergeQueue(branch" in joined:
                return {"data": {"repository": {"mergeQueue": {"id": "Q"} if self.queue else None}}}
            if "enqueuePullRequest" in joined:
                return {"data": {"enqueuePullRequest": {"mergeQueueEntry": {"position": 1}}}}
            return self.ready.pop(0) if len(self.ready) > 1 else self.ready[0]
        path = args[1]
        if "/rulesets?" in path:
            return [{"id": r["id"]} for r in self.rulesets] if self.readable else None
        if "/rulesets/" in path:
            rid = int(path.rsplit("/", 1)[1])
            return next(r for r in self.rulesets if r["id"] == rid)
        if path.startswith("repos/o/r/commits"):
            return [{"parents": [{}, {}]}]
        if path == "repos/o/r":
            return self.repo
        return None

    def merges(self):
        return [c for c in self.calls if "--method" in c]

    def enqueues(self):
        return [c for c in self.calls if any("enqueuePullRequest" in a for a in c)]


def _install(monkeypatch, api, method=None):
    monkeypatch.setattr(w, "gh_json", api)
    monkeypatch.setattr(w, "fetch_merged_at", lambda repo, pr: "2026-11-01T12:00:00Z")
    monkeypatch.setattr(w, "_STRATEGY_CACHE", {})
    monkeypatch.setattr(w, "LAND_WHEN_READY", True)
    monkeypatch.setattr(w, "MERGE_METHOD", method)
    return api


def _q(**kw):
    base = dict(state="OPEN", base_ref="main", auto_merge=False, entry_state=None, position=None,
                queue_sha=None, removal_reason=None, removed_at=None)
    return w.QueueState(**{**base, **kw})


def _seen():
    return w.MergeFollow(started=w._utc_now())


# ---- detection ----

def test_detect_queue_from_merge_queue_rule(monkeypatch, capsys) -> None:
    _install(monkeypatch, Api(rulesets=[QUEUE_RULESET], queue=True))
    s = w.cached_strategy("o/r", "main")
    assert s.kind == "queue"
    assert "11:main" in capsys.readouterr().out


def test_detect_direct_without_rulesets(monkeypatch, capsys) -> None:
    _install(monkeypatch, Api())
    assert w.cached_strategy("o/r", "main").kind == "direct"
    assert "STRATEGY direct" in capsys.readouterr().out


def test_detect_direct_with_ruleset_lacking_queue(monkeypatch) -> None:
    _install(monkeypatch, Api(rulesets=[PLAIN_RULESET]))
    assert w.cached_strategy("o/r", "main").kind == "direct"


def test_ruleset_on_other_branch_does_not_count(monkeypatch) -> None:
    other = {**QUEUE_RULESET, "conditions": {"ref_name": {"include": ["refs/heads/release"]}}}
    _install(monkeypatch, Api(rulesets=[other]))
    assert w.cached_strategy("o/r", "main").kind == "direct"


def test_queue_outside_rulesets_still_queue(monkeypatch) -> None:
    _install(monkeypatch, Api(queue=True))
    assert w.cached_strategy("o/r", "main").kind == "queue"


def test_unreadable_rulesets_refuse_never_direct(monkeypatch, capsys) -> None:
    api = _install(monkeypatch, Api(rulesets_readable=False))
    assert not w._enqueue_step("o/r", 7, _q(), _seen())
    assert api.merges() == []
    assert api.enqueues() == []
    assert "NOT-READY rulesets-unreadable" in capsys.readouterr().out


# ---- queue path ----

def test_queue_strategy_enqueues_on_green(monkeypatch, capsys) -> None:
    api = _install(monkeypatch, Api(rulesets=[QUEUE_RULESET], queue=True))
    assert w._enqueue_step("o/r", 7, _q(), _seen())
    assert len(api.enqueues()) == 1
    assert api.merges() == []
    assert not any("jump" in a for a in api.enqueues()[0])


def test_queue_noop_when_already_queued(monkeypatch) -> None:
    api = _install(monkeypatch, Api(rulesets=[QUEUE_RULESET], queue=True))
    assert not w._enqueue_step("o/r", 7, _q(entry_state="QUEUED", position=1), _seen())
    assert api.enqueues() == []


def test_queue_refuses_pending_check(monkeypatch, capsys) -> None:
    pending = [{"name": "build", "status": "IN_PROGRESS", "conclusion": None, "isRequired": True}]
    api = _install(monkeypatch, Api(rulesets=[QUEUE_RULESET], queue=True,
                                    ready=[_pr(checks=pending)]))
    assert not w._enqueue_step("o/r", 7, _q(), _seen())
    assert api.enqueues() == []
    assert "NOT-READY check-pending" in capsys.readouterr().out


def test_timeout_drop_is_reenqueued_bounded(monkeypatch, capsys) -> None:
    api = _install(monkeypatch, Api(rulesets=[QUEUE_RULESET], queue=True))
    monkeypatch.setattr(w, "MAX_ENQUEUES", 2)
    seen = _seen()
    assert w._enqueue_step("o/r", 7, _q(), seen)
    later = seen.started + timedelta(seconds=5)
    assert w._enqueue_step("o/r", 7, _q(removal_reason="CHECKS_TIMED_OUT", removed_at=later), seen)
    again = _q(removal_reason="CHECKS_TIMED_OUT", removed_at=later + timedelta(seconds=5))
    assert not w._enqueue_step("o/r", 7, again, seen)
    out = capsys.readouterr().out
    assert len(api.enqueues()) == 2
    assert "DROPPED-TIMEOUT" in out
    assert "ENQUEUE-CAP" in out


def test_real_drop_is_not_reenqueued(monkeypatch, capsys) -> None:
    api = _install(monkeypatch, Api(rulesets=[QUEUE_RULESET], queue=True))
    seen = _seen()
    assert w._enqueue_step("o/r", 7, _q(), seen)
    later = seen.started + timedelta(seconds=5)
    assert not w._enqueue_step("o/r", 7, _q(removal_reason="FAILED_CHECKS", removed_at=later), seen)
    assert len(api.enqueues()) == 1
    assert "DROPPED  #7" in capsys.readouterr().out


def test_unmergeable_queue_entry_reported_not_retried(monkeypatch) -> None:
    api = _install(monkeypatch, Api(rulesets=[QUEUE_RULESET], queue=True))
    monkeypatch.setattr(w, "fetch_queue_state", lambda repo, pr: _q(entry_state="UNMERGEABLE",
                                                                   position=1, queue_sha=HEAD))
    monkeypatch.setattr(w, "EXPLICIT_MERGE_WAIT", True)
    seen = _seen()
    assert w._poll_queue("o/r", 7, seen, None) is None
    code, why = w._poll_queue("o/r", 7, seen, None)
    assert code == w.EXIT_UNMERGEABLE
    assert "UNMERGEABLE" in why
    assert api.enqueues() == []


# ---- direct path ----

def test_direct_merges_on_green_and_prints_sha_and_time(monkeypatch, capsys) -> None:
    api = _install(monkeypatch, Api())
    assert w._enqueue_step("o/r", 7, _q(), _seen())
    (call,) = api.merges()
    assert f"sha={HEAD}" in call
    assert "merge_method=squash" in call
    out = capsys.readouterr().out
    assert f"sha={MERGE_SHA}" in out
    assert "mergedAt=2026-11-01T12:00:00Z" in out


def test_direct_uses_only_allowed_method(monkeypatch) -> None:
    repo = {"default_branch": "main", "allow_squash_merge": False, "allow_merge_commit": True,
            "allow_rebase_merge": False}
    api = _install(monkeypatch, Api(repo=repo))
    assert w._enqueue_step("o/r", 7, _q(), _seen())
    assert "merge_method=merge" in api.merges()[0]


def test_direct_history_convention_when_no_squash(monkeypatch) -> None:
    repo = {"default_branch": "main", "allow_squash_merge": False, "allow_merge_commit": True,
            "allow_rebase_merge": True}
    api = _install(monkeypatch, Api(repo=repo))
    assert w._enqueue_step("o/r", 7, _q(), _seen())
    assert "merge_method=merge" in api.merges()[0]  # stub history has merge commits


def test_direct_refuses_squash_on_merge_commit_only_repo(monkeypatch, capsys) -> None:
    repo = {"default_branch": "main", "allow_squash_merge": False, "allow_merge_commit": True,
            "allow_rebase_merge": False}
    api = _install(monkeypatch, Api(repo=repo), method="squash")
    assert not w._enqueue_step("o/r", 7, _q(), _seen())
    assert api.merges() == []
    assert "NOT-READY method-not-allowed" in capsys.readouterr().out


def test_direct_refusals_have_distinct_codes(monkeypatch, capsys) -> None:
    pending = [{"name": "build", "status": "QUEUED", "conclusion": None, "isRequired": True}]
    failing = [{"name": "build", "status": "COMPLETED", "conclusion": "FAILURE",
                "isRequired": True}]
    cases = {"draft": _pr(draft=True), "blocked": _pr(status="BLOCKED"),
             "dirty": _pr(status="DIRTY"), "conflict": _pr(mergeable="CONFLICTING", status="DIRTY"),
             "check-pending": _pr(checks=pending), "check-failing": _pr(checks=failing)}
    for code, ready in cases.items():
        api = _install(monkeypatch, Api(ready=[ready]))
        assert not w._enqueue_step("o/r", 7, _q(), _seen())
        assert api.merges() == []
        assert f"NOT-READY {code} " in capsys.readouterr().out


def test_unreadable_checks_refused(monkeypatch, capsys) -> None:
    truncated = _pr()
    commit = truncated["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]
    commit["statusCheckRollup"]["contexts"]["pageInfo"]["hasNextPage"] = True
    api = _install(monkeypatch, Api(ready=[truncated]))
    assert not w._enqueue_step("o/r", 7, _q(), _seen())
    assert api.merges() == []
    assert "NOT-READY checks-unreadable" in capsys.readouterr().out


def test_direct_head_moved_between_validation_and_merge(monkeypatch, capsys) -> None:
    api = _install(monkeypatch, Api(ready=[_pr(), _pr(head=MOVED)], merge_ok=False))
    assert not w._enqueue_step("o/r", 7, _q(), _seen())
    assert len(api.merges()) == 1  # re-validated once, not retried
    assert "NOT-READY head-moved" in capsys.readouterr().out


def test_direct_rejection_reported_not_retried(monkeypatch, capsys) -> None:
    api = _install(monkeypatch, Api(merge_ok=False))
    assert not w._enqueue_step("o/r", 7, _q(), _seen())
    assert len(api.merges()) == 1
    assert "NOT-READY merge-rejected" in capsys.readouterr().out


def test_land_flag_implies_until_merged() -> None:
    ns = w.parse_args(["7", "--land-when-ready", "--merge-method", "rebase"])
    assert ns.land_when_ready
    assert ns.merge_method == "rebase"
