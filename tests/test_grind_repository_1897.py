"""Offline execution of grind's repository handoff contract (#1897)."""

from __future__ import annotations

import json
import os
import re
import shlex
import sys
from pathlib import Path

import pytest

from tests import process

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / "crates/clud-bin/assets/workflows/grind-run.js"
PROBE = ROOT / "tests/fixtures/grind_workflow_probe.mjs"


def probe(tmp_path: Path, classify: dict | None = None, **fields: object) -> dict:
    args = {
        "repo": str(tmp_path),
        "main": "main",
        "planOnly": True,
        "meta": "10",
        "goals": [{"id": "12", "title": "fix", "brief": "fix"}],
        **fields,
    }
    config = tmp_path / "probe.json"
    config.write_text(json.dumps({"args": args, "classify": classify}), encoding="utf-8")
    result = process.run(
        ["node", str(PROBE), str(WORKFLOW), str(config)],
        capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


@pytest.mark.parametrize("fields", [
    {"github_repo": "fork/project"},
    {"plan": {"repo": "fork/project"}},
    {"github_repo": "Fork/Project", "plan": {"repo": "fork/project"}},
])
def test_classification_receives_explicit_forge_identity(tmp_path: Path, fields: dict) -> None:
    result = probe(tmp_path, **fields)
    assert "error" not in result, result
    assert len(result["prompts"]) == 1, result
    prompt = result["prompts"][0]["prompt"]
    expected = fields.get("github_repo") or fields["plan"]["repo"]
    assert f"GitHub repository: {expected}" in prompt, prompt


@pytest.mark.parametrize("fields", [
    {},
    {"github_repo": "fork/project", "plan": {"repo": "upstream/project"}},
    {"github_repo": "/absolute/checkout"},
    {"github_repo": ""},
    {"github_repo": None},
    {"github_repo": "fork/.."},
    {"github_repo": "https://REDACT-ME@github.com/fork/project"},
])
def test_missing_invalid_or_conflicting_identity_stops_before_any_agent(
    tmp_path: Path, fields: dict
) -> None:
    result = probe(tmp_path, **fields)
    assert "GitHub repository" in result.get("error", ""), result
    assert result["prompts"] == [], result
    assert "REDACT-ME" not in json.dumps(result)


def test_missing_repository_points_at_a_stale_router_skill(tmp_path: Path) -> None:
    """#2437: only a stale /grind router omits the repository, so say so."""
    error = probe(tmp_path).get("error", "")
    assert "GitHub repository is missing" in error, error
    assert "skills/grind/SKILL.md" in error, error
    assert "managed-by: clud" in error, error


def test_watcher_timeout_is_not_an_arbitrary_two_observation_cutoff() -> None:
    text = (ROOT / "crates/clud-bin/assets/skills/grind-land/SKILL.md").read_text(
        encoding="utf-8"
    )
    assert "a second timeout is `gave_up`" not in text
    assert "A second\n     watchdog stop is `gave_up`" not in text
    assert "same CI" in text


def test_every_workflow_role_receives_the_selected_repository(tmp_path: Path) -> None:
    result = probe(
        tmp_path, planOnly=False, mode="sequential", github_repo="fork/project",
        plan={"schema": "grind-plan/v1", "run_id": "1897", "meta": "10",
              "repo": "fork/project", "stages": []},
        feature={"branch": "feature", "worktree": str(tmp_path), "pr": "34"},
        feature_merge="auto",
    )
    assert "error" not in result, result
    assert {item["role"] for item in result["prompts"]} == {
        "grind-prework", "grind-planner", "grind-worker", "grind-reviewer",
        "grind-integrator", "grind-lander",
    }, result
    for item in result["prompts"]:
        assert "GitHub repository: fork/project" in item["prompt"], item
        starts = list(re.finditer(
            r"\bgh (?:pr|issue|label) (?:view|edit|comment|create|merge|ready)\b",
            item["prompt"],
        ))
        for index, match in enumerate(starts):
            end = starts[index + 1].start() if index + 1 < len(starts) else len(item["prompt"])
            assert "--repo fork/project" in item["prompt"][match.start():end], item
    feature = next(item for item in result["prompts"] if item["label"] == "land:feature")
    commands = re.findall(r"(?m)^\d+\. (gh .+)$", feature["prompt"])
    assert len(commands) == 2, feature
    for command in commands:
        words = shlex.split(command.partition(" (")[0])
        assert words[words.index("--repo") + 1] == "fork/project", command


def isolated_env(tmp_path: Path) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("CLUD_")}
    env.update(HOME=str(tmp_path), USERPROFILE=str(tmp_path), CLUD_HOOK_HOME=str(tmp_path),
               CLUD_SKIP_RM_IDENTITY="1", CLUD_NO_UNLOCK="1", CLUD_SERVER_SETTINGS="0",
               CLAUDE_CODE_SESSION_ID="1897-regression")
    return env


