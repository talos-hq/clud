"""`/do` typed in the real Claude Code: what the model receives (#1322).

`/do`'s body is one `` !`clud do-prompt "$ARGUMENTS"` `` line that Claude Code
runs at invocation, so the model's first request carries the rendered prompt:
the starting-branch verdict and the contract. Each scenario builds a world
(branch, PRs and issues on the fake GitHub), types `/do <target>`, and
asserts on that first request.
"""

from __future__ import annotations

import json

import pytest

from tests import process
from tests.harness.harness import Harness

ISSUE = "https://github.com/o/r/issues/7"
META = "https://github.com/o/r/issues/9"
ACK = {"default_text": "ACK", "roles": [{"name": "main", "steps": [{"text": "ACK"}]}]}


def _gh(harness: Harness, **overrides: object) -> None:
    state = {
        "repo": "o/r",
        "issues": {
            "7": {"title": "fix the thing", "body": "please", "sub_issues": []},
            "9": {
                "title": "meta",
                "body": "children",
                "sub_issues": [{"number": 10, "state": "open"}],
            },
        },
        "prs": [],
    }
    state.update(overrides)
    harness.write_gh_state(state)


def _branch(harness: Harness, name: str, *, commit: bool) -> None:
    harness.git("switch", "-q", "-c", name)
    if commit:
        (harness.repo / f"{name.replace('/', '_')}.txt").write_text("work\n", encoding="utf-8")
        harness.git("add", "-A")
        harness.git("commit", "-q", "-m", f"work on {name}")


def _do(harness: Harness, target: str, **run_kwargs: object) -> str:
    result = harness.run(f"/do {target}", ACK, **run_kwargs)  # type: ignore[arg-type]
    assert result.returncode == 0, result.stdout[-2000:]
    return result.prompt_text("main")


def _rendered(text: str) -> None:
    assert "rendered for this checkout by `clud do-prompt`" in text, text[:2000]
    assert "!`clud" not in text
    assert "$ARGUMENTS" not in text


def test_h1_default_branch_single_issue(harness: Harness) -> None:
    _gh(harness)
    text = _do(harness, ISSUE)
    _rendered(text)
    assert "Verdict: `default`" in text
    assert "Read the issue and implement it" in text
    assert "## Finish (always, as the very last step)" in text
    assert "git pull --ff-only origin main" in text


def test_h2_open_pr_branch_is_in_progress(harness: Harness) -> None:
    _branch(harness, "feat/x", commit=True)
    _gh(harness, prs=[{"number": 41, "head": "feat/x", "state": "OPEN"}])
    text = _do(harness, ISSUE)
    _rendered(text)
    assert "Verdict: `in_progress`" in text
    assert "Continue on it; `feat/x` is your starting branch" in text


def test_h3_merged_pr_branch_is_stale(harness: Harness) -> None:
    _branch(harness, "feat/x", commit=True)
    _gh(harness, prs=[{"number": 812, "head": "feat/x", "state": "MERGED"}])
    text = _do(harness, ISSUE)
    _rendered(text)
    assert "Verdict: `stale_merged`" in text
    assert "PR #812 is merged" in text
    assert "git switch main && git pull --ff-only" in text


def test_h4_empty_branch_is_stale(harness: Harness) -> None:
    _branch(harness, "feat/empty", commit=False)
    _gh(harness)
    text = _do(harness, ISSUE)
    assert "Verdict: `stale_empty`" in text


def test_h5_uncommitted_changes_stop_and_ask(harness: Harness) -> None:
    (harness.repo / "README.md").write_text("# edited\n", encoding="utf-8")
    _gh(harness)
    text = _do(harness, ISSUE)
    assert "Verdict: `dirty`" in text
    assert "Stop and ask the user" in text
    assert "README.md" in text


def test_h6_meta_issue_routes_to_grind(harness: Harness) -> None:
    _gh(harness)
    text = _do(harness, META)
    _rendered(text)
    assert META in text
    assert "This is a meta issue" in text
    assert "use Read to read the complete grind router" in text
    assert str(harness.config / "skills" / "grind" / "SKILL.md") in text
    assert "/grind requires the Claude harness" in text
    assert f"invoke `/grind {META}`" not in text
    assert "Read the issue and implement it" not in text


def test_h7_broken_gh_stops(harness: Harness) -> None:
    _gh(harness, fail=True)
    text = _do(harness, ISSUE)
    _rendered(text)
    assert "Stop. Tell the user:" in text
    assert "Read the issue and implement it" not in text


def test_h8_free_form_goal(harness: Harness) -> None:
    _gh(harness)
    text = _do(harness, "refactor the launcher")
    _rendered(text)
    assert "The goal: refactor the launcher" in text
    assert "validated, tested, pushed and merged" in text
    assert "## Finish (always, as the very last step)" in text


