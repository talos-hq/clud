"""`/grind` and a repo's `./lint` / `./test` on the real Claude Code (#1336).

The `/grind` router carries a `` !`clud grind-scripts` `` line, so the model's
first request holds the detected scripts, their run commands and the files to
read for modes (L1-L4). The router records the one answer in
the session's run facts and passes it to `grind-run` as `args.scripts`; the
workflow puts the commands, in order, into every integrator prompt (L5, L11,
L12), and the integrator runs them before each push (L6, L8-L10). Workers
still cannot run them (L7).

The fixture scripts append one JSON line per run to `logs/ran.jsonl`, so a
test sees what actually executed; the hook log gives the order of the tool
calls around it (the `git push`).
"""

from __future__ import annotations

import json
import sys
from typing import Any

from tests.harness.harness import Harness, RunResult
from tests.harness.test_grind import (
    _after,
    _bash,
    _by_role,
    _goal_roles,
    _goals,
    _merged,
    _no_notes,
    _script,
    _structured,
)

GRIND = "/grind https://github.com/o/r/issues/1"
ACK = {"default_text": "ACK", "roles": [{"name": "main", "steps": [{"text": "ACK"}]}]}
OK = {"is_error": False}
FAILED = {"is_error": True}
SCRIPTS = {"lint": "bash ./lint", "test": "bash ./test --integration"}

CI_TEST = """import argparse
import json
import sys

parser = argparse.ArgumentParser(description="repo test entry point")
parser.add_argument("--integration", action="store_true", help="also run the integration suite")
args, _ = parser.parse_known_args()
with open({log!r}, "a", encoding="utf-8") as fh:
    fh.write(json.dumps({{"name": "test", "args": " ".join(sys.argv[1:])}}) + "\\n")
sys.exit(0)
"""


# ---- world builders ------------------------------------------------------------


def _body(h: Harness, kind: str, *, code: int, sleep_s: int, pass_if: str | None) -> str:
    log = h.logs / "ran.jsonl"
    lines = ["#!/bin/bash", 'cd "$(dirname "$0")"']
    if sleep_s:
        lines.append(f"sleep {sleep_s}")
    if pass_if:
        lines.append(f"if [ -f {pass_if} ]; then rc=0; else rc=1; fi")
    else:
        lines.append(f"rc={code}")
    lines.append(f'printf \'{{"name":"{kind}","args":"%s","rc":%s}}\\n\' "$*" "$rc" >> "{log}"')
    lines.append(f'echo "$rc" > "{h.logs / (kind + ".exit")}"')
    lines.append('exit "$rc"')
    return "\n".join(lines) + "\n"


def make_scripts(
    h: Harness,
    *,
    lint: bool = True,
    test: bool = True,
    kind: str = "plain",
    executable: bool = True,
    delegate_to: bool = False,
    test_code: int = 0,
    sleep_s: int = 0,
    test_pass_if: str | None = None,
    lint_pass_if: str | None = None,
) -> None:
    """Write the repo's entry scripts, commit them and push them to origin/main."""
    ext = {"plain": "", "sh": ".sh", "bat": ".bat", "ps1": ".ps1"}[kind]
    written: list[str] = []
    if lint:
        name = f"lint{ext}"
        (h.repo / name).write_text(
            _body(h, "lint", code=0, sleep_s=0, pass_if=lint_pass_if), encoding="utf-8"
        )
        written.append(name)
    if test:
        name = f"test{ext}"
        if delegate_to:
            (h.repo / "ci").mkdir(exist_ok=True)
            (h.repo / "ci" / "test.py").write_text(
                CI_TEST.format(log=str(h.logs / "ran.jsonl")), encoding="utf-8"
            )
            written.append("ci/test.py")
            body = f'#!/bin/bash\ncd "$(dirname "$0")"\nexec "{sys.executable}" ci/test.py "$@"\n'
        else:
            body = _body(h, "test", code=test_code, sleep_s=sleep_s, pass_if=test_pass_if)
        (h.repo / name).write_text(body, encoding="utf-8")
        written.append(name)
    for name in written:
        (h.repo / name).chmod(0o755 if executable else 0o644)
    h.git("add", *written)
    h.git("commit", "-q", "-m", "add scripts")
    h.git("push", "-q", "origin", "main")


