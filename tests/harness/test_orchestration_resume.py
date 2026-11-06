"""Resume the real native Workflow cache without redoing completed branches."""
from __future__ import annotations

import difflib
import json
import shlex
import sys
from pathlib import Path
from typing import Any

import pytest

from tests.harness.harness import Harness, RunResult
from tests.harness.test_grind import (
    _after,
    _bash,
    _goal_roles,
    _goals,
    _merged,
    _no_notes,
    _script,
    _structured,
)
from tests.harness.test_grind_gate_binding import install_outcome_guard
from tests.harness.workflow_results import workflow_notices, workflow_results


def capture_resume_input(h: Harness, monkeypatch: pytest.MonkeyPatch) -> Path:
    install_outcome_guard(h, monkeypatch)
    epochs = h.logs / "next-gate-epochs.json"
    epochs.write_text(json.dumps({"b": 1, "c": 1}))
    output = h.logs / "native-resume-input.json"
    original = h.settings
    recorder = Path(__file__).with_name("capture_workflow_resume.py")
    command = shlex.join([sys.executable, str(recorder), str(epochs), str(output)])

    def settings(**kwargs: Any) -> Path:
        path = original(**kwargs)
        data = json.loads(path.read_text())
        data["hooks"]["PostToolUse"].append({"matcher": "Workflow", "hooks": [{
            "type": "command", "command": command,
        }]})
        path.write_text(json.dumps(data))
        return path

    monkeypatch.setattr(h, "settings", settings)
    return output


def gate_role(name: str, goal: str, tag: str, ready: bool, epoch: int | None = None) -> dict:
    role = {"name": name, "match": "You are the /grind gate checker", "match_prompt": [
        f"Goal {goal}:",
    ], "steps": [
        _bash(f"gh release view {tag} --repo o/r"),
        _after({"is_error": not ready}, _structured({"ready": ready, "evidence": tag})),
    ]}
    if epoch is not None:
        role["match_prompt"].append(f"Observation: {epoch}/1.")
    return role



def native_resume_steps(captured: Path, scheduled: bool) -> list[dict[str, Any]]:
    steps = [{"text": "unused"}, {"text": "unused"}]
    if scheduled:
        steps.extend([
            {"tool_use": {"name": "ScheduleWakeup", "input": {
                "delaySeconds": 60, "noop": False,
                "reason": "Resume the deferred publication branch after its native wakeup.",
                "prompt": 'WAKE_DEFERRED_GRAPH remaining_goals=["b","c"]',
            }}},
            _after({"is_error": False}, {"text": "GRAPH_RESUMPTION_ARMED"}),
        ])
    steps.extend([
        {"tool_use": {"name": "Workflow", "input_file": str(captured)}},
        _after({"is_error": False}, {"text": "NATIVE_RESUME_STARTED"}),
    ])
    if scheduled:
        steps.extend([
            {"tool_use": {"name": "ScheduleWakeup", "input": {"stop": True}}},
            _after({"is_error": False}, {"text": "NATIVE_GRAPH_RESUME_COMPLETE"}),
        ])
    else:
        steps.append({"text": "NATIVE_GRAPH_RESUME_COMPLETE"})
    return steps


def scheduled_resume(h: Harness, script: dict[str, Any]) -> RunResult:
    """Keep native wakeup failures observable after the PTY times out."""
    try:
        return h.run_clud_interactive(
            '/loop WAKE_DEFERRED_GRAPH remaining_goals=["b","c"]', script,
            model="nvidia/nemotron-3-ultra-550b-a55b:free",
            finished="NATIVE_GRAPH_RESUME_COMPLETE", timeout=180, confirm_background_exit=False,
        )
    except AssertionError:
        requests = h.logs / "requests.jsonl"
        lines = requests.read_text().splitlines() if requests.exists() else []
        rows = [json.loads(line) for line in lines]
        for row in [row for row in rows if row.get("role") == "main"][-8:]:
            print("NATIVE_RESUME_WAKEUP_REQUEST", json.dumps({
                "n": row.get("n"), "turn": row.get("turn"), "note": row.get("note"),
                "tools": row.get("tools"),
                "latest": str(row.get("messages", [])[-1:])[-1200:],
            }))
        hooks = h.logs / "hooks.jsonl"
        records = ([json.loads(line) for line in hooks.read_text().splitlines()]
                   if hooks.exists() else [])
        for row in [row for row in records if row.get("tool_name") == "Workflow"][-4:]:
            print("NATIVE_RESUME_HOOK_METADATA", json.dumps({key: row.get(key) for key in (
                "event", "session_id", "transcript_path",
                "transcript_versions", "transcript_read_error",
            )}))
        for transcript in list(h.config.glob("projects/*/*.jsonl"))[:3]:
            rows = [json.loads(line) for line in transcript.read_text().splitlines()]
            print("NATIVE_RESUME_TRANSCRIPT_METADATA", json.dumps({
                "path": str(transcript), "session": h.session_id,
                "versions": [{key: row.get(key) for key in ("type", "sessionId", "version")}
                             for row in rows if "version" in row][-8:],
            }))
        raise


