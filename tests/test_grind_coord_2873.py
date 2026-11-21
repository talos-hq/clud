"""#2873: the /grind coordinator template and its workflow linter.

`grind-coord.js` is the template the /grind router tailors per run: one
worker at a time in the central checkout, a Sonnet chaperone per PR that only
watches and reports, fixups scheduled back through the central checkout ahead
of new tasks, and an in-flight cap for backpressure. `grind/lint_workflow.py`
must pass a tailored copy before launch. These tests run the real template
and the linter's simulated runs (mock agents, scripted chaperone events) and
prove the linter catches each failure mode of the FastLED run on
2026-11-20: a watcher storm, builds racing in one checkout, and a producer
that kept starting work while problems piled up.
"""

from __future__ import annotations

import importlib.util
import json
import shutil
import sys
from pathlib import Path
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[1]
TEMPLATE = ROOT / "crates/clud-bin/assets/workflows/grind-coord.js"
LINTER = ROOT / "crates/clud-bin/assets/tools/grind/lint_workflow.py"

pytestmark = pytest.mark.skipif(
    shutil.which("node") is None, reason="the simulated run needs Node.js"
)


def _linter():
    spec = importlib.util.spec_from_file_location("grind_lint_workflow", LINTER)
    assert spec
    assert spec.loader
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod
    spec.loader.exec_module(mod)
    return mod


LW = _linter()


def _args(**extra: Any) -> dict[str, Any]:
    args: dict[str, Any] = {
        "repo": "/work/fastled",
        "github_repo": "FastLED/FastLED",
        "main": "master",
        "gate": {"commands": ["./lint", "./test --cpp"]},
        "policy": {"maxInFlight": 3, "maxFixups": 2, "maxWatches": 6},
        "tasks": [
            {"id": "5327", "issue": 5327},
            {"id": "5325", "issue": 5325, "after": ["5327"]},
            {"id": "5326", "issue": 5326, "after": ["5325"]},
            {"id": "5324", "issue": 5324},
            {"id": "5330", "issue": 5330},
            {"id": "5332", "issue": 5332},
            {"id": "5321", "issue": 5321, "gate": "human", "gate_reason": "needs hardware"},
            {"id": "5333", "issue": 5333, "after": ["5321"]},
        ],
    }
    args.update(extra)
    return args


def _lint(script: Path, args: dict[str, Any], only: list[str] | None = None) -> list[Any]:
    findings, ran = LW.lint(script, args, only)
    assert ran
    return findings


def _sim(tmp_path: Path, args: dict[str, Any], scenario: str = "happy") -> dict[str, Any]:
    pol = LW.Policy.of(args)
    sc = next(s for s in LW.scenarios(args, pol) if s.name == scenario)
    return LW.simulate(shutil.which("node"), TEMPLATE, args, sc, tmp_path)


def _labels(run: dict[str, Any]) -> list[str]:
    return [e["label"] for e in run["trace"] if e["ev"] == "start"]


def test_template_passes_every_simulated_scenario() -> None:
    findings = _lint(TEMPLATE, _args())
    assert findings == [], [f.text() for f in findings]


def test_template_passes_with_a_single_slot_and_no_fixups() -> None:
    findings = _lint(TEMPLATE, _args(policy={"maxInFlight": 1, "maxFixups": 0, "maxWatches": 1}))
    assert findings == [], [f.text() for f in findings]