def _record_run_json(h: Harness, scripts: dict[str, str]) -> None:
    """What the router writes once the user answers (not committed)."""
    path = h.run_facts_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"verify": scripts}, indent=1), encoding="utf-8")


def _ran(h: Harness) -> list[dict[str, Any]]:
    path = h.logs / "ran.jsonl"
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line]


# ---- scripted roles ------------------------------------------------------------


def _in_repo(h: Harness, command: str) -> str:
    return f"cd {h.repo} && {command}"


def _push_cmd(h: Harness, goal: str, *, branch_exists: bool = False) -> str:
    repo, branch = str(h.repo), f"grind/{goal}"
    start = (
        f"git -C {repo} switch -q {branch}"
        if branch_exists
        else f"git -C {repo} switch -q main && git -C {repo} switch -q -c {branch}"
    )
    return (
        f"{start} && git -C {repo} add {goal}.txt && git -C {repo} commit -q -m {goal} && "
        f"git -C {repo} push -q -u origin {branch} && gh pr create --title {goal} --head {branch}"
    )


def _pushed(goal: str) -> dict[str, Any]:
    url = f"https://github.com/o/r/pull/grind/{goal}"
    return _structured({"pushed": True, "pr_url": url, "summary": "pushed"})


def _verify_then_push(h: Harness, goal: str) -> list[dict[str, Any]]:
    """Integrator: lint, then push, each checked green (DD-205: CI runs the suites)."""
    return [
        _bash(_in_repo(h, SCRIPTS["lint"])),
        _after(OK, _bash(_push_cmd(h, goal))),
        _after(OK, _pushed(goal)),
    ]


def _set_steps(roles: list[dict[str, Any]], name: str, steps: list[dict[str, Any]]) -> None:
    for role in roles:
        if role["name"] == name:
            role["steps"] = steps
            return
    raise AssertionError(f"no role {name}")


def _integrator_bash(result: RunResult) -> list[str]:
    return [
        h["tool_input"]["command"]
        for h in result.hooks
        if h.get("event") == "PreToolUse"
        and h.get("agent_type") == "grind-integrator"
        and h.get("tool_name") == "Bash"
    ]


def _index(commands: list[str], needle: str, start: int = 0) -> int:
    for i in range(start, len(commands)):
        if needle in commands[i]:
            return i
    raise AssertionError(f"{needle!r} not run after #{start}: {commands}")


def _grind(h: Harness, roles: list[dict[str, Any]], **args: Any) -> RunResult:
    extra_env = args.pop("extra_env", None)
    script = _script(h, "sequential", _goals("g"), roles, **args)
    result = h.run("start grind", script, timeout=300, extra_env=extra_env)
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    return result


# ---- L1-L4: what the router is given -------------------------------------------


def _router_text(h: Harness) -> str:
    result = h.run(GRIND, ACK)
    assert result.returncode == 0, result.stdout[-2000:]
    text = result.prompt_text("main")
    assert "rendered by `clud grind-scripts`" in text, text[:3000]
    assert "!`clud grind-scripts`" not in text
    return text


def test_l1_executable_scripts_render_direct_run_commands(harness: Harness) -> None:
    make_scripts(harness, executable=True)
    text = _router_text(harness)
    assert "- lint: `./lint`" in text
    assert "- test: `./test`" in text
    assert "Found `./lint` and `./test`. Run them before each push?" in text
    # One combined question block, not one per script.
    assert text.count("Found `./lint` and `./test`. Run them before each push?") == 1
    assert "bash ./lint" not in text


