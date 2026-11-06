"""`safe-rm` on the real Claude Code with a scripted model (#1461).

Agents delete through clud's `safe-rm`: it trashes by default
into `~/.clud/trash`, only inside the session's roots (`CLUD_RM_ROOTS`, set
by the harness as clud sets it), and clud's hook allows a command made only
of it, so Claude Code runs it with no permission prompt. An agent's own
`rm` is rewritten in the hook. The `/grind` caps
narrow the roots per role: a worker deletes only under its task's
directories, the integrator anywhere in its checkout.

Script note: a step's `expect` checks the tool results of the *previous*
step, so an expectation about a command sits on the step after it.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from tests import process
from tests.harness.harness import Harness, RunResult

OK = {"is_error": False}
MARK = {
    "worker": "You are the /grind editor",
    "integrator": "You are the /grind integrator",
}


def _bash(command: str) -> dict[str, Any]:
    return {"tool_use": {"name": "Bash", "input": {"command": command, "description": "rm"}}}


def _after(expect: dict[str, Any], step: dict[str, Any]) -> dict[str, Any]:
    return {**step, "expect": expect}


def _main(steps: list[dict[str, Any]]) -> dict[str, Any]:
    return {"default_text": "OK", "roles": [{"name": "main", "steps": steps}]}


def _no_notes(result: RunResult) -> None:
    notes = [(r["role"], r["note"]) for r in result.requests if r.get("note")]
    assert not notes, notes


def _denials(result: RunResult) -> list[Any]:
    return [
        d
        for e in result.events
        if e.get("type") == "result"
        for d in e.get("permission_denials") or []
    ]


def _trash(h: Harness) -> list[dict[str, Any]]:
    root = h.home / ".clud" / "trash"
    if not root.is_dir():
        return []
    return [
        json.loads((entry / ".clud-rm.json").read_text(encoding="utf-8"))
        for entry in sorted(root.iterdir())
        if (entry / ".clud-rm.json").is_file()
    ]


def _trashed_origins(h: Harness) -> set[str]:
    return {Path(item["origin"]).name for entry in _trash(h) for item in entry["items"]}


def _make(h: Harness, *paths: str) -> None:
    for rel in paths:
        path = h.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(rel, encoding="utf-8")


@pytest.mark.parametrize("skip_permissions", [False, True], ids=["prompts-on", "bypass"])
def test_safe_rm_runs_without_any_prompt_and_lands_in_the_trash(
    harness: Harness, skip_permissions: bool
) -> None:
    h = harness
    _make(h, "build/obj/a.o", "notes.txt")
    script = _main(
        [
            _bash("safe-rm -r build"),
            _after(OK, _bash("safe-rm notes.txt")),
            _after(OK, {"text": "DONE"}),
        ]
    )
    result = h.run("clean up", script, skip_permissions=skip_permissions)
    assert result.returncode == 0, (result.stdout + result.stderr)[-3000:]
    _no_notes(result)
    assert not _denials(result), _denials(result)
    assert not (h.repo / "build").exists()
    assert not (h.repo / "notes.txt").exists()
    assert _trashed_origins(h) == {"build", "notes.txt"}
    entry = _trash(h)[0]
    assert entry["role"] == "agent", entry
    assert entry["session_id"] == h.session_id, entry


def test_an_agents_rm_is_rewritten_and_succeeds(harness: Harness) -> None:
    h = harness
    _make(h, "build/out.bin")
    script = _main(
        [
            _bash("rm -rf build"),
            _after(OK, _bash("test ! -e build")),
            _after(OK, {"text": "DONE"}),
        ]
    )
    result = h.run("clean up", script, skip_permissions=False)
    assert result.returncode == 0, (result.stdout + result.stderr)[-3000:]
    _no_notes(result)
    assert not (h.repo / "build").exists()
    assert _trashed_origins(h) == {"build"}
    transcripts = list((h.home / ".claude" / "projects").rglob("*.jsonl"))
    assert transcripts, "Claude did not write a session transcript"
    rewritten_commands = set()
    for transcript in transcripts:
        for line in transcript.read_text(encoding="utf-8").splitlines():
            entry = json.loads(line)
            attachment = entry.get("attachment") or {}
            if attachment.get("type") != "hook_success":
                continue
            if attachment.get("hookEvent") != "PreToolUse":
                continue
            hook_output = json.loads(attachment["stdout"])
            updated_input = hook_output.get("hookSpecificOutput", {}).get("updatedInput", {})
            command = updated_input.get("command")
            if isinstance(command, str):
                rewritten_commands.add(command)
    assert "safe-rm -rf build" in rewritten_commands


def test_nested_shell_deletion_is_refused_with_safe_command(harness: Harness) -> None:
    h = harness
    _make(h, "build/out.bin")
    script = _main(
        [
            _bash("bash -c 'r" + "m -rf build'"),
            _after({"is_error": True, "content_contains": "safe-rm"}, {"text": "DONE"}),
        ]
    )
    result = h.run("clean up", script)
    assert result.returncode == 0, (result.stdout + result.stderr)[-3000:]
    assert (h.repo / "build/out.bin").is_file()
    assert not _trash(h)


def test_human_cleanup_script_deletes_outside_agent_roots(harness: Harness) -> None:
    h = harness
    cache = h.home / ".cache" / "probe"
    cache.mkdir(parents=True)
    (cache / "old").write_text("remove", encoding="utf-8")
    cleanup = h.repo / "cleanup.sh"
    cleanup.write_text(
        "#!/bin/sh\n" + "r" + 'm -rf "$HOME/.cache/probe"\n', encoding="utf-8"
    )
    cleanup.chmod(0o755)
    script = _main([_bash("./cleanup.sh"), _after(OK, {"text": "DONE"})])
    result = h.run("run the cleanup script", script)
    assert result.returncode == 0, (result.stdout + result.stderr)[-3000:]
    assert not cache.exists()
    logs = list((h.home / ".clud/state/logs/rm").glob("*.jsonl"))
    records = [json.loads(line) for path in logs for line in path.read_text().splitlines()]
    assert any(record["role"] == "child" for record in records), records


def test_human_script_unset_operand_cannot_reach_real_handoff(harness: Harness) -> None:
    h = harness
    _make(h, "keep.txt")
    stub_dir = h.root / "after-shim"
    stub_dir.mkdir()
    handoff_log = h.root / "handoff.log"
    stub = stub_dir / ("r" + "m")
    stub.write_text(
        '#!/bin/sh\n/usr/bin/touch "$RM_HANDOFF_LOG"\n', encoding="utf-8"
    )
    stub.chmod(0o755)
    cleanup = h.repo / "cleanup.sh"
    cleanup.write_text(
        "#!/bin/sh\nunset SP\n" + "r" + 'm -rf "$SP"/\n', encoding="utf-8"
    )
    cleanup.chmod(0o755)
    script = _main(
        [
            _bash("./cleanup.sh"),
            _after({"is_error": True, "content_contains": "filesystem root"}, {"text": "DONE"}),
        ]
    )
    result = h.run(
        "run the cleanup script",
        script,
        extra_env={"PATH": f"{h.bin}:{stub_dir}:{h.path()}", "RM_HANDOFF_LOG": str(handoff_log)},
    )
    assert result.returncode == 0, (result.stdout + result.stderr)[-3000:]
    assert not handoff_log.exists()
    assert (h.repo / "keep.txt").is_file()


def test_clud_injected_deny_blocks_deletion_when_claude_hooks_are_disabled(
    harness: Harness,
) -> None:
    h = harness
    _make(h, "build/out.bin")
    settings_path = h.settings(cmd_scan=False)
    settings = json.loads(settings_path.read_text(encoding="utf-8"))
    settings["disableAllHooks"] = True
    settings_path.write_text(json.dumps(settings), encoding="utf-8")
    script = _main(
        [
            _bash("r" + "m -rf build"),
            _after({"is_error": True, "content_contains": "denied"}, {"text": "DONE"}),
        ]
    )
    with h.backend(script) as base_url:
        result = process.run(
            [
                str(h.clud), "--claude", "--no-daemon", "--subprocess", "-p", "clean up",
                "--", "--settings", str(settings_path), "--session-id", h.session_id,
                "--output-format", "stream-json", "--verbose", "--model", "claude-sonnet-5",
            ],
            cwd=str(h.repo),
            env=h.env({"ANTHROPIC_BASE_URL": base_url}),
            capture_output=True,
            text=True,
            timeout=120,
        )
    assert result.returncode == 0, (result.stdout + result.stderr)[-3000:]
    assert (h.repo / "build/out.bin").is_file()
    assert not _trash(h)


def test_find_exec_safe_rm_runs_without_a_prompt(harness: Harness) -> None:
    h = harness
    _make(h, "build/a.tmp", "build/sub/b.tmp", "build/keep.txt")
    script = _main(
        [
            _bash("find build -name '*.tmp' -exec safe-rm {} +"),
            _after(OK, {"text": "DONE"}),
        ]
    )
    result = h.run("clean up", script, skip_permissions=False)
    assert result.returncode == 0, (result.stdout + result.stderr)[-3000:]
    _no_notes(result)
    assert not _denials(result), _denials(result)
    assert not (h.repo / "build/a.tmp").exists()
    assert not (h.repo / "build/sub/b.tmp").exists()
    assert (h.repo / "build/keep.txt").exists()
    assert _trashed_origins(h) == {"a.tmp", "b.tmp"}


def test_safe_rm_outside_the_session_roots_is_refused(harness: Harness) -> None:
    h = harness
    outside = h.root / "outside.txt"
    outside.write_text("keep", encoding="utf-8")
    script = _main(
        [
            _bash(f"safe-rm {outside}"),
            _after(
                {"is_error": True, "content_contains": "outside the allowed roots"},
                {"text": "DONE"},
            ),
        ]
    )
    result = h.run("clean up", script)
    assert result.returncode == 0, (result.stdout + result.stderr)[-3000:]
    _no_notes(result)
    assert outside.read_text(encoding="utf-8") == "keep"


def test_grind_worker_and_integrator_have_their_own_roots(harness: Harness) -> None:
    h = harness
    _make(h, "src/cache/old.txt", "docs/notes.txt", "build/out.bin")
    h.write_run_facts(
        {"mode": "sequential", "tasks": [{"worktree": str(h.repo), "files": ["src/cache/mod.rs"]}]}
    )

    def agent(kind: str) -> dict[str, Any]:
        return {
            "tool_use": {
                "name": "Agent",
                "input": {
                    "subagent_type": f"grind-{kind}",
                    "description": f"rm {kind}",
                    "prompt": f"RMTEST {kind}",
                },
            }
        }

    worker = {
        "name": "worker",
        "match": MARK["worker"],
        "match_prompt": "RMTEST worker",
        "steps": [
            _bash("safe-rm src/cache/old.txt"),
            _after(OK, _bash("safe-rm docs/notes.txt")),
            _after(
                {"is_error": True, "content_contains": "may delete only under"},
                {"text": "WORKER DONE"},
            ),
        ],
    }
    integrator = {
        "name": "integrator",
        "match": MARK["integrator"],
        "match_prompt": "RMTEST integrator",
        "steps": [_bash("safe-rm -r build"), _after(OK, {"text": "INTEGRATOR DONE"})],
    }
    main = {
        "name": "main",
        "steps": [agent("worker"), agent("integrator"), {"text": "DONE"}],
    }
    script = {"default_text": "OK", "roles": [worker, integrator, main]}
    result = h.run("clean up", script, timeout=300, extra_env={"CLUD_ALLOW_GRIND_AGENTS": "1"})
    assert result.returncode == 0, (result.stdout + result.stderr)[-3000:]
    _no_notes(result)
    roles = {r.get("agent_type") for r in result.hooks if r.get("tool_name") == "Bash"}
    assert roles == {"grind-editor", "grind-integrator"}, roles
    assert not (h.repo / "src/cache/old.txt").exists()
    assert (h.repo / "docs/notes.txt").exists(), "outside the worker's task directories"
    assert not (h.repo / "build").exists()
    assert _trashed_origins(h) == {"old.txt", "build"}