def print_resume_diagnostics(h: Harness, result: RunResult, captured: Path) -> None:
    print("NATIVE_RESUME_REFERENCE", captured.read_text() if captured.exists() else "missing")
    metadata = [row for row in result.hooks if row.get("tool_name") == "Workflow"]
    for row in metadata[-4:]:
        print("NATIVE_WORKFLOW_VERSION_METADATA", json.dumps(row)[-6000:])
    outcomes = h.logs / "gate-native-outcomes.jsonl"
    if outcomes.exists():
        rows = [json.loads(line) for line in outcomes.read_text().splitlines()]
        for row in [row for row in rows if row.get("tool_name") == "Workflow"][-4:]:
            print("NATIVE_WORKFLOW_POST_METADATA", json.dumps(row)[-6000:])
    for receipt in list(h.config.glob("projects/*/*/grind-resume-*.json"))[:6]:
        print("NATIVE_WORKFLOW_ORIGIN_RECEIPT", str(receipt), receipt.read_text())
    for role in ("planner:a", "worker:a", "reviewer:a", "integrator:a"):
        requests = [row for row in result.requests
                    if row["role"] == role and row.get("turn") == 0]
        if len(requests) > 1:
            first = json.dumps(requests[0]["messages"], indent=1).splitlines()
            last = json.dumps(requests[-1]["messages"], indent=1).splitlines()
            print("NATIVE_RESUME_PROMPT_DIFF", role,
                  "\n".join(list(difflib.unified_diff(first, last))[:80]))
    for journal in list(h.config.glob("projects/*/*/subagents/workflows/*/journal.jsonl"))[:4]:
        print("NATIVE_RESUME_JOURNAL", journal.name, journal.read_text()[:20000])