def test_l2_non_executable_scripts_run_through_bash(harness: Harness) -> None:
    make_scripts(harness, executable=False)
    text = _router_text(harness)
    assert "- lint: `bash ./lint`" in text
    assert "- test: `bash ./test`" in text


def test_l3_delegated_file_is_listed_and_read_by_the_router(harness: Harness) -> None:
    make_scripts(harness, lint=False, delegate_to=True)
    repo = harness.repo
    script = {
        "default_text": "ACK",
        "roles": [
            {
                "name": "main",
                "steps": [
                    {"tool_use": {"name": "Read", "input": {"file_path": f"{repo}/test"}}},
                    _after(
                        {"is_error": False, "content_contains": "ci/test.py"},
                        {
                            "tool_use": {
                                "name": "Read",
                                "input": {"file_path": f"{repo}/ci/test.py"},
                            }
                        },
                    ),
                    _after(
                        {"is_error": False, "content_contains": "--integration"}, {"text": "READ"}
                    ),
                ],
            }
        ],
    }
    result = harness.run(GRIND, script)
    assert result.returncode == 0, result.stdout[-2000:]
    assert "READ" in result.stdout, result.stdout[-2000:]
    _no_notes(result)
    text = result.prompt_text("main")
    assert "- read for modes: `test`, `ci/test.py`" in text, text[:3000]
    assert "Never execute the scripts" in text
    reads = [
        h["tool_input"]["file_path"]
        for h in result.hooks
        if h.get("event") == "PreToolUse" and h.get("tool_name") == "Read"
    ]
    assert reads == [f"{repo}/test", f"{repo}/ci/test.py"]
    # Reading for modes never runs the script.
    assert _ran(harness) == []


def test_l4_only_test_bat_on_linux_detects_nothing(harness: Harness) -> None:
    make_scripts(harness, lint=False, kind="bat", executable=False)
    text = _router_text(harness)
    assert "No ./lint or ./test detected" in text
    assert "Skip the scripts question" in text
    assert "Run it before each push?" not in text
    assert "test.bat" not in text


# ---- L5, L11, L12: what the integrator is told ---------------------------------


def test_l5_integrator_prompt_runs_focused_test_then_lint_not_tests(harness: Harness) -> None:
    make_scripts(harness, delegate_to=True)
    _record_run_json(harness, SCRIPTS)
    roles = _goal_roles(harness, "g")
    result = _grind(harness, roles, scripts=SCRIPTS)
    text = result.prompt_text("integrator:g")
    focused = text.index("1. planner's focused test:")
    lint = text.index("2. bash ./lint")
    gate = text.index("MULTI-PLATFORM LINT GATE")
    assert focused < lint < gate, text[:3000]
    assert "bash ./test" not in text
    # The planner was told not to invent lint/test commands.
    assert "do not add lint or test commands" in result.prompt_text("planner:g")
    assert _merged(harness) == ["grind/g"]


def test_l11_neither_keeps_the_planners_verify_commands(harness: Harness) -> None:
    make_scripts(harness)
    _record_run_json(harness, {})
    roles = _goal_roles(harness, "g")
    focused = "python -m pytest tests/test_focus.py"
    for role in roles:
        if role["name"] == "planner:g":
            role["steps"][0]["tool_use"]["input"]["verify"] = focused
    result = _grind(harness, roles, scripts={})
    text = result.prompt_text("integrator:g")
    assert f"Verify commands:\\n  {focused}" in text, text[:3000]
    assert "./lint" not in text
    assert "planner's focused test" not in text
    assert "do not add lint or test commands" not in result.prompt_text("planner:g")
    assert _ran(harness) == []


