"""End-to-end coverage for `bash.block_cd` in the cmd-scan hook.

zackees/clud#967 Phase 1. The Rust decision table is asserted in
`block_bad_cmd_cd_tests.rs`; these tests check the parts only a real process
exercises — that settings and hook configs are discovered from the payload's
cwd, and that a denial reaches the caller as a hook-protocol exit 2.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
from pathlib import Path

from tests import process

SENSITIVE_HOOK = json.dumps(
    {
        "hooks": {
            "Stop": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": "uv run python ci/hooks/check-on-stop.py",
                        }
                    ]
                }
            ]
        }
    }
)


def _binary_name(name: str) -> str:
    return f"{name}.exe" if sys.platform == "win32" else name


def _cmd_scan_binary() -> Path:
    clud_binary = os.environ.get("CLUD_TEST_BINARY")
    if clud_binary:
        sibling = Path(clud_binary).with_name(_binary_name("clud-cmd-scan"))
        if sibling.is_file():
            return sibling

    resolved = shutil.which(_binary_name("clud-cmd-scan"))
    if resolved:
        return Path(resolved)

    raise AssertionError("clud-cmd-scan test binary not found")


def _make_repo(
    tmp_path: Path,
    *,
    hooks: str | None,
    settings: str | None = None,
    hooks_json: str | None = None,
) -> Path:
    repo = tmp_path / "repo"
    (repo / ".git").mkdir(parents=True)
    (repo / "src").mkdir()
    if hooks is not None:
        claude = repo / ".claude"
        claude.mkdir()
        (claude / "settings.json").write_text(hooks, encoding="utf-8")
    if settings is not None:
        clud = repo / ".clud"
        clud.mkdir()
        (clud / "settings.json").write_text(settings, encoding="utf-8")
    if hooks_json is not None:
        clud = repo / ".clud"
        clud.mkdir()
        (clud / "hooks.json").write_text(hooks_json, encoding="utf-8")
    return repo


def _run(
    tmp_path: Path,
    repo: Path,
    command: str,
    *,
    extra_env: dict[str, str] | None = None,
) -> process.CompletedProcess[str]:
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    env = os.environ.copy()
    # An isolated HOME keeps the developer's own user-level settings and hook
    # configs out of the resolution.
    env["HOME"] = str(home)
    env["USERPROFILE"] = str(home)
    env.pop("CLUD_BAD_CMD_OVERRIDE", None)
    if extra_env:
        env.update(extra_env)
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "cwd": str(repo),
            "tool_input": {"command": command},
        }
    )
    return process.run(
        [str(_cmd_scan_binary())],
        input=payload,
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
    )


def test_cwd_sensitive_hook_makes_auto_deny_an_in_repo_cd(tmp_path: Path) -> None:
    # The wedge from the issue: a relative-path Stop hook plus an in-repo cd.
    repo = _make_repo(tmp_path, hooks=SENSITIVE_HOOK)

    result = _run(tmp_path, repo, "cd src && ls")

    assert result.returncode == 2, result.stdout + result.stderr
    assert "deny" in result.stdout
    assert "(cd DIR && CMD)" in result.stdout
    assert "check-on-stop.py" in result.stdout, "the denial names the hook that earned it"


def test_a_subshell_cd_is_allowed_under_the_same_policy(tmp_path: Path) -> None:
    # The workaround the denial recommends has to actually work.
    repo = _make_repo(tmp_path, hooks=SENSITIVE_HOOK)

    result = _run(tmp_path, repo, "(cd src && ls)")

    assert result.returncode == 0, result.stdout + result.stderr


def test_a_repo_without_hooks_sees_no_behavior_change(tmp_path: Path) -> None:
    repo = _make_repo(tmp_path, hooks=None)

    result = _run(tmp_path, repo, "cd /somewhere/else && ls")

    assert result.returncode == 0, result.stdout + result.stderr


def test_path_binary_hooks_resolve_to_escape_only(tmp_path: Path) -> None:
    hooks = json.dumps(
        {
            "hooks": {
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"command": "clud-cmd-scan"}]}
                ]
            }
        }
    )
    repo = _make_repo(tmp_path, hooks=hooks)

    inside = _run(tmp_path, repo, "cd src && ls")
    assert inside.returncode == 0, inside.stdout + inside.stderr

    outside = _run(tmp_path, repo, "cd ~ && ls")
    assert outside.returncode == 2, outside.stdout + outside.stderr


def test_settings_can_turn_the_policy_off(tmp_path: Path) -> None:
    repo = _make_repo(
        tmp_path,
        hooks=SENSITIVE_HOOK,
        settings=json.dumps({"bash": {"block_cd": False}}),
    )

    result = _run(tmp_path, repo, "cd src && ls")

    assert result.returncode == 0, result.stdout + result.stderr


def test_override_env_releases_a_single_call(tmp_path: Path) -> None:
    repo = _make_repo(tmp_path, hooks=SENSITIVE_HOOK)

    result = _run(
        tmp_path,
        repo,
        "cd src && ls",
        extra_env={"CLUD_BAD_CMD_OVERRIDE": "block-cd:timing a build from the subdir"},
    )

    assert result.returncode == 0, result.stdout + result.stderr


# ---------------------------------------------------------------------
# Phase 5: "auto" relaxes for a migrated repo.
# ---------------------------------------------------------------------


def test_a_migrated_repo_relaxes_to_allow_in_repo_cd(tmp_path: Path) -> None:
    # Migration earns relaxation (D13): a `.clud/hooks.json` opt-in means the
    # repo's hooks are dispatcher-managed and cwd-immune, however sensitive
    # their command text looks — so "auto" resolves to relaxed, `cd` moves
    # freely inside the registered root, and only an escape is denied.
    repo = _make_repo(
        tmp_path,
        hooks=None,
        hooks_json=json.dumps(
            {
                "hooks": {
                    "Stop": [
                        {"command": "uv run python ci/hooks/check-on-stop.py"}
                    ]
                }
            }
        ),
    )

    inside = _run(tmp_path, repo, "cd src && ls")
    assert inside.returncode == 0, inside.stdout + inside.stderr

    escaping = _run(tmp_path, repo, "cd ~ && ls")
    assert escaping.returncode == 2, escaping.stdout + escaping.stderr
    assert "deny" in escaping.stdout


def test_a_migrated_repo_that_keeps_a_sensitive_raw_hook_stays_strict(
    tmp_path: Path,
) -> None:
    # The harness fires raw frontend-settings hooks unrooted, so a sensitive
    # one keeps the repo strict even after the migration — the opt-in cannot
    # mask it.
    repo = _make_repo(
        tmp_path,
        hooks=SENSITIVE_HOOK,
        hooks_json=json.dumps(
            {
                "hooks": {
                    "Stop": [
                        {"command": "uv run python ci/hooks/check-on-stop.py"}
                    ]
                }
            }
        ),
    )

    result = _run(tmp_path, repo, "cd src && ls")

    assert result.returncode == 2, result.stdout + result.stderr
    assert "check-on-stop.py" in result.stdout


def test_relaxed_movement_judges_against_the_whole_registered_root_set(
    tmp_path: Path,
) -> None:
    # Relaxed pins the session to the registered trees — parent and extern
    # alike (Phase 3 roots) — so a move into a granted sibling is allowed and
    # only an escape of all of them is denied.
    repo = _make_repo(
        tmp_path,
        hooks=None,
        hooks_json=json.dumps(
            {"hooks": {"Stop": [{"command": "check-on-stop.py"}]}}
        ),
    )
    extern = tmp_path / "extern" / "dep"
    extern.mkdir(parents=True)
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    roots = json.dumps(
        [
            {"kind": "parent", "path": str(repo)},
            {"kind": "extern", "path": str(extern)},
        ]
    )

    into_extern = _run(
        tmp_path,
        repo,
        f"cd {extern} && ls",
        extra_env={"CLUD_HOOK_ROOTS": roots},
    )
    assert into_extern.returncode == 0, into_extern.stdout + into_extern.stderr

    escaping = _run(
        tmp_path,
        repo,
        f"cd {elsewhere} && ls",
        extra_env={"CLUD_HOOK_ROOTS": roots},
    )
    assert escaping.returncode == 2, escaping.stdout + escaping.stderr
    assert "deny" in escaping.stdout


def test_issue_2571_rewrites_every_call_but_condenses_the_notice(tmp_path: Path) -> None:
    repo = _make_repo(tmp_path, hooks=SENSITIVE_HOOK)
    home = tmp_path / "home"
    home.mkdir()
    env = os.environ.copy()
    env.update(
        HOME=str(home),
        USERPROFILE=str(home),
        CLUD_DAEMON_STATE_DIR=str(tmp_path / "state"),
        CLAUDE_PROJECT_DIR=str(repo),
    )
    env.pop("CLUD_BAD_CMD_OVERRIDE", None)
    env.pop("CLUD_ALLOW_ALL_CMDS", None)
    commands = ["cd src && echo first", "cd src && echo second", "cd src && echo fresh"]
    for index, command in enumerate(commands):
        payload = json.dumps(
            {
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "cwd": str(repo),
                "session_id": "fresh" if index == 2 else "repeat",
                "permission_mode": "bypassPermissions",
                "tool_input": {"command": command},
            }
        )
        result = process.run(
            [str(_cmd_scan_binary())],
            input=payload,
            capture_output=True,
            text=True,
            env=env,
            timeout=30,
        )
        assert result.returncode == 0, result.stderr
        output = json.loads(result.stdout)["hookSpecificOutput"]
        assert output["permissionDecision"] == "allow"
        assert output["updatedInput"]["command"] == f"({command})"
        notice = output["additionalContext"]
        if index == 1:
            assert notice == (
                "[clud] cd ran in a subshell to preserve the session cwd (bash.block_cd)."
            )
        else:
            assert command in notice
