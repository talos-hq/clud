"""#2873: one worker per goal, stacked dependents, haiku watchers and opus fixers.

A real /grind run on a heavy C++ repository (30-90 min merge queue per PR)
stalled even though each goal took about five minutes to write: dependents
waited for their blocker to MERGE, goals ran in the graph's listed order, and
each goal went through five agents. These tests execute the production
`grind-run.js` in the controlled host `tests/fixtures/grind_workflow_probe.mjs`
with scripted replies. They cover scheduling and data flow, not real agents.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from tests import process

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / "crates/clud-bin/assets/workflows/grind-run.js"
PROBE = ROOT / "tests/fixtures/grind_workflow_probe.mjs"


def _slow(value: Any, ms: int = 80) -> dict[str, Any]:
    return {"delay_ms": ms, "value": value}


def _verdict(verdict: str, **extra: Any) -> dict[str, Any]:
    return {"verdict": verdict, "summary": verdict, **extra}


MERGED = _verdict("merged", exit_code=0)
GREEN = _verdict("green", exit_code=0)
FIXED = {"pushed": True, "summary": "fixed", "head_sha": "f" * 40}


def _head(goal: str) -> str:
    return (goal.lower() * 40)[:40]


def _pushed(goal: str, base: str = "0" * 40) -> dict[str, Any]:
    """A worker that pushed goal `goal`'s PR from `base`."""
    return {
        "status": "pushed",
        "pr_url": f"https://github.com/o/r/pull/{100 + ord(goal[0])}",
        "branch": f"grind/{goal.lower()}",
        "head_sha": _head(goal),
        "base_sha": base,
        "local_ci": "./lint ok; ./test ok",
        "summary": f"pushed {goal}",
    }


def _pr(goal: str) -> str:
    return f"https://github.com/o/r/pull/{100 + ord(goal[0])}"


def _graph(repo: str, goals: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "session_repo": "o/r",
        "session_root": repo,
        "goals": [{"repo": "o/r", "main": "main", "blockers": [], **g} for g in goals],
        "proposed_goals": [],
        "risks": [],
    }


def probe(
    tmp_path: Path,
    script: dict[str, list[Any]],
    goals: tuple[str | dict[str, Any], ...] = ("g1",),
    graph: list[dict[str, Any]] | None = None,
    mode: str = "sequential",
    **extra: Any,
) -> dict:
    repo = str(tmp_path / "proj")
    args: dict[str, Any] = {
        "repo": repo,
        "github_repo": "o/r",
        "main": "main",
        "mode": mode,
        "goals": [g if isinstance(g, dict) else {"id": g, "title": g, "brief": g} for g in goals],
        **extra,
    }
    if graph is not None:
        args["orchestration"] = _graph(repo, graph)
    config = tmp_path / "probe.json"
    config.write_text(json.dumps({"args": args, "script": script}), encoding="utf-8")
    result = process.run(
        ["node", str(PROBE), str(WORKFLOW), str(config)],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def _ok(out: dict) -> dict:
    assert "error" not in out, out
    return out


def _at(out: dict, key: str, label: str) -> int:
    return next(i for i, e in enumerate(out["prompts"]) if e.get(key, "").startswith(label))


def _calls(out: dict, label: str) -> list[dict]:
    return [p for p in out["prompts"] if p.get("label", "").startswith(label)]


def _prompts(out: dict, label: str) -> list[str]:
    return [p["prompt"] for p in _calls(out, label)]


def _goals(out: dict) -> list[dict]:
    result = out["result"]
    return result["goals"] if isinstance(result, dict) else result


def _goal(out: dict, goal: str) -> dict:
    return next(s for s in _goals(out) if s["goal"] == goal)


def _work_order(out: dict) -> list[str]:
    return [p["label"].split(":", 1)[1] for p in _calls(out, "work:")]


# ---- one worker per goal --------------------------------------------------------


def test_one_worker_owns_each_goal_and_n_independent_goals_open_n_prs_back_to_back(
    tmp_path: Path,
) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "work:g1": [_pushed("g1")],
                "work:g2": [_pushed("h2")],
                "work:g3": [_pushed("i3")],
                "chaperone:g1": [_slow(MERGED, 400)],
                "chaperone:g2": [_slow(MERGED, 400)],
                "chaperone:g3": [_slow(MERGED, 400)],
            },
            goals=("g1", "g2", "g3"),
        )
    )
    roles = {p.get("role") for p in out["prompts"] if "role" in p}
    assert roles == {"grind-worker", "grind-chaperone"}, roles
    # The producer never waits on CI or the queue: every PR is open before the
    # first chaperone sees a merge.
    first_merge = _at(out, "done", "chaperone:g1")
    assert _at(out, "label", "work:g3") < first_merge
    assert _at(out, "done", "work:g3") < first_merge
    worker = _prompts(out, "work:g1")[0]
    assert "never wait for CI or the merge queue and never enqueue" in worker
    assert "Follow your built-in /grind-worker procedure" in worker
    assert all(_goal(out, g)["merged"] is True for g in ("g1", "g2", "g3"))