def scan_role(tmp_path: Path, role: str, command: str) -> process.CompletedProcess[str]:
    facts = tmp_path / ".clud/tmp/grind/1897-regression.json"
    facts.parent.mkdir(parents=True, exist_ok=True)
    facts.write_text(json.dumps({"meta": "10", "mode": "sequential"}), encoding="utf-8")
    payload = {"hook_event_name": "PreToolUse", "session_id": "1897-regression",
               "agent_type": role, "tool_name": "Bash",
               "tool_input": {"command": command}, "cwd": str(tmp_path)}
    return process.run(
        [os.environ["CLUD_TEST_BINARY"], "__cmd-scan", "--event", "PreToolUse"],
        input=json.dumps(payload), env=isolated_env(tmp_path), cwd=tmp_path,
        capture_output=True, text=True, timeout=20,
    )


@pytest.mark.parametrize("role", ["grind-planner", "grind-lander", "grind-prework"])
def test_documented_scoped_reads_reach_the_real_hook(tmp_path: Path, role: str) -> None:
    result = scan_role(tmp_path, role, "gh issue view 12 --comments --repo fork/project")
    assert result.returncode == 0, result.stdout + result.stderr
    denied = scan_role(tmp_path, role, "gh issue close 12 --repo fork/project")
    assert denied.returncode == 2, denied.stdout + denied.stderr


def test_prework_preserves_parentheses_and_keeps_meta_scope(tmp_path: Path) -> None:
    command = ("printf '%s' 'plan (keep parentheses)' | "
               "gh issue comment 10 --repo fork/project --body-file -")
    result = scan_role(tmp_path, "grind-prework", command)
    assert result.returncode == 0, result.stdout + result.stderr
    denied = scan_role(tmp_path, "grind-prework", command.replace("comment 10", "comment 11"))
    assert denied.returncode == 2, denied.stdout + denied.stderr


def test_grind_facts_cli_preserves_files_after_the_separator(tmp_path: Path) -> None:
    binary = os.environ["CLUD_TEST_BINARY"]
    result = process.run(
        [binary, "grind-facts", "task", "--checkout", str(tmp_path), "--",
         "src/a.rs", "-leading.rs"],
        env=isolated_env(tmp_path), cwd=tmp_path, capture_output=True, text=True, timeout=20,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    facts = json.loads((tmp_path / ".clud/tmp/grind/1897-regression.json").read_text())
    assert facts["tasks"] == [{"worktree": str(tmp_path), "files": ["src/a.rs", "-leading.rs"]}]


def test_lander_command_targets_the_fork_instead_of_the_upstream_default(tmp_path: Path) -> None:
    for argv in [
        ["git", "init", "-q"],
        ["git", "remote", "add", "origin", "https://github.com/fork/project.git"],
        ["git", "remote", "add", "upstream", "https://github.com/upstream/project.git"],
    ]:
        setup = process.run(argv, cwd=tmp_path, env=isolated_env(tmp_path),
                            capture_output=True, text=True, timeout=5)
        assert setup.returncode == 0, setup.stderr
    text = (ROOT / "crates/clud-bin/assets/skills/grind-land/SKILL.md").read_text()
    snippets = re.findall(r"(?<!`)`([^`]+)`(?!`)", text, re.DOTALL)
    sample = next(code for code in snippets if code.startswith("gh pr merge"))
    words = shlex.split(sample.replace("<github-repo>", "fork/project").replace("<n>", "34"))
    assert words[0:3] == ["gh", "pr", "merge"]
    allowed = scan_role(tmp_path, "grind-lander", shlex.join(words))
    assert allowed.returncode == 0, allowed.stdout + allowed.stderr
    # A controlled gh process models the observed fork's upstream default.
    # It records the actual arguments extracted from the shipping procedure.
    fixture = tmp_path / "gh_fixture.py"
    fixture.write_text(
        "import configparser, json, sys\n"
        "config = configparser.ConfigParser()\n"
        "config.read('.git/config')\n"
        "upstream = config['remote \\\"upstream\\\"']['url']\n"
        "default = upstream.removeprefix('https://github.com/').removesuffix('.git')\n"
        "args = sys.argv[1:]\n"
        "repo = args[args.index('--repo') + 1] if '--repo' in args else default\n"
        "print(json.dumps({'repo': repo, 'args': args}))\n", encoding="utf-8",
    )
    result = process.run(
        [sys.executable, str(fixture), *words[1:]], cwd=tmp_path,
        capture_output=True, text=True, timeout=5,
    )
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout)["repo"] == "fork/project"
    unscoped = list(words[1:])
    index = unscoped.index("--repo")
    del unscoped[index:index + 2]
    control = process.run(
        [sys.executable, str(fixture), *unscoped], cwd=tmp_path,
        capture_output=True, text=True, timeout=5,
    )
    assert control.returncode == 0, control.stderr
    assert json.loads(control.stdout)["repo"] == "upstream/project"