def test_l12_fix_round_reruns_the_scripts_before_its_push(harness: Harness) -> None:
    make_scripts(harness, executable=False)
    scripts = {"lint": "bash ./lint", "test": "bash ./test"}
    roles = _goal_roles(harness, "g", land=["needs_fix", "merged"])
    first = [
        _bash(_in_repo(harness, scripts["lint"])),
        _after(OK, _bash(_push_cmd(harness, "g"))),
        _after(OK, _pushed("g")),
    ]
    fix = [
        _bash(_in_repo(harness, scripts["lint"])),
        _after(OK, _bash(f"git -C {harness.repo} push -q origin grind/g")),
        _after(OK, _pushed("g")),
    ]
    _set_steps(roles, "integrator:g", first)
    _set_steps(roles, "chaperone-fix:g:1", fix)
    result = _grind(harness, roles, scripts=scripts)
    fix_prompt = result.prompt_text("chaperone-fix:g:1")
    assert "FIX ROUND 1:" in fix_prompt
    assert "2. bash ./lint" in fix_prompt
    assert "MULTI-PLATFORM LINT GATE" in fix_prompt
    assert [r["name"] for r in _ran(harness)] == ["lint", "lint"]
    assert _merged(harness) == ["grind/g"]


# ---- L6-L10: what actually runs ------------------------------------------------


def test_l6_integrator_runs_lint_before_the_push(harness: Harness) -> None:
    make_scripts(harness, delegate_to=True, executable=False)
    _record_run_json(harness, SCRIPTS)
    roles = _goal_roles(harness, "g")
    _set_steps(roles, "integrator:g", _verify_then_push(harness, "g"))
    result = _grind(harness, roles, scripts=SCRIPTS)
    ran = _ran(harness)
    assert [(r["name"], r["args"]) for r in ran] == [("lint", "")]
    cmds = _integrator_bash(result)
    lint = _index(cmds, "bash ./lint")
    push = _index(cmds, "git -C", lint + 1)
    assert lint < push
    assert _merged(harness) == ["grind/g"]


def test_l7_worker_cannot_run_the_scripts(harness: Harness) -> None:
    make_scripts(harness, executable=False)
    repo = str(harness.repo)
    denied = {"is_error": True, "content_contains": "/grind role caps"}
    write = {"file_path": f"{repo}/g.txt", "content": "g\n"}
    worker = [
        _bash(_in_repo(harness, "bash ./test")),
        _after(denied, _bash(_in_repo(harness, "bash ./lint"))),
        _after(denied, {"tool_use": {"name": "Write", "input": write}}),
        _after(OK, _structured({"files_touched": ["g.txt"], "summary": "wrote it"})),
    ]
    roles = _goal_roles(harness, "g", worker_steps=worker)
    result = _grind(harness, roles, scripts={"lint": "bash ./lint", "test": "bash ./test"})
    bash = [
        h["tool_input"]["command"]
        for h in result.hooks
        if h.get("agent_type") == "grind-editor" and h.get("tool_name") == "Bash"
    ]
    # Claude Code strips a leading `cd <cwd> &&` from the recorded command.
    assert bash == ["bash ./test", "bash ./lint"]
    assert _ran(harness) == []
    assert _merged(harness) == ["grind/g"]


