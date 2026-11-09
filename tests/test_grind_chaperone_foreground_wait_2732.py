"""#2732: a grind chaperone must run its waiters in the foreground.

A Workflow subagent never receives a background-task completion notification.
When the chaperone was told to background `pr_merge_watch`, it said "waiting
for notification", ended its turn and returned `queued, enqueued:false`; the
workflow then started a fresh chaperone that did the same, so no PR was ever
enqueued. These tests run the production `grind-run.js` in the scripted
mock-agent host (`tests/fixtures/grind_workflow_probe.mjs`) and assert that
every chaperone prompt, on every path, demands a foreground waiter that fits
the Bash cap, and that the agent files say the same.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

import pytest

from tests import process
from tests.test_grind_editor_integrator_chaperones import (
    ASSETS,
    FIXED,
    MERGED,
    PROBE,
    WORKFLOW,
    _goal,
    _prompts,
    _verdict,
    probe,
)

BASH_CAP_S = 600
STALL = _verdict("queued", enqueued=False)  # the #2732 symptom
QUEUED = _verdict("queued", exit_code=4, enqueued=True)
FIX = {"chaperone-fix:g1": [FIXED]}
RED = _verdict("needs_fix", exit_code=1, failure_log="test x failed")
EJECTED = _verdict("needs_fixup", exit_code=13)

# Any instruction that sends a waiter to the background or tells the agent to
# wait for a notification it will never get.
BACKGROUND_INSTRUCTIONS = (
    re.compile(r"in the background", re.I),
    re.compile(r"wait for (its|the|a) (completion )?notification", re.I),
    re.compile(r"(?<!never use )(?<!never with )`?run_in_background`?(?!: you get no)", re.I),
)


def _watch_prompts(out: dict) -> list[str]:
    return [p["prompt"] for p in out["prompts"] if p.get("label", "").startswith("chaperone:")]


def _assert_foreground(prompt: str) -> None:
    assert "in the foreground" in prompt, prompt
    assert "timeout 600000" in prompt, prompt
    assert "never use run_in_background" in prompt, prompt
    assert "enqueue before you return" in prompt, prompt
    for pattern in BACKGROUND_INSTRUCTIONS:
        assert not pattern.search(prompt.replace("never use run_in_background", "")), (
            pattern.pattern,
            prompt,
        )
    for timeout in re.findall(r"--timeout (\d+)", prompt):
        assert int(timeout) < BASH_CAP_S, prompt


@pytest.mark.parametrize(
    ("name", "script"),
    [
        ("merged at once", {}),
        ("stalled then merged", {"chaperone:g1": [STALL, MERGED]}),
        ("queued then merged", {"chaperone:g1": [QUEUED, MERGED]}),
        ("red check then fix then merged", {"chaperone:g1": [RED, MERGED], **FIX}),
        ("ejected then fixup then merged", {"chaperone:g1": [EJECTED, MERGED], **FIX}),
        ("queued until finish", {"chaperone:g1": [QUEUED] * 4}),
        ("stalled until finish", {"chaperone:g1": [STALL] * 4}),
    ],
)
def test_every_chaperone_watch_prompt_demands_a_foreground_waiter(
    tmp_path: Path, name: str, script: dict
) -> None:
    out = probe(tmp_path, script)
    prompts = _watch_prompts(out)
    assert prompts, name
    for prompt in prompts:
        _assert_foreground(prompt)


def test_a_feature_stage_watch_prompt_is_foreground_too(tmp_path: Path) -> None:
    out = probe(
        tmp_path,
        {},
        feature={"branch": "grind/meta-1-x", "worktree": str(tmp_path), "pr": "77"},
        plan={
            "schema": "grind-plan/v1",
            "run_id": "x",
            "meta": "1",
            "repo": "o/r",
            "stages": [{"stage": "feature", "group": "x", "children": ["g1"]}],
        },
    )
    for prompt in _watch_prompts(out):
        _assert_foreground(prompt)


def test_many_parallel_chaperones_all_get_foreground_prompts(tmp_path: Path) -> None:
    goals = ("g1", "g2", "g3", "g4")
    out = probe(tmp_path, {"chaperone:g2": [STALL, MERGED]}, goals=goals, mode="parallel")
    prompts = _watch_prompts(out)
    assert len(prompts) >= len(goals)
    for prompt in prompts:
        _assert_foreground(prompt)


def test_a_chaperone_that_stalls_unenqueued_is_bounded_not_respawned_forever(
    tmp_path: Path,
) -> None:
    # The exact #2732 loop: every chaperone returns queued/enqueued:false.
    out = probe(tmp_path, {"chaperone:g1": [STALL] * 20})
    watches = _prompts(out, "chaperone:g1")
    assert len(watches) <= 5, len(watches)
    g1 = _goal(out, "g1")
    assert g1["merged"] is False
    # Never enqueued, so no re-watch may claim it is already in the queue.
    assert not any("already in the queue" in p for p in watches)
    assert all("github/pr_merge.py" in p for p in watches)


def test_a_stalled_chaperone_is_replaced_by_one_that_still_enqueues(tmp_path: Path) -> None:
    out = probe(tmp_path, {"chaperone:g1": [STALL, MERGED]})
    first, second = _prompts(out, "chaperone:g1")[:2]
    for prompt in (first, second):
        assert prompt.index("--no-merge-wait") < prompt.index("github/pr_merge.py")
        assert "--match-head-commit" in prompt
    assert _goal(out, "g1")["merged"] is True


def test_an_enqueued_pr_is_followed_without_re_enqueueing(tmp_path: Path) -> None:
    out = probe(tmp_path, {"chaperone:g1": [QUEUED, MERGED]})
    second = _prompts(out, "chaperone:g1")[1]
    assert "already in the queue" in second
    assert "github/pr_merge.py" not in second
    _assert_foreground(second)


def _raw_probe(tmp_path: Path, **extra: object) -> dict:
    args = {
        "repo": str(tmp_path / "proj"),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "goals": [{"id": "g1", "title": "g1", "brief": "g1"}],
        **extra,
    }
    config = tmp_path / "probe.json"
    config.write_text(json.dumps({"args": args, "script": {}}), encoding="utf-8")
    result = process.run(
        ["node", str(PROBE), str(WORKFLOW), str(config)],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


@pytest.mark.parametrize("seconds", [541, 600, 3600])
def test_a_watch_deadline_past_the_bash_cap_is_rejected(tmp_path: Path, seconds: int) -> None:
    out = _raw_probe(tmp_path, queueWatchSeconds=seconds)
    assert "queueWatchSeconds must be an integer from 60 to 540" in out.get("error", ""), out


@pytest.mark.parametrize("seconds", [60, 300, 540])
def test_a_watch_deadline_within_the_cap_reaches_the_prompt(tmp_path: Path, seconds: int) -> None:
    out = probe(tmp_path, {}, queueWatchSeconds=seconds)
    prompt = _watch_prompts(out)[0]
    assert f"--timeout {seconds}" in prompt
    _assert_foreground(prompt)


def _instruction(text: str) -> str:
    return text.split("\n## ", 1)[0]


@pytest.mark.parametrize("agent", ["grind-chaperone", "grind-integrator"])
def test_waiting_agents_never_background_pr_merge_watch(agent: str) -> None:
    text = (ASSETS / "agents" / f"{agent}.md").read_text(encoding="utf-8")
    for line in text.splitlines():
        if "pr_merge_watch" in line or "waiter" in line:
            assert "wait for its completion notification" not in line, line
            assert not re.search(r"with `?run_in_background`?;", line), line
    assert "timeout 600000" in text
    assert "#2732" in text


def test_the_chaperone_agent_must_enqueue_within_one_invocation() -> None:
    text = _instruction((ASSETS / "agents/grind-chaperone.md").read_text(encoding="utf-8"))
    assert "Never use run_in_background" in text
    assert "in the foreground" in text
    assert "enqueue within the same invocation" in text