MUTANTS = {
    # The 2026-11-20 storm: every interim watcher return started a fixer.
    "watch_storm": (
        "  if (pr.get(task).watches >= POLICY.maxWatches) "
        "return settle(task, verdict === 'queued' ? 'left_in_queue' : 'stuck')\n",
        "",
        "GS106",
    ),
    # Two builds in one checkout crashed zccache and timed out the lock.
    "parallel_workers": (
        "if (next && inFlight < POLICY.maxInFlight) { await runTask(next); continue }",
        "if (next && inFlight < POLICY.maxInFlight) { runTask(next); await nextEvent(); continue }",
        "GS102",
    ),
    "no_in_flight_cap": ("next && inFlight < POLICY.maxInFlight", "next", "GS103"),
    "fixups_last": (
        "  if (fixQueue.length) { await runFixup(fixQueue.shift()); continue }",
        "  if (fixQueue.length && !ready().length) { await runFixup(fixQueue.shift()); continue }",
        "GS104",
    ),
    "fixups_dropped": (
        "  if (fixQueue.length) { await runFixup(fixQueue.shift()); continue }",
        "",
        "GS104",
    ),
    "stale_refixed": (
        "if ((r.head_sha && r.head_sha !== p.head) || p.fixed.has(key)) {",
        "if (false) {",
        "GS113",
    ),
    "no_cleanup": ("  if (dirty && !halted) { await cleanup(); continue }", "", "GS105"),
    "chaperone_on_session_model": (
        "model: POLICY.chaperoneModel, schema: WATCH",
        "schema: WATCH",
        "GS107",
    ),
    "background_wait": (
        "never run_in_background: you get no completion notification",
        "run it with run_in_background",
        "GS107",
    ),
    "worktree_worker": (
        "{ label, phase: 'Produce', agentType: 'grind-worker'",
        "{ label, phase: 'Produce', isolation: 'worktree', agentType: 'grind-worker'",
        "GW003",
    ),
    "ignores_dependencies": ("t.after.every(d => status.get(d) === 'merged')", "true", "GS108"),
    "nondeterministic": (
        "const A = args || {}",
        "const A = args || {}; const T0 = Date.now()",
        "GW002",
    ),
    # The regression DD-236 records: a PR left in the queue re-marked unstarted.
    "requeue_left_in_queue": (
        "verdict === 'queued' ? 'left_in_queue' : 'stuck'",
        "verdict === 'queued' ? 'queued' : 'stuck'",
        "GS112",
    ),
    "same_head_refixed": ("|| p.fixed.has(key)) {", ") {", "GS113"),
    "covered_open_counts_as_merged": (
        "if (r.status === 'covered' && r.covered_state !== 'merged')",
        "if (false)",
        "GS108",
    ),
    "fix_settles_without_fixing": (
        "    } else if (p.fixups >= POLICY.maxFixups) {",
        "    } else if (true) {",
        "GS104",
    ),
    "unwatched_fix": (
        "p.head = r.head_sha; rewatch(f.task) }",
        "p.head = r.head_sha; settle(f.task, 'stuck') }",
        "GS109",
    ),
    "drops_report": (
        "  report.push({ task: taskId, status: final, "
        "pr: pr.get(taskId) && pr.get(taskId).url })\n",
        "",
        "GS110",
    ),
    "lost_wakeup": ("returned nothing' } }); notify() })", "returned nothing' } }) })", "GS111"),
    "no_meta": ("export const meta = {", "const meta = {", "GW001"),
    "unknown_role": ("agentType: 'grind-chaperone'", "agentType: 'general-purpose'", "GW004"),
    "nested_workflow": (
        "phase('Finish')\n",
        "phase('Finish')\nawait workflow('grind-run')\n",
        "GW005",
    ),
}


@pytest.mark.parametrize("name", sorted(MUTANTS))
def test_linter_catches_each_broken_coordinator(tmp_path: Path, name: str) -> None:
    old, new, rule = MUTANTS[name]
    src = TEMPLATE.read_text(encoding="utf-8")
    assert old in src, f"mutant {name} no longer applies; update it with the template"
    script = tmp_path / f"{name}.js"
    script.write_text(src.replace(old, new), encoding="utf-8")
    rules = {f.rule for f in _lint(script, _args())}
    assert rule in rules, f"{name}: expected {rule}, got {sorted(rules)}"


def test_fixups_run_in_the_central_checkout_before_new_tasks(tmp_path: Path) -> None:
    args = _args()
    run = _sim(tmp_path, args, "fix_while_producing")
    assert "error" not in run, run["error"]
    labels = _labels(run)
    # Every runnable task went red once, and each got exactly one fixup, all
    # through the central checkout (one worker-type agent at a time).
    runnable = [str(t["id"]) for t in LW.runnable(args)]
    assert sorted(lab for lab in labels if lab.startswith("fixup:")) == sorted(
        f"fixup:{t}#1" for t in runnable
    )
    # The trace check owns the ordering rule (GS104) and exclusivity (GS102).
    sc = next(s for s in LW.scenarios(args, LW.Policy.of(args)) if s.name == "fix_while_producing")
    assert LW.check_trace(args, LW.Policy.of(args), sc, run) == []


def test_human_gate_and_its_dependents_wait(tmp_path: Path) -> None:
    run = _sim(tmp_path, _args())
    labels = _labels(run)
    assert "work:5321" not in labels
    assert "work:5333" not in labels
    by_task = {t["task"]: t["status"] for t in run["result"]["tasks"]}
    assert by_task["5321"] == "waiting_on_person"
    assert by_task["5333"] == "waiting_on_person"
    assert by_task["5326"] == "merged"


def test_critical_path_head_starts_first(tmp_path: Path) -> None:
    run = _sim(tmp_path, _args())
    assert _labels(run)[0] == "work:5327"


def test_failed_worker_blocks_only_its_dependents(tmp_path: Path) -> None:
    args = _args()
    args["tasks"] = [{"id": "a"}, {"id": "b", "after": ["a"]}, {"id": "c"}]
    pol = LW.Policy.of(args)
    sc = LW.Scenario("a_fails", "", {}, {"a": "failed"})
    run = LW.simulate(shutil.which("node"), TEMPLATE, args, sc, tmp_path)
    by_task = {t["task"]: t["status"] for t in run["result"]["tasks"]}
    assert by_task == {"a": "failed", "b": "blocked", "c": "merged"}
    assert LW.check_trace(args, pol, sc, run) == []