def test_the_staged_chain_stays_available_as_an_opt_in_per_goal(tmp_path: Path) -> None:
    risky = {"id": "g2", "title": "g2", "brief": "g2", "pipeline": "staged"}
    out = _ok(probe(tmp_path, {}, goals=("g1", risky)))
    assert _calls(out, "work:g1")
    assert not _calls(out, "plan:g1")
    assert _calls(out, "plan:g2")
    assert _calls(out, "review:g2")
    assert not _calls(out, "work:g2")
    staged = _ok(probe(tmp_path, {}, pipeline="staged"))
    assert _calls(staged, "plan:g1")
    assert not _calls(staged, "work:")


def test_an_unknown_pipeline_is_rejected_before_any_agent(tmp_path: Path) -> None:
    out = probe(tmp_path, {}, pipeline="fast")
    assert 'pipeline must be "worker" or "staged"' in out["error"]
    assert not out["prompts"]


def test_a_worker_covered_goal_is_skipped_and_a_failed_one_is_parked(tmp_path: Path) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "work:g1": [{"status": "covered", "covered_by": "#77", "summary": "dup"}],
                "work:g2": [
                    {
                        "status": "failed",
                        "summary": "red",
                        "failure_log": "lint red",
                        "files_touched": ["src/x.rs"],
                        "branch": "grind/g2",
                    }
                ],
                "park:g2": [
                    {"parked": True, "clean": True, "branch": "wip/grind-g2", "summary": "parked"}
                ],
            },
            goals=("g1", "g2"),
        )
    )
    assert _goal(out, "g1")["status"] == "covered"
    assert _goal(out, "g1")["note"] == "covered by #77"
    assert "src/x.rs" in _prompts(out, "park:g2")[0]
    assert _goal(out, "g2")["parked"] == "wip/grind-g2"
    assert not _calls(out, "chaperone:")


# ---- local CI is the push gate ---------------------------------------------------


def test_the_test_script_runs_before_every_push_not_only_lint(tmp_path: Path) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "chaperone:g1": [_verdict("needs_fix", failure_log="red"), MERGED],
                "chaperone-fix:g1": [FIXED],
            },
            scripts={"lint": "./lint", "test": "./test --integration"},
        )
    )
    for label in ("work:g1", "chaperone-fix:g1"):
        prompt = _prompts(out, label)[0]
        assert "./lint" in prompt, label
        assert "./test --integration" in prompt, label
        assert "before every push" in prompt, label


# ---- pushed-gate stacking --------------------------------------------------------


def _chain_graph() -> list[dict[str, Any]]:
    return [
        {"id": "A"},
        {"id": "B", "blockers": [{"goal": "A", "when": "merged", "edge": "file_overlap"}]},
        # No edge: a merged blocker defaults to file_overlap, which stacks.
        {"id": "C", "blockers": [{"goal": "B", "when": "merged"}]},
    ]