def test_h9_renders_with_permission_prompts_on(harness: Harness) -> None:
    # No --dangerously-skip-permissions: the skill's allowed-tools must cover it.
    _gh(harness)
    text = _do(harness, ISSUE, skip_permissions=False)
    _rendered(text)
    assert "Verdict: `default`" in text


def test_h10_command_guard_and_rendering(harness: Harness) -> None:
    # With clud-cmd-scan registered, the prompt still renders. Record whether
    # the `!` command itself was seen by PreToolUse: the documented answer
    # lives in testing-tiers.md; this test pins it.
    _gh(harness)
    result = harness.run(f"/do {ISSUE}", ACK)
    _rendered(result.prompt_text("main"))
    seen = [
        h
        for h in result.hooks
        if h.get("event") == "PreToolUse" and "do-prompt" in json.dumps(h.get("tool_input"))
    ]
    assert seen == [], "the `!` expansion is not a tool call and never reaches PreToolUse"


def test_h11_missing_clud_aborts_before_any_model_call(harness: Harness) -> None:
    # With `clud` off PATH (only possible outside a clud-launched session),
    # the `!` command fails, and
    # Claude Code aborts the skill before calling the model: nothing
    # half-rendered ever reaches it. Assert the structure, not Claude Code's or
    # the shell's wording.
    harness.hide_clud = True
    _gh(harness)
    result = harness.run(f"/do {ISSUE}", ACK)
    assert result.requests == []
    errors = [
        e
        for e in result.events
        if e.get("type") == "user" and "do-prompt" in json.dumps(e) and "stderr" in json.dumps(e)
    ]
    assert errors, json.dumps(result.events)[:2000]


@pytest.mark.parametrize("target", [ISSUE, META, "refactor the launcher"])
def test_h12_clud_do_prompt_and_slash_do_agree(harness: Harness, target: str) -> None:
    _branch(harness, "feat/x", commit=True)
    _gh(harness, prs=[{"number": 812, "head": "feat/x", "state": "MERGED"}])
    direct = process.run(
        [str(harness.clud), "do-prompt", target],
        cwd=str(harness.repo),
        capture_output=True,
        text=True,
        env=harness.env(),
        timeout=60,
    )
    assert direct.returncode == 0, direct.stdout
    text = _do(harness, target)
    # Every line of the direct render reached the model through /do.
    for line in (direct.stdout or "").splitlines():
        if line.strip():
            assert json.dumps(line)[1:-1] in text, line


def test_h13_goal_seed_does_not_bypass_explicit_only_do_policy(harness: Harness) -> None:
    # /goal does not expand the nested /do. The old model-side load is now
    # deliberately refused by the explicit-only policy (#1510). #2013 tracks
    # the CLI launch handoff; direct explicit /do rendering stays covered by
    # H1-H12 rather than treating this denied tool call as a rendered prompt.
    _gh(harness)
    script = {
        "default_text": "ACK",
        "roles": [
            {
                "name": "main",
                "steps": [
                    {"tool_use": {"name": "Skill", "input": {"skill": "do", "args": ISSUE}}},
                    {"text": "ACK"},
                ],
            }
        ],
    }
    result = harness.run(f"/goal /do {ISSUE}", script)
    assert result.returncode == 0, result.stdout[-2000:]
    first = result.first_request("main")
    assert f"/do {ISSUE}" in json.dumps(first.get("messages"))
    after_skill = json.dumps([r.get("messages") for r in result.requests if r.get("turn") == 1])
    assert "cannot be used with Skill tool due to disable-model-invocation" in after_skill
    assert "rendered for this checkout by `clud do-prompt`" not in after_skill


def _cli_launch(harness: Harness, target: str) -> list[str]:
    launch = process.run(
        [str(harness.clud), "--dry-run", "--claude", "do", target],
        cwd=str(harness.repo),
        capture_output=True,
        text=True,
        env=harness.env(),
        timeout=60,
    )
    assert launch.returncode == 0, launch.stderr
    return json.loads(launch.stdout)["command"]


def _cli_system(command: list[str]) -> list[str]:
    """The plan's appended system prompt, which carries the /do procedure (#2372)."""
    at = command.index("--append-system-prompt")
    return command[at : at + 2]


