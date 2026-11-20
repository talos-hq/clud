"""DD-230: after a merge, pr_merge_watch deletes the head branch on GitHub
when the repository's own delete_branch_on_merge is off, and names the local
retire command. Every gh call is stubbed; nothing reaches GitHub."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / "crates/clud-bin/assets/tools/github/pr_merge_watch.py"
)


@pytest.fixture
def watcher(monkeypatch):
    name = "clud_test_pr_merge_watch_head_branch"
    spec = importlib.util.spec_from_file_location(name, SCRIPT)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    monkeypatch.delenv("CLUD_PR_MERGE_WATCH_NO_BRANCH_CLEANUP", raising=False)
    try:
        yield module
    finally:
        sys.modules.pop(name, None)


VIEW = {"headRefName": "feat/x", "headRefOid": "abc123", "isCrossRepository": False,
        "labels": []}


def decide(watcher, view=None, setting=False, ref="abc123", stacked=(), keep=False):
    return watcher.head_branch_decision(view or VIEW, setting, ref, list(stacked), keep)


def test_a_branch_at_the_merged_head_is_deleted_when_github_will_not(watcher) -> None:
    assert decide(watcher) == ("delete", "merged at its exact head")


@pytest.mark.parametrize(
    ("kwargs", "expected"),
    [
        ({"keep": True}, "keep"),
        ({"view": {**VIEW, "labels": [{"name": "keep-branch"}]}}, "keep"),
        ({"view": {**VIEW, "isCrossRepository": True}}, "keep"),
        ({"ref": None}, "gone"),
        ({"setting": True}, "keep"),
        ({"ref": "fff999"}, "keep"),
        ({"stacked": [12]}, "keep"),
    ],
)
def test_every_doubt_keeps_the_branch(watcher, kwargs, expected) -> None:
    assert decide(watcher, **kwargs)[0] == expected


def test_work_pushed_after_the_merge_is_named(watcher) -> None:
    assert "unmerged work" in decide(watcher, ref="fff999")[1]


def _stub(watcher, monkeypatch, *, setting, ref_sha, stacked=()):
    calls: list[tuple[str, ...]] = []

    def gh_json(*args):
        if args[:2] == ("pr", "view"):
            return VIEW
        if args == ("api", "repos/o/r"):
            return {"delete_branch_on_merge": setting}
        if args[0] == "api" and "/git/ref/heads/" in args[1]:
            return {"object": {"sha": ref_sha}} if ref_sha else None
        if args[:2] == ("pr", "list"):
            return [{"number": n} for n in stacked]
        raise AssertionError(args)

    def gh(*args, **_kw):
        calls.append(args)
        return watcher.GhResult(0, "", "")

    monkeypatch.setattr(watcher, "gh_json", gh_json)
    monkeypatch.setattr(watcher, "gh", gh)
    return calls


def test_after_merge_deletes_and_warns_about_the_setting(watcher, monkeypatch, capsys) -> None:
    calls = _stub(watcher, monkeypatch, setting=False, ref_sha="abc123")
    watcher.after_merge_branch("o/r", 7)
    assert calls == [("api", "-X", "DELETE", "repos/o/r/git/refs/heads/feat/x")]
    out = capsys.readouterr().out
    assert "HEAD-BRANCH  #7 feat/x deleted" in out
    assert "delete_branch_on_merge off" in out
    assert "gc branches --retire" in out


def test_after_merge_leaves_it_to_github_when_the_setting_is_on(
    watcher, monkeypatch, capsys
) -> None:
    calls = _stub(watcher, monkeypatch, setting=True, ref_sha="abc123")
    watcher.after_merge_branch("o/r", 7)
    assert calls == []
    out = capsys.readouterr().out
    assert "WARN" not in out
    assert "gc branches --retire" in out


def test_the_test_guard_disables_the_step(watcher, monkeypatch, capsys) -> None:
    monkeypatch.setenv("CLUD_PR_MERGE_WATCH_NO_BRANCH_CLEANUP", "1")
    monkeypatch.setattr(watcher, "gh_json", lambda *a: pytest.fail("must not call gh"))
    watcher.after_merge_branch("o/r", 7)
    assert capsys.readouterr().out == ""