def test_a_file_overlap_chain_opens_three_stacked_prs_before_the_first_merges(
    tmp_path: Path,
) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "work:A": [_pushed("A")],
                "work:B": [_pushed("B", base=_head("A"))],
                "work:C": [_pushed("C", base=_head("B"))],
                "chaperone:A": [_slow(MERGED, 500)],
                "chaperone:B": [GREEN, MERGED],
                "chaperone:C": [GREEN, MERGED],
                "chaperone-fix:B": [FIXED],
                "chaperone-fix:C": [FIXED],
            },
            goals=("A", "B", "C"),
            graph=_chain_graph(),
        )
    )
    a_merged = _at(out, "done", "chaperone:A")
    # All three PRs exist before A merges: nobody waited on the merge queue.
    assert _at(out, "done", "work:C") < a_merged
    b = _prompts(out, "work:B")[0]
    assert "STACKED on goal A's pushed PR" in b
    assert "origin/grind/a" in b
    assert "gh pr create --repo o/r --base main" in b
    assert "STACKED on goal B's pushed PR" in _prompts(out, "work:C")[0]
    # While its parent is open a stacked PR's watcher never enqueues it.
    first_b_watch = _prompts(out, "chaperone:B")[0]
    assert "STACKED on" in first_b_watch
    assert "never enqueue a stacked PR" in first_b_watch
    assert "github/pr_merge.py" not in first_b_watch
    assert _goal(out, "B")["stacked_on"] == "A"
    assert all(_goal(out, g)["merged"] is True for g in ("A", "B", "C"))


def test_a_parent_merge_wakes_the_child_which_is_rebased_onto_main_then_enqueued(
    tmp_path: Path,
) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "work:A": [_pushed("A")],
                "work:B": [_pushed("B", base=_head("A"))],
                "chaperone:A": [_slow(MERGED, 300)],
                "chaperone:B": [GREEN, MERGED],
                "chaperone-fix:B": [FIXED],
            },
            goals=("A", "B"),
            graph=_chain_graph()[:2],
        )
    )
    rebase = _calls(out, "chaperone-fix:B")
    assert len(rebase) == 1
    fix = rebase[0]["prompt"]
    assert "parent_merged_needs_rebase" in fix
    assert f"rebase --onto origin/main {_head('A')}" in fix
    # The rebase happens only after the parent merged, on the large model.
    assert _at(out, "done", "chaperone:A") < _at(out, "label", "chaperone-fix:B")
    assert rebase[0]["model"] == "opus"
    # After the rebase it is an ordinary PR: the next watch enqueues it pinned.
    after = _prompts(out, "chaperone:B")[1]
    assert "github/pr_merge.py" in after
    assert "--match-head-commit" in after
    assert "STACKED on" not in after
    journal = "\n".join(out["logs"])
    assert "goal B: parent_merged_needs_rebase" in journal
    assert "goal B: watcher -> fixer: parent merged" in journal
    assert "goal B: fixer -> watcher" in journal


def test_a_stack_parent_that_ends_unmerged_sends_the_child_to_a_parent_failed_fixer(
    tmp_path: Path,
) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "work:A": [_pushed("A")],
                "work:B": [_pushed("B", base=_head("A"))],
                "chaperone:A": [_slow(_verdict("stuck", failure_log="closed"), 200)],
                "chaperone:B": [GREEN, MERGED],
                "chaperone-fix:B": [FIXED],
            },
            goals=("A", "B"),
            graph=_chain_graph()[:2],
        )
    )
    fix = _prompts(out, "chaperone-fix:B")[0]
    assert "parent_failed" in fix
    assert f"rebase --onto origin/main {_head('A')}" in fix
    assert _goal(out, "B")["merged"] is True