@pytest.mark.parametrize(
    "target",
    [
        ISSUE,
        "refactor the launcher",
        'repair "quoted words"; $(touch cli-target-executed) `touch cli-backtick-executed` &',
    ],
)
def test_h14_explicit_cli_goal_delivers_live_do_contract(harness: Harness, target: str) -> None:
    _branch(harness, "feat/x", commit=True)
    _gh(harness, prs=[{"number": 812, "head": "feat/x", "state": "MERGED"}])
    command = _cli_launch(harness, target)
    seed = command[-1]
    assert seed.startswith("/goal ")
    # Claude Code rejects a /goal condition over 4000 chars (#2372).
    assert len(seed) - len("/goal ") < 4000
    direct = process.run(
        [str(harness.clud), "do-prompt", target],
        cwd=str(harness.repo),
        capture_output=True,
        text=True,
        env=harness.env(),
        timeout=60,
    )
    assert direct.returncode == 0, direct.stderr
    # Feed the actual CLI seed through the real harness, with no recovery
    # Skill call. The first model request must already have the live verdict.
    result = harness.run(seed, ACK, extra_args=_cli_system(command))
    assert result.returncode == 0, result.stdout[-2000:]
    request = result.first_request("main")
    text = json.dumps([request.get("system"), request.get("messages")])
    _rendered(text)
    assert "Verdict: `stale_merged`" in text
    assert "A session-scoped Stop hook is now active" in text
    assert "812" in text
    assert "RED" in text
    assert "GREEN" in text
    for line in direct.stdout.splitlines():
        if line.strip():
            assert json.dumps(line)[1:-1] in text, line
    assert not (harness.repo / "cli-target-executed").exists()
    assert not (harness.repo / "cli-backtick-executed").exists()


def test_h15_explicit_cli_meta_handoff_reads_the_router(harness: Harness) -> None:
    _gh(harness)
    router = harness.config / "skills" / "grind" / "SKILL.md"
    command = _cli_launch(harness, META)
    seed = command[-1]
    assert seed.startswith("/goal ")
    script = {
        "default_text": "ACK",
        "roles": [
            {
                "name": "main",
                "steps": [
                    {"tool_use": {"name": "Read", "input": {"file_path": str(router)}}},
                    {"text": "ACK"},
                ],
            }
        ],
    }
    result = harness.run(seed, script, extra_args=_cli_system(command))
    assert result.returncode == 0, result.stdout[-2000:]
    request = result.first_request("main")
    text = json.dumps([request.get("system"), request.get("messages")])
    _rendered(text)
    assert "Verdict: `default`" in text
    assert META in text
    assert "A session-scoped Stop hook is now active" in text
    assert str(router) in text
    assert "Workflow" in text
    after_read = json.dumps([r.get("messages") for r in result.requests if r.get("turn") == 1])
    assert "/grind requires the Claude harness" in after_read
    assert "cannot be used with Skill tool" not in after_read


@pytest.mark.parametrize(
    ("issue", "labels", "verdict"),
    [
        ({"state": "closed"}, [], "blocked"),
        ({"assignees": ["other"]}, [], "blocked"),
        ({"comments": [{"id": 1, "author": "other", "body": "Claiming: fix"}]}, [], "blocked"),
        ({"comments": [{"id": 1, "author": "fake-gh", "body": "Claiming: fix"}]}, [], "unknown"),
        ({}, ["ready"], "needs-ready"),
        ({"labels": ["ready"]}, ["ready"], "ok"),
        ({}, [], "ok"),
    ],
)
def test_do_issue_preflight(harness: Harness, issue: dict, labels: list[str], verdict: str) -> None:
    _gh(harness, issues={"7": issue}, labels=labels)
    result = process.run(
        [str(harness.clud), "do-prompt", ISSUE],
        cwd=str(harness.repo),
        env=harness.env(),
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    starting = result.stdout.split("## Starting point", 1)[1].split("## What to do", 1)[0]
    assert f"Issue verdict: `{verdict}`" in starting
    assert "before the first code change" in result.stdout.lower()
    assert "before `gh pr create`" in result.stdout
    assert "before enabling auto-merge or queueing" in result.stdout
    calls = harness.read_gh_state()["calls"]
    assert not any(
        call[:2] in [["issue", "comment"], ["issue", "edit"], ["issue", "close"]] for call in calls
    )


@pytest.mark.parametrize(
    "fault",
    [
        "issue view",
        "api GET user",
        "api GET repos/o/r/issues/7/comments?per_page=100",
        "label list",
    ],
)
def test_do_issue_read_failure_requires_manual_check(harness: Harness, fault: str) -> None:
    _gh(harness, fail_on=[fault])
    result = process.run(
        [str(harness.clud), "do-prompt", ISSUE],
        cwd=str(harness.repo),
        env=harness.env(),
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    assert "Issue verdict: `unknown`" in result.stdout
    assert "check manually" in result.stdout
    assert "Issue verdict: `ok`" not in result.stdout