@pytest.mark.timeout(240)
@pytest.mark.parametrize(("mode", "scheduled"), [
    ("parallel", False), ("sequential", False), ("parallel", True),
])
def test_native_resume_only_reobserves_deferred_branch(
    harness: Harness, monkeypatch, mode: str, scheduled: bool,
) -> None:
    h = harness
    graph = {"session_repo": "o/r", "session_root": str(h.repo), "goals": [
        {"id": "a", "repo": "o/r", "main": "main", "blockers": []},
        {"id": "b", "repo": "o/r", "main": "main", "blockers": [
            {"goal": "a", "when": "released", "check": "gh release view v-pending --repo o/r"},
        ]},
        {"id": "c", "repo": "o/r", "main": "main", "blockers": [{"goal": "b", "when": "merged"}]},
        {"id": "d", "repo": "o/r", "main": "main", "blockers": []},
        {"id": "e", "repo": "o/r", "main": "main", "blockers": [
            {"goal": "d", "when": "released", "check": "gh release view v-ready --repo o/r"},
        ]},
    ], "proposed_goals": [], "risks": []}
    state = h.read_gh_state()
    state["releases"] = {"v-ready": {"publishedAt": "2026-10-22T00:00:00Z"}}
    state["release_after_views"] = {"v-pending": {
        "after": 1, "release": {"publishedAt": "2026-10-22T00:00:00Z"},
    }}
    h.write_gh_state(state)
    h.write_run_facts({"phase": "execute", "orchestration": graph})
    captured = capture_resume_input(h, monkeypatch)
    roles = [role for goal in ("a", "b", "c", "d", "e") for role in _goal_roles(h, goal)]
    roles.extend([
        gate_role("gate:b:resume", "b", "v-pending", True, 1),
        gate_role("gate:b:initial", "b", "v-pending", False),
        gate_role("gate:e", "e", "v-ready", True),
    ])
    script = _script(h, mode, _goals("a", "b", "c", "d", "e"), roles,
                     orchestration=graph, gateAttempts=1)
    if scheduled:
        script["roles"][-1]["steps"].extend(native_resume_steps(captured, True)[2:])
        script["roles"].insert(0, {"name": "title", "match_prompt":
            "Write the title in the predominant language", "steps": [{"text": "Graph resume"}]})
    else:
        script["roles"].insert(-1, {"name": "main", "match_prompt": "</task-notification>",
                                   "steps": native_resume_steps(captured, False)})
    if scheduled:
        result = scheduled_resume(h, script)
    else:
        result = h.run("Resume only deferred publication work using the native Workflow cache.",
                       script, timeout=180)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    if any(row.get("note") for row in result.requests):
        print_resume_diagnostics(h, result, captured)
    _no_notes(result)
    outputs = workflow_results(result)
    assert len(outputs) == 2, outputs
    assert outputs[0]["resume"]["remaining_goals"] == ["b", "c"], outputs[0]
    assert "resume" not in outputs[1], outputs[1]
    reference = json.loads(captured.read_text())
    assert reference["resumeFromRunId"].startswith("wf_")
    resumed = [row["n"] for row in result.requests
               if row.get("step", {}).get("tool_use", {}).get("input_file") == str(captured)]
    assert len(resumed) == 1, resumed
    boundary = resumed[0]
    if scheduled:
        assert any(row.get("event") == "PostToolUse" and row.get("tool_name") == "ScheduleWakeup"
                   for row in result.hooks)
        resumed_request = next(row for row in result.requests if row["n"] == boundary)
        assert "WAKE_DEFERRED_GRAPH" in json.dumps(resumed_request["messages"][-1])
    assert _merged(h) == [f"grind/{goal}" for goal in ("a", "b", "c", "d", "e")]
    for row in result.requests:
        if row["role"] in ("gate:e", "planner:a", "worker:a", "reviewer:a", "integrator:a",
                           "chaperone:a:0", "planner:d", "worker:d", "reviewer:d", "integrator:d",
                           "chaperone:d:0", "planner:e", "worker:e", "reviewer:e", "integrator:e",
                           "chaperone:e:0"):
            assert row["n"] < boundary, row["role"]
    assert any(row["role"] == "gate:b:resume" and row["n"] > boundary for row in result.requests)
    assert any(row["role"] == "planner:b" and row["n"] > boundary for row in result.requests)
    assert any(row["role"] == "planner:c" and row["n"] > boundary for row in result.requests)


@pytest.mark.timeout(240)
@pytest.mark.parametrize("mode", ["parallel", "sequential"])
def test_native_resume_preserves_completed_work_across_three_generations(
    harness: Harness, monkeypatch, mode: str,
) -> None:
    h = harness
    graph = {"session_repo": "o/r", "session_root": str(h.repo), "goals": [
        {"id": "a", "repo": "o/r", "main": "main", "blockers": []},
        {"id": "b", "repo": "o/r", "main": "main", "blockers": [
            {"goal": "a", "when": "released", "check": "gh release view v-b --repo o/r"},
        ]},
        {"id": "c", "repo": "o/r", "main": "main", "blockers": [
            {"goal": "b", "when": "released", "check": "gh release view v-c --repo o/r"},
        ]},
        {"id": "d", "repo": "o/r", "main": "main", "blockers": []},
    ], "proposed_goals": [], "risks": []}
    state = h.read_gh_state()
    state["release_after_views"] = {tag: {
        "after": 1, "release": {"publishedAt": "2026-10-22T00:00:00Z"},
    } for tag in ("v-b", "v-c")}
    h.write_gh_state(state)
    h.write_run_facts({"phase": "execute", "orchestration": graph})
    captured = capture_resume_input(h, monkeypatch)
    (h.logs / "next-gate-epochs.json").write_text(json.dumps([
        {"b": 1, "c": 1}, {"b": 1, "c": 2},
    ]))
    roles = [role for goal in ("a", "b", "c", "d") for role in _goal_roles(h, goal)]
    roles.extend([
        gate_role("gate:b:0", "b", "v-b", False, 0),
        gate_role("gate:b:1", "b", "v-b", True, 1),
        gate_role("gate:c:1", "c", "v-c", False, 1),
        gate_role("gate:c:2", "c", "v-c", True, 2),
    ])
    script = _script(h, mode, _goals("a", "b", "c", "d"), roles,
                     orchestration=graph, gateAttempts=1)
    resume = {"tool_use": {"name": "Workflow", "input_file": str(captured)}}
    script["roles"].insert(-1, {"name": "main", "match_prompt": "</task-notification>", "steps": [
        {"text": "unused"}, {"text": "unused"}, resume,
        _after({"is_error": False}, {"text": "GENERATION_ONE_STARTED"}), resume,
        _after({"is_error": False}, {"text": "GENERATION_TWO_STARTED"}),
        {"text": "THREE_GENERATIONS_COMPLETE"},
    ]})
    result = h.run("Resume both publication gates without repeating completed work.",
                   script, timeout=210)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    outputs = workflow_results(result)
    assert len(outputs) == 3, outputs
    assert outputs[0]["resume"]["remaining_goals"] == ["b", "c"]
    assert outputs[1]["resume"]["remaining_goals"] == ["c"]
    assert outputs[1]["resume"]["gateEpochs"] == {"b": 1, "c": 2}
    assert "resume" not in outputs[2]
    assert _merged(h) == [f"grind/{goal}" for goal in ("a", "b", "c", "d")]
    for goal in ("a", "b", "c", "d"):
        for role in ("planner", "worker", "reviewer", "integrator", "chaperone"):
            label = f"chaperone:{goal}:0" if role == "chaperone" else f"{role}:{goal}"
            turns = [row for row in result.requests
                     if row["role"] == label and row.get("turn") == 0]
            assert len(turns) == 1, (goal, role, turns)
    for gate in ("gate:b:0", "gate:b:1", "gate:c:1", "gate:c:2"):
        assert len([row for row in result.requests
                    if row["role"] == gate and row.get("turn") == 0]) == 1, gate


