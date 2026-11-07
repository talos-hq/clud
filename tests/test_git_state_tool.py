"""Unit coverage for the bundled `git/state.py` tool (#1339 item 7)."""

from __future__ import annotations

import sys
from pathlib import Path

TOOL_DIR = Path(__file__).resolve().parents[1] / "crates" / "clud-bin" / "assets" / "tools" / "git"


def _load():
    if str(TOOL_DIR) not in sys.path:
        sys.path.insert(0, str(TOOL_DIR))
    import state

    return state


state = _load()

STATUS = """# branch.oid 0123456789abcdef0123456789abcdef01234567
# branch.head feat/x
# branch.upstream origin/feat/x
# branch.ab +2 -1
1 .M N... 100644 100644 100644 aaaa bbbb src/main.rs
2 R. N... 100644 100644 100644 aaaa bbbb R100 new name.rs\told.rs
u UU N... 100644 100644 100644 100644 aaaa bbbb cccc conflict.rs
? notes.txt
"""


def test_status_parses_branch_upstream_and_every_entry_kind() -> None:
    parsed = state.parse_status(STATUS)
    assert parsed["branch"] == "feat/x"
    assert parsed["head"] == "0123456789ab"
    assert (parsed["upstream"], parsed["ahead"], parsed["behind"]) == ("origin/feat/x", 2, 1)
    assert parsed["changes"] == [
        {"xy": ".M", "path": "src/main.rs"},
        {"xy": "R.", "path": "new name.rs"},
        {"xy": "UU", "path": "conflict.rs"},
        {"xy": "??", "path": "notes.txt"},
    ]


def test_a_detached_head_without_upstream() -> None:
    parsed = state.parse_status("# branch.oid abcdefabcdefabcdef\n# branch.head (detached)\n")
    assert parsed["branch"] is None
    assert parsed["upstream"] is None
    parsed.update(commits=[], stashes=[], worktrees=[])
    assert "branch: (detached at abcdefabcdef)" in state.render(parsed)


def test_worktrees_with_a_stash_render_together() -> None:
    trees = state.parse_worktrees(
        "worktree /r\nHEAD aa\nbranch refs/heads/main\n\nworktree /r/wt\nHEAD bb\ndetached\n"
    )
    assert trees == [{"path": "/r", "branch": "main"}, {"path": "/r/wt", "branch": None}]
    parsed = state.parse_status(STATUS)
    parsed.update(commits=["abc fix"], stashes=["stash@{0} WIP"], worktrees=trees)
    text = state.render(parsed)
    assert "upstream: origin/feat/x ahead 2 behind 1" in text
    assert "stashes: 1" in text
    assert "/r/wt [detached]" in text


# --- bounded output + fetch (#2233) -----------------------------------------
#
# This page is re-sent on every later request of a session, so the default
# render is bounded while the headers keep the true totals. A list that fits
# stays byte-identical to the unbounded form, and `--all` still prints it all.


def _state(**overrides) -> dict:
    parsed = state.parse_status(STATUS)
    parsed.update(
        commits=["abc fix", "def feat", "012 chore", "345 docs"],
        stashes=[f"stash@{{{i}}} WIP {i}" for i in range(8)],
        worktrees=[
            {"path": f"/r/wt{i}", "branch": f"b{i}" if i % 2 == 0 else None} for i in range(7)
        ],
    )
    parsed.update(overrides)
    return parsed


def test_capped_list_names_the_continuation_and_keeps_the_total() -> None:
    parsed = _state()
    parsed["limits"] = {"files": 20, "stashes": 5, "worktrees": 5}
    text = state.render(parsed)
    assert "stashes: 8" in text
    assert "  +3 more (--stashes N or --all)" in text
    assert "worktrees: 7" in text
    assert "  +2 more (--worktrees N or --all)" in text
    # The visible rows are the first ones, in git's own order.
    assert "stash@{0} WIP 0" in text
    assert "stash@{5} WIP 5" not in text


def test_a_list_that_fits_is_printed_whole_and_unchanged() -> None:
    parsed = _state(stashes=["stash@{0} only"], worktrees=[{"path": "/r", "branch": None}])
    parsed["limits"] = {"files": 20, "stashes": 5, "worktrees": 5}
    text = state.render(parsed)
    assert "stashes: 1\n  stash@{0} only\n" in text
    assert "more" not in text


def test_files_cap_trims_an_untracked_flood() -> None:
    flood = state.parse_status(
        "# branch.head main\n" + "".join(f"? out/{i}.log\n" for i in range(40))
    )
    flood.update(commits=[], stashes=[], worktrees=[])
    flood["limits"] = {"files": 20}
    text = state.render(flood)
    assert "changes: 40" in text
    assert "  +20 more (--files N or --all)" in text


def test_all_lifts_the_caps_but_never_unbounds_the_commit_range() -> None:
    seen: list[tuple[str, ...]] = []

    def fake_git(*args: str) -> tuple[int, str]:
        seen.append(args)
        if args[0] == "status":
            return 0, STATUS
        if args[0] == "log":
            return 0, "abc fix\ndef feat\n012 chore\n"
        if args[0] == "stash":
            return 0, "stash@{0} a\nstash@{1} b\nstash@{2} c\nstash@{3} d\nstash@{4} e\n"
        if args[0] == "worktree":
            return 0, "worktree /r\nHEAD aa\nbranch refs/heads/main\n"
        return 0, ""

    original = state.git
    state.git = fake_git
    try:
        assert state.main(["--all", "--json"]) == 0
    finally:
        state.git = original
    log_call = next(a for a in seen if a[0] == "log")
    assert log_call[1] == "-3", f"--all must not ask git for the whole history: {log_call}"


def test_the_page_never_touches_the_network(monkeypatch) -> None:
    """`tools::bundled_includes_git_state` pins that state.py never fetches.

    The check is a whole-call invariant, not a flag: any `fetch`/`pull`/`ls-remote`
    would make a state probe depend on the network, which is why the routed
    instruction sends the fetch to git itself.
    """
    calls: list[tuple[str, ...]] = []

    def fake_git(*args: str) -> tuple[int, str]:
        calls.append(args)
        return {
            "status": (0, STATUS),
            "log": (0, "abc fix\n"),
            "stash": (0, ""),
            "worktree": (0, ""),
        }.get(args[0], (0, ""))

    monkeypatch.setattr(state, "git", fake_git)
    for argv in ([], ["--all"], ["--commits", "0"], ["--json"]):
        calls.clear()
        assert state.main(argv) == 0
        assert not [c for c in calls if c[0] in {"fetch", "pull", "ls-remote", "remote"}], (
            f"state.py talked to the network for {argv}: {calls}"
        )


def test_a_zero_commit_request_skips_git_log(monkeypatch) -> None:
    calls: list[tuple[str, ...]] = []

    def fake_git(*args: str) -> tuple[int, str]:
        calls.append(args)
        return (0, STATUS) if args[0] == "status" else (0, "")

    monkeypatch.setattr(state, "git", fake_git)
    assert state.main(["--commits", "0"]) == 0
    assert not any(c[0] == "log" for c in calls)