def test_repeated_bounded_observations_resume_the_same_ci_until_green(tmp_path: Path) -> None:
    """Controlled procedure execution, without a model or GitHub requests."""
    harness = ROOT / "tests/fixtures/gh_watch_harness.py"
    args = [sys.executable, str(harness), "--", "123", "--repo", "zackees/clud",
            "--require", ".*", "--no-cancel", "--timeout", "540"]
    for scenario, expected in [("timeout", 4), ("timeout", 4), ("green", 0)]:
        env = isolated_env(tmp_path)
        env.update(GH_WATCH_SCENARIO=scenario, GH_WATCH_CLUD_BINARY=os.environ["CLUD_TEST_BINARY"],
                   GH_WATCH_CANCEL_LOG=str(tmp_path / "cancels"))
        result = process.run(args, env=env, cwd=tmp_path,
                             capture_output=True, text=True, timeout=15)
        assert result.returncode == expected, result.stdout + result.stderr
    logs = sorted((tmp_path / ".clud/logs/pr-merge-watch/zackees__clud").glob("*.jsonl"))
    assert len(logs) == 3
    for log in logs:
        start = json.loads(log.read_text().splitlines()[0])
        assert start["repo"] == "zackees/clud"
        assert start["pr"] == 123
    assert not (tmp_path / "cancels").exists()


# #1830: identical input flipped a lone child between bug and feature.
def _classify(children: list[dict], groups: list[dict]) -> dict:
    return {"confident": True, "children": children, "groups": groups,
            "order": [c["id"] for c in children]}


def test_a_lone_child_always_takes_the_bug_track(tmp_path: Path) -> None:
    reply = _classify([{"id": "12", "track": "feature", "group": "docs", "read": True}],
                      [{"name": "docs", "independent": True, "children": ["12"]}])
    result = probe(tmp_path, classify=reply, github_repo="fork/project")["result"]
    assert result["lines"] == ["#12 bug → main"], result
    assert result["classification"]["groups"] == []


def test_an_unread_feature_takes_the_bug_track_and_a_read_one_stays(tmp_path: Path) -> None:
    goals = [{"id": n, "title": "t", "brief": "b"} for n in ("12", "13")]
    reply = _classify(
        [{"id": "12", "track": "feature", "group": "g", "read": False},
         {"id": "13", "track": "feature", "group": "g", "read": True}],
        [{"name": "g", "independent": True, "children": ["12", "13"]}])
    result = probe(tmp_path, classify=reply, goals=goals, github_repo="fork/project")["result"]
    assert result["lines"] == ["#12 bug → main", "#13 feature → grind/meta-10-g"], result
    assert result["classification"]["groups"][0]["children"] == ["13"]