def test_a_file_overlap_parent_without_a_pr_lets_the_child_branch_from_main(
    tmp_path: Path,
) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "work:A": [{"status": "failed", "summary": "cannot", "failure_log": "x"}],
                "work:B": [_pushed("B")],
            },
            goals=("A", "B"),
            graph=_chain_graph()[:2],
        )
    )
    b = _prompts(out, "work:B")[0]
    assert "STACKED" not in b
    assert "pushed no PR: branch from origin/main" in b
    assert _goal(out, "B")["merged"] is True


def test_a_semantic_edge_reads_the_parent_diff_and_blocks_when_the_parent_has_no_pr(
    tmp_path: Path,
) -> None:
    graph = [
        {"id": "A"},
        {"id": "B", "blockers": [{"goal": "A", "when": "merged", "edge": "semantic"}]},
    ]
    stacked = _ok(probe(tmp_path, {"work:A": [_pushed("A")]}, goals=("A", "B"), graph=graph))
    assert "gh pr diff " + _pr("A") in _prompts(stacked, "work:B")[0]
    failed = _ok(
        probe(
            tmp_path,
            {"work:A": [{"status": "failed", "summary": "cannot", "failure_log": "x"}]},
            goals=("A", "B"),
            graph=graph,
        )
    )
    assert not _calls(failed, "work:B")
    assert _goal(failed, "B")["blocked"] is True


def test_a_merge_edge_still_waits_for_the_parents_merge(tmp_path: Path) -> None:
    graph = [
        {"id": "A"},
        {"id": "B", "blockers": [{"goal": "A", "when": "merged", "edge": "merge"}]},
    ]
    out = _ok(
        probe(
            tmp_path,
            {"work:A": [_pushed("A")], "chaperone:A": [_slow(MERGED, 200)]},
            goals=("A", "B"),
            graph=graph,
        )
    )
    assert _at(out, "done", "chaperone:A") < _at(out, "label", "work:B")
    assert "STACKED" not in _prompts(out, "work:B")[0]


def test_a_goal_stacks_on_its_nearest_parent_not_on_an_ancestor_inside_it(
    tmp_path: Path,
) -> None:
    graph = [
        {"id": "A"},
        {"id": "B", "blockers": [{"goal": "A", "when": "merged"}]},
        {"id": "C", "blockers": [{"goal": "A", "when": "merged"}, {"goal": "B", "when": "merged"}]},
    ]
    out = _ok(
        probe(
            tmp_path,
            {"work:A": [_pushed("A")], "work:B": [_pushed("B")], "work:C": [_pushed("C")]},
            goals=("A", "B", "C"),
            graph=graph,
        )
    )
    assert "STACKED on goal B's pushed PR" in _prompts(out, "work:C")[0]


# ---- critical-path order ---------------------------------------------------------


def test_the_producer_queue_follows_the_critical_path_not_the_listed_order(
    tmp_path: Path,
) -> None:
    # Listed: X, Y, A, B, C, Z. The chain A -> B -> C is the longest; Z needs
    # X merged, so it goes last.
    graph = [
        {"id": "X"},
        {"id": "Y"},
        {"id": "A"},
        {"id": "B", "blockers": [{"goal": "A", "when": "merged"}]},
        {"id": "C", "blockers": [{"goal": "B", "when": "merged"}]},
        {"id": "Z", "blockers": [{"goal": "X", "when": "merged", "edge": "merge"}]},
    ]
    script = {f"work:{g}": [_pushed(g)] for g in "XYABCZ"}
    out = _ok(probe(tmp_path, script, goals=tuple("XYABCZ"), graph=graph))
    assert _work_order(out) == ["A", "X", "B", "Y", "C", "Z"]


# ---- human gates -----------------------------------------------------------------