def test_l8_slow_test_runs_in_background_and_its_exit_code_decides(harness: Harness) -> None:
    # Bash's limit is lowered to 5 s; the test takes 7 s.
    make_scripts(harness, sleep_s=7)
    scripts = {"lint": "./lint", "test": "./test"}
    exit_file = harness.logs / "test.exit"
    background = {
        "tool_use": {
            "name": "Bash",
            "input": {
                "command": _in_repo(harness, "./test"),
                "description": "grind",
                "run_in_background": True,
            },
        }
    }
    wait = _bash(f"sleep 4; cat {exit_file} 2>/dev/null || echo pending")
    steps = [
        _bash(_in_repo(harness, "./lint")),
        _after(OK, background),
        _after(OK, wait),
        _after(OK, wait),
        _after(OK, _bash(f"cat {exit_file}")),
        _after({"is_error": False, "content_contains": "0"}, _bash(_push_cmd(harness, "g"))),
        _after(OK, _pushed("g")),
    ]
    roles = _goal_roles(harness, "g")
    _set_steps(roles, "integrator:g", steps)
    env = {"BASH_DEFAULT_TIMEOUT_MS": "5000", "BASH_MAX_TIMEOUT_MS": "5000"}
    result = _grind(harness, roles, scripts=scripts, extra_env=env)
    ran = _ran(harness)
    assert [(r["name"], r["rc"]) for r in ran] == [("lint", 0), ("test", 0)]
    assert exit_file.read_text(encoding="utf-8").strip() == "0"
    started = [
        h
        for h in result.hooks
        if h.get("agent_type") == "grind-integrator"
        and h.get("tool_name") == "Bash"
        and h["tool_input"].get("run_in_background")
    ]
    assert [h["tool_input"]["command"] for h in started] == ["./test"]
    cmds = _integrator_bash(result)
    assert _index(cmds, "./test") < _index(cmds, "push -q -u origin")
    assert _merged(harness) == ["grind/g"]


def test_l9_a_failing_test_loops_inside_the_integrator(harness: Harness) -> None:
    make_scripts(harness, test_pass_if="fixed.flag")
    scripts = {"lint": "./lint", "test": "./test"}
    steps = [
        _bash(_in_repo(harness, "./lint")),
        _after(OK, _bash(_in_repo(harness, "./test"))),
        _after(FAILED, _bash(f"touch {harness.repo}/fixed.flag")),
        _after(OK, _bash(_in_repo(harness, "./test"))),
        _after(OK, _bash(_push_cmd(harness, "g"))),
        _after(OK, _pushed("g")),
    ]
    roles = _goal_roles(harness, "g")
    _set_steps(roles, "integrator:g", steps)
    result = _grind(harness, roles, scripts=scripts)
    tests = [r["rc"] for r in _ran(harness) if r["name"] == "test"]
    assert tests == [1, 0]
    cmds = _integrator_bash(result)
    second = _index(cmds, "./test", _index(cmds, "fixed.flag") + 1)
    assert second < _index(cmds, "push -q -u origin")
    ran = _by_role(result)
    assert "chaperone-fix:g:1" not in ran
    assert "chaperone:g:1" not in ran
    assert result.prompt_text("chaperone:g:0").count("Follow your WATCH") == 1
    assert _merged(harness) == ["grind/g"]


def test_l10_red_main_is_fixed_in_its_own_commit_ahead_of_the_goal(harness: Harness) -> None:
    # red_on_main("lint"): origin/main's lint fails until lint.ok exists.
    make_scripts(harness, lint_pass_if="lint.ok")
    scripts = {"lint": "./lint", "test": "./test"}
    repo = str(harness.repo)
    title = "fix: pre-existing lint failure on main"
    fix_main = (
        f"git -C {repo} switch -q main && git -C {repo} switch -q -c grind/g && "
        f"touch {repo}/lint.ok && git -C {repo} add lint.ok && "
        f"git -C {repo} commit -q -m '{title}'"
    )
    steps = [
        _bash(_in_repo(harness, "./lint")),
        _after(FAILED, _bash(fix_main)),
        _after(OK, _bash(_in_repo(harness, "./lint"))),
        _after(OK, _bash(_in_repo(harness, "./test"))),
        _after(OK, _bash(_push_cmd(harness, "g", branch_exists=True))),
        _after(OK, _pushed("g")),
    ]
    roles = _goal_roles(harness, "g")
    _set_steps(roles, "integrator:g", steps)
    _grind(harness, roles, scripts=scripts)
    subjects = harness.git("log", "--format=%s", "origin/main..origin/grind/g").splitlines()
    assert subjects == ["g", title], subjects
    assert [(r["name"], r["rc"]) for r in _ran(harness)] == [("lint", 1), ("lint", 0), ("test", 0)]