def test_graph_resume_exhaustion_returns_deferred_manifest(harness: Harness, monkeypatch) -> None:
    h = harness
    graph = {"session_repo": "o/r", "session_root": str(h.repo), "goals": [
        {"id": "a", "repo": "o/r", "main": "main", "blockers": []},
        {"id": "b", "repo": "o/r", "main": "main", "blockers": [
            {"goal": "a", "when": "released", "check": "gh release view v-pending --repo o/r"},
        ]},
    ], "proposed_goals": [], "risks": []}
    h.write_run_facts({"phase": "execute", "orchestration": graph})
    install_outcome_guard(h, monkeypatch)
    roles = [*_goal_roles(h, "a"), gate_role("gate:b", "b", "v-pending", False, 64)]
    result = h.run("Retain publication state after the final permitted observation round.",
                   _script(h, "sequential", _goals("a", "b"), roles,
                           orchestration=graph, gateAttempts=1, gateEpoch=64))
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    outputs = workflow_results(result)
    assert len(outputs) == 1, outputs
    manifest = outputs[0]["resume"]
    assert manifest["exhausted"] is True
    assert manifest["remaining_goals"] == ["b"]
    assert manifest["gateEpochs"] == {"b": 64}
    assert _merged(h) == ["grind/a"]


@pytest.mark.parametrize("field", ["gateEpoch", "gateEpochs"])
def test_graph_resume_history_is_bounded_before_agents(harness: Harness, field: str) -> None:
    h = harness
    graph = {"session_repo": "o/r", "session_root": str(h.repo), "goals": [
        {"id": "a", "repo": "o/r", "main": "main", "blockers": []},
    ], "proposed_goals": [], "risks": []}
    h.write_run_facts({"phase": "execute", "orchestration": graph})
    script = _script(h, "sequential", _goals("a"), [], orchestration=graph,
                     **{field: 1000000000 if field == "gateEpoch" else {"a": 1000000000}})
    before = h.git("status", "--porcelain")
    result = h.run("Reject an excessive replay history before launching agents.", script)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    assert {row["role"] for row in result.requests} == {"main"}
    if field == "gateEpochs":
        errors = json.dumps([row.get("tool_results", []) for row in result.requests])
        assert "Initial graph launches cannot advance gateEpochs" in errors, errors
    else:
        notices = workflow_notices(result)
        assert "gateEpoch must be an integer from 0 to 64" in str(notices), notices
    assert h.git("status", "--porcelain") == before