def test_a_human_gated_goal_and_its_dependents_are_reported_never_scheduled(
    tmp_path: Path,
) -> None:
    graph = [
        {"id": "H", "gate": "human", "gate_reason": "maintainer decision"},
        {"id": "D", "blockers": [{"goal": "H", "when": "merged"}]},
        {"id": "E"},
    ]
    out = _ok(probe(tmp_path, {"work:E": [_pushed("E")]}, goals=("H", "D", "E"), graph=graph))
    labels = [p.get("label", "") for p in out["prompts"]]
    assert not [label for label in labels if label.endswith(":H") or ":H#" in label]
    assert not [label for label in labels if label.endswith(":D") or ":D#" in label]
    h = _goal(out, "H")
    assert h["status"] == "waiting"
    assert h["gate"] == "human"
    assert "maintainer decision" in h["note"]
    assert "blocked by human-gated goal H" in _goal(out, "D")["note"]
    assert _goal(out, "E")["merged"] is True


def test_human_gate_blocks_a_selected_goal_through_unselected_blockers(tmp_path: Path) -> None:
    graph = [
        {"id": "leaf", "blockers": [{"goal": "middle", "when": "merged"}]},
        {"id": "middle", "blockers": [{"goal": "human", "when": "merged"}]},
        {"id": "human", "gate": "human", "gate_reason": "needs hardware"},
    ]
    out = _ok(probe(tmp_path, {}, goals=("leaf",), graph=graph))
    assert not out["prompts"]
    assert _goal(out, "leaf")["status"] == "waiting"
    assert "blocked by human-gated goal middle" in _goal(out, "leaf")["note"]


def test_a_run_of_only_human_gated_goals_starts_no_agent(tmp_path: Path) -> None:
    out = _ok(probe(tmp_path, {}, goals=("H",), graph=[{"id": "H", "gate": "human"}]))
    assert not out["prompts"]
    assert _goal(out, "H")["status"] == "waiting"


@pytest.mark.parametrize(
    ("goal", "expect"),
    [
        (
            {"id": "B", "blockers": [{"goal": "maintainer decision", "when": "merged"}]},
            ['unknown goal "maintainer decision" in blockers of B', 'gate: "human"', "(A, B)"],
        ),
        (
            {"id": "B", "blockers": [{"goal": "all", "when": "merged"}]},
            ['unknown goal "all"', "list several goals as several blockers"],
        ),
        (
            {"id": "B", "checkout": "sister", "blockers": []},
            ['unknown checkout "sister"', '"local"', '"worktree"'],
        ),
        (
            {"id": "B", "gate": "hardware", "blockers": []},
            ['unknown gate "hardware"', 'the only gate is "human"'],
        ),
        (
            {"id": "B", "blockers": [{"goal": "A", "when": "merged", "edge": "touches"}]},
            ['unknown edge "touches"', "file_overlap", "semantic", "merge"],
        ),
    ],
)
def test_the_validator_rejects_a_bad_graph_with_an_actionable_error(
    tmp_path: Path, goal: dict[str, Any], expect: list[str]
) -> None:
    out = probe(tmp_path, {}, goals=("A", "B"), graph=[{"id": "A"}, goal])
    assert "error" in out, out
    for text in expect:
        assert text in out["error"], (text, out["error"])
    assert not out["prompts"]


# ---- chaperone: haiku watcher, opus fixer ---------------------------------------


def test_watchers_run_on_haiku_and_fixers_on_opus_by_default(tmp_path: Path) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "chaperone:g1": [_verdict("needs_fix", failure_log="red"), MERGED],
                "chaperone-fix:g1": [FIXED],
            },
        )
    )
    watches = _calls(out, "chaperone:g1")
    assert [w["model"] for w in watches] == ["haiku", "haiku"]
    assert [f["model"] for f in _calls(out, "chaperone-fix:g1")] == ["opus"]
    labels = [p["label"] for p in out["prompts"] if "label" in p]
    seq = [
        label.split(":")[0]
        for label in labels
        if label.startswith(("chaperone:", "chaperone-fix:"))
    ]
    assert seq == ["chaperone", "chaperone-fix", "chaperone"]
    journal = "\n".join(out["logs"])
    assert "goal g1: watcher -> fixer: fix round 1" in journal
    assert "goal g1: fixer -> watcher: pushed " + "f" * 40 in journal