def test_feature_branch_mode_targets_the_feature_and_opens_the_final_pr(tmp_path: Path) -> None:
    args = _args(feature={"branch": "grind/meta-5335-scratch"})
    run = _sim(tmp_path, args)
    starts = [e for e in run["trace"] if e["ev"] == "start"]
    work = next(e for e in starts if e["label"] == "work:5327")
    assert "Branch from origin/grind/meta-5335-scratch" in work["prompt"]
    assert "switch --discard-changes grind/meta-5335-scratch" in work["prompt"]
    assert starts[-1]["label"] == "feature-pr"
    assert "into master" in starts[-1]["prompt"]
    assert _lint(TEMPLATE, args, ["happy", "red_check"]) == []


def test_repo_without_merge_queue_merges_pinned(tmp_path: Path) -> None:
    run = _sim(tmp_path, _args(land="merge"))
    chap = next(
        e for e in run["trace"] if e["ev"] == "start" and e["label"].startswith("chaperone:")
    )
    assert "no merge queue" in chap["prompt"]
    assert "--match-head-commit h-5327-0" in chap["prompt"]


def test_push_script_gate_is_named_in_worker_prompts(tmp_path: Path) -> None:
    run = _sim(
        tmp_path,
        _args(gate={"push": "./push", "commands": ["soldr lint --host-only"]}),
        "red_check",
    )
    prompts = [
        e["prompt"]
        for e in run["trace"]
        if e["ev"] == "start" and e["label"].startswith(("work:", "fixup:"))
    ]
    assert prompts
    assert all("push only with `./push`" in p for p in prompts)
    assert all("`soldr lint --host-only`" in p for p in prompts)


def test_cli_reports_ok_and_json(tmp_path: Path) -> None:
    args_file = tmp_path / "args.json"
    args_file.write_text(json.dumps(_args()), encoding="utf-8")
    assert LW.main([str(TEMPLATE), "--args", str(args_file), "--scenario", "happy"]) == 0


def _run(tmp_path: Path, args: dict[str, Any], sc: Any) -> dict[str, Any]:
    run = LW.simulate(shutil.which("node"), TEMPLATE, args, sc, tmp_path)
    assert "error" not in run, run["error"]
    return run


def _chain_args() -> dict[str, Any]:
    return _args(tasks=[{"id": "a"}, {"id": "b", "after": ["a"]}, {"id": "c"}])


def test_open_covering_pr_blocks_dependents_until_it_merges(tmp_path: Path) -> None:
    run = _run(tmp_path, _chain_args(), LW.Scenario("open", "", {}, {"a": "covered"}))
    by_task = {t["task"]: t["status"] for t in run["result"]["tasks"]}
    assert by_task == {"a": "covered_open", "b": "blocked", "c": "merged"}
    assert "work:b" not in _labels(run)


def test_merged_covering_pr_releases_dependents(tmp_path: Path) -> None:
    run = _run(tmp_path, _chain_args(), LW.Scenario("merged", "", {}, {"a": "covered_merged"}))
    assert "work:b" in _labels(run)


def test_a_fixup_with_nothing_to_change_is_not_repeated(tmp_path: Path) -> None:
    args = _chain_args()
    sc = LW.Scenario(
        "nc", "", {"a": ["red_check", "red_check", "merged"]}, fixup={"a": "no_change"}
    )
    run = _run(tmp_path, args, sc)
    assert [lab for lab in _labels(run) if lab.startswith("fixup:")] == ["fixup:a#1"]
    by_task = {t["task"]: t["status"] for t in run["result"]["tasks"]}
    assert by_task["a"] == "merged"
    assert LW.check_trace(args, LW.Policy.of(args), sc, run) == []


def test_a_checkout_that_stays_dirty_halts_central_work_and_still_reports(tmp_path: Path) -> None:
    args = _chain_args()
    sc = LW.Scenario("dirty", "", {}, {}, ["a"])
    sc.__dict__["stuck_dirty"] = True
    run = _run(tmp_path, args, sc)
    labels = _labels(run)
    cleanups = [lab for lab in labels if lab.startswith("cleanup:")]
    assert len(cleanups) == LW.Policy.of(args).max_cleanups + 1
    # Nothing central runs after the last cleanup, but a's chaperone drains.
    after = labels[labels.index(cleanups[-1]) + 1 :]
    assert not [lab for lab in after if lab.startswith(("work:", "fixup:"))]
    result = run["result"]
    assert {t["task"]: t["status"] for t in result["tasks"]}["a"] == "merged"
    assert result["problems"][0]["kind"] == "checkout"


def test_null_policy_values_use_the_defaults() -> None:
    pol = LW.Policy.of({"policy": {"maxInFlight": None, "maxFixups": None}})
    assert (pol.max_in_flight, pol.max_fixups) == (6, 3)