@pytest.mark.parametrize("orchestrated", [False, True])
@pytest.mark.parametrize("reason", ["no-overlap", "deferred-group"])
def test_stage_deferral_never_creates_publication_resume(
    harness: Harness, orchestrated: bool, reason: str,
) -> None:
    h = harness
    plan = {"stages": [{"stage": "feature", "group": "next", "children": ["b"]}],
            "rules": {"no_overlap": "bugs_only"}, "deferred_groups": []}
    if reason == "no-overlap":
        plan["waiting_on_pr"] = 90
    else:
        plan["deferred_groups"] = [{"group": "next", "children": ["b"]}]
    args = {"plan": plan, "plan_url": "https://github.com/o/r/issues/1#issuecomment-1"}
    if orchestrated:
        args["orchestration"] = {"session_repo": "o/r", "session_root": str(h.repo),
                                 "goals": [{"id": "b", "repo": "o/r", "main": "main",
                                            "blockers": []}]}
    before = h.git("status", "--porcelain")
    result = h.run("Preserve the stage deferral without scheduling publication checks.",
                   _script(h, "sequential", _goals("b"), [], **args))
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    assert {row["role"] for row in result.requests} == {"main"}
    outputs = workflow_results(result)
    assert len(outputs) == 1, outputs
    assert outputs[0]["goals"][0]["status"] == "deferred", outputs
    assert "resume" not in outputs[0], outputs
    assert h.git("status", "--porcelain") == before


@pytest.mark.parametrize("change_args", [False, True])
@pytest.mark.parametrize("structured", [False, True])
def test_native_resume_replays_one_unchanged_agent(
    harness: Harness, monkeypatch, change_args: bool, structured: bool,
) -> None:
    """Separate agent-cache behavior from parallelism and argument changes."""
    h = harness
    captured = capture_resume_input(h, monkeypatch)
    (h.config / "workflows").mkdir(exist_ok=True)
    (h.config / "agents").mkdir(exist_ok=True)
    (h.config / "agents" / "resume-cache-probe.md").write_text(
        "---\nname: resume-cache-probe\ndescription: Native cache probe\ntools: [Bash]\n---\n"
        "NATIVE_CACHE_PROBE_ROLE\n"
    )
    options = {"agentType": "resume-cache-probe", "label": "cache-probe"}
    if structured:
        options["schema"] = {"type": "object", "required": ["observed"], "properties": {
            "observed": {"type": "string"},
        }}
    answer = "observed.observed" if structured else "observed"
    (h.config / "workflows" / "resume-cache-probe.js").write_text(
        "export const meta = {name: 'resume-cache-probe', description: 'Native cache probe'};\n"
        "const observed = await agent('Write the cache probe once.', "
        + json.dumps(options) + ");\nreturn " + answer + ";\n"
    )
    report = (_structured({"observed": "CACHE_OBSERVATION_COMPLETE"}) if structured
              else {"text": "CACHE_OBSERVATION_COMPLETE"})
    resume = {"tool_use": {"name": "Workflow", "input_file": str(captured)}}
    script = {"roles": [
        {"name": "cache-probe", "match": "NATIVE_CACHE_PROBE_ROLE", "steps": [
            _bash("printf observed >> cache-probe.txt"),
            _after({"is_error": False}, report),
        ]},
        {"name": "main", "match_prompt": "</task-notification>", "steps": [
            {"text": "unused"}, {"text": "unused"}, resume,
            _after({"is_error": False}, {"text": "CACHE_RESUME_STARTED"}),
        ]},
        {"name": "main", "steps": [
            {"tool_use": {"name": "Workflow", "input": {
                "name": "resume-cache-probe",
                "args": {} if change_args else {"gateEpochs": {"b": 1, "c": 1}},
            }}},
            {"text": "CACHE_PROBE_STARTED"},
        ]},
    ]}
    result = h.run("Observe the native unchanged-agent cache contract.", script)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    calls = [row for row in result.hooks if row.get("event") == "PreToolUse"
             and row.get("tool_name") == "Bash"
             and row.get("tool_input", {}).get("command") == "printf observed >> cache-probe.txt"]
    resumed = [row for row in result.requests if row.get("step", {}).get("tool_use", {}).get(
        "input_file") == str(captured)]
    assert len(resumed) == 1, resumed
    assert "CACHE_RESUME_STARTED" in result.stdout
    notices = workflow_notices(result)
    assert len(notices) == 2, notices
    for notice in notices.values():
        assert "<status>completed</status>" in notice, notice
        assert "<result>" in notice, notice
        assert "CACHE_OBSERVATION_COMPLETE" in notice, notice
    assert len(calls) == 1, {"calls": calls, "reference": captured.read_text()}
    assert (h.repo / "cache-probe.txt").read_text() == "observed"