def test_watcher_and_fixer_models_are_configurable(tmp_path: Path) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "chaperone:g1": [_verdict("needs_fix", failure_log="red"), MERGED],
                "chaperone-fix:g1": [FIXED],
            },
            models={"watcher": "sonnet", "fixer": "fable", "worker": "opus"},
        )
    )
    assert _calls(out, "chaperone:g1")[0]["model"] == "sonnet"
    assert _calls(out, "chaperone-fix:g1")[0]["model"] == "fable"
    assert _calls(out, "work:g1")[0]["model"] == "opus"


def test_a_red_stacked_pr_whose_parent_already_merged_is_rebased_before_any_fix(
    tmp_path: Path,
) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "work:A": [_pushed("A")],
                "work:B": [_pushed("B", base=_head("A"))],
                # A merges while B's first watch is still running.
                "chaperone:A": [_slow(MERGED, 150)],
                "chaperone:B": [
                    _slow(_verdict("needs_fix", failure_log="red"), 400),
                    MERGED,
                ],
                "chaperone-fix:B": [FIXED],
            },
            goals=("A", "B"),
            graph=_chain_graph()[:2],
        )
    )
    fixes = _prompts(out, "chaperone-fix:B")
    assert len(fixes) == 1
    assert "parent_merged_needs_rebase" in fixes[0]
    assert "origin/grind/a" not in fixes[0]
    assert _goal(out, "B")["merged"] is True


# ---- watcher waiting and escalation dedupe (talos-hq/clud#2877, #2878) --------


def test_a_waiting_watch_is_watched_again_and_never_escalated(tmp_path: Path) -> None:
    waiting = _verdict("waiting", exit_code=4)
    out = _ok(probe(tmp_path, {"chaperone:A": [waiting, waiting, MERGED]}, goals=("A",)))
    assert len(_prompts(out, "chaperone:A")) == 3
    assert _prompts(out, "chaperone-fix:A") == []
    assert _goal(out, "A")["merged"] is True
    assert "return waiting" in _prompts(out, "chaperone:A")[0]


def test_endless_waiting_hands_the_pr_back_without_a_fixer(tmp_path: Path) -> None:
    waiting = _verdict("waiting", exit_code=4)
    out = _ok(probe(tmp_path, {"chaperone:A": [waiting] * 10}, goals=("A",)))
    # Seven chain watches (the seventh exceeds the six waits), then Finish's one
    # last bounded watch; none of them reaches a fixer.
    assert len(_prompts(out, "chaperone:A")) == 8
    assert _prompts(out, "chaperone-fix:A") == []
    goal = _goal(out, "A")
    assert goal["merged"] is False
    assert goal["status"] == "waiting"
    assert "still pending" in goal["note"]


def test_the_same_problem_on_the_same_head_goes_to_a_fixer_once(tmp_path: Path) -> None:
    review = _verdict("needs_fix", failure_log="changes requested",
                      head_sha="e" * 40, evidence_id="review-9")
    out = _ok(
        probe(
            tmp_path,
            {
                "chaperone:A": [review, review, MERGED],
                # The fixer answered the review without a new commit.
                "chaperone-fix:A": [{**FIXED, "head_sha": "e" * 40}],
            },
            goals=("A",),
        )
    )
    assert len(_prompts(out, "chaperone-fix:A")) == 1
    assert _goal(out, "A")["merged"] is True
    assert any("already went to a fixer" in line for line in out["logs"])


def test_a_new_failure_on_a_new_head_still_escalates(tmp_path: Path) -> None:
    out = _ok(
        probe(
            tmp_path,
            {
                "chaperone:A": [
                    _verdict(
                        "needs_fix", failure_log="red", head_sha="e" * 40, evidence_id="run-1"
                    ),
                    _verdict(
                        "needs_fix", failure_log="red", head_sha="f" * 40, evidence_id="run-2"
                    ),
                    MERGED,
                ],
                "chaperone-fix:A": [FIXED, FIXED],
            },
            goals=("A",),
        )
    )
    assert len(_prompts(out, "chaperone-fix:A")) == 2
