"""Regression tests for bundled hook stdin handling."""

from __future__ import annotations

import json
import os
import shlex
import shutil
import sys
from pathlib import Path

import pytest

from tests import process

ROOT = Path(__file__).resolve().parent.parent
TELEMETRY = ROOT / "crates" / "clud-bin" / "assets" / "tools" / "hooks" / "telemetry.py"
CLAUDE_SOLDR_HOOK = ROOT / ".claude" / "hooks" / "check-soldr.py"
CODEX_SOLDR_HOOK = ROOT / ".codex" / "hooks" / "check-soldr.py"


def _hook_env(home: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["HOME"] = str(home)
    env["USERPROFILE"] = str(home)
    env["CLUD_HOOK_STDIN_IDLE_TIMEOUT_SEC"] = "0.05"
    env["CLUD_HOOK_STDIN_DEADLINE_SEC"] = "0.25"
    env["CLUD_TELEMETRY_STDIN_IDLE_TIMEOUT_SEC"] = "0.05"
    env["CLUD_TELEMETRY_STDIN_DEADLINE_SEC"] = "0.25"
    return env


def _binary_name(name: str) -> str:
    return f"{name}.exe" if sys.platform == "win32" else name


def _block_bad_cmd_binary() -> Path:
    env_binary = os.environ.get("CLUD_TEST_BLOCK_BAD_CMD_BINARY")
    if env_binary and Path(env_binary).is_file():
        return Path(env_binary)

    clud_binary = os.environ.get("CLUD_TEST_BINARY")
    if clud_binary:
        sibling = Path(clud_binary).with_name(_binary_name("clud-block-bad-cmd"))
        if sibling.is_file():
            return sibling

    resolved = shutil.which(_binary_name("clud-block-bad-cmd"))
    if resolved:
        return Path(resolved)

    raise AssertionError("clud-block-bad-cmd test binary not found")


def _run_hook_with_open_stdin(
    tmp_path: Path,
    payload: str | None,
    argv: list[str] | None = None,
    extra_env: dict[str, str] | None = None,
) -> process.CompletedProcess[str]:
    env = _hook_env(tmp_path / "home")
    if extra_env:
        env.update(extra_env)
    if argv is None:
        argv = [str(_block_bad_cmd_binary())]
    proc = process.Popen(
        argv,
        stdin=process.PIPE,
        stdout=process.PIPE,
        stderr=process.PIPE,
        text=True,
        env=env,
    )
    assert proc.stdin is not None
    assert proc.stdout is not None
    assert proc.stderr is not None
    if payload is not None:
        proc.stdin.write(payload)
        proc.stdin.flush()

    try:
        returncode = proc.wait(timeout=5.0)
    except process.TimeoutExpired:
        proc.kill()
        stdout, stderr = proc.communicate(timeout=1.0)
        raise AssertionError(
            f"hook did not exit while stdin pipe remained open; stdout={stdout!r} stderr={stderr!r}"
        ) from None

    proc.stdin.close()
    stdout = proc.stdout.read()
    stderr = proc.stderr.read()
    return process.CompletedProcess(proc.args, returncode, stdout, stderr)


def test_block_bad_cmd_reads_payload_without_waiting_for_stdin_eof(tmp_path: Path) -> None:
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "bad" + " cmd"},
        }
    )

    result = _run_hook_with_open_stdin(tmp_path, payload)

    assert result.returncode == 2
    assert "permissionDecision" in result.stdout
    assert "deny" in result.stdout
    assert "refusing to run" in result.stderr


def test_block_bad_cmd_allows_missing_payload_without_waiting_for_stdin_eof(
    tmp_path: Path,
) -> None:
    result = _run_hook_with_open_stdin(tmp_path, None)

    assert result.returncode == 0
    log_path = tmp_path / "home" / ".clud" / "tools" / "hooks" / "block-bad-cmd.log"
    log = log_path.read_text(encoding="utf-8")
    assert "stdin_read_incomplete" in log
    assert "raw_stdin_bytes=0" in log


def test_block_bad_cmd_allows_malformed_json(tmp_path: Path) -> None:
    result = _run_hook_with_open_stdin(tmp_path, "{not-json")

    assert result.returncode == 0
    assert "permissionDecision" not in result.stdout


def test_non_shell_patch_payload_skips_shell_identity_analysis(tmp_path: Path) -> None:
    """A patch body is data for the harness tool, never shell source."""
    payload = json.dumps(
        {
            "tool_name": "apply_patch",
            "tool_input": (
                "*** Begin Patch\n*** Update File: note.txt\n@@\n-old\n"
                "+rm -rf /\n*** End Patch"
            ),
            "cwd": str(tmp_path),
        }
    )

    result = _run_hook_with_open_stdin(tmp_path, payload)

    assert result.returncode == 0, result.stderr
    assert "permissionDecision" not in result.stdout


def test_deletion_environment_denial_has_a_categorized_audit_record(tmp_path: Path) -> None:
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "PATH=/usr/bin safe-rm x"},
            "cwd": str(tmp_path),
        }
    )

    result = _run_hook_with_open_stdin(tmp_path, payload, extra_env={"PATH": "/usr/bin"})

    assert result.returncode == 2
    log_path = tmp_path / "home" / ".clud" / "tools" / "hooks" / "block-bad-cmd.log"
    log = log_path.read_text(encoding="utf-8")
    assert 'RM-IDENTITY-BLOCKED tool_name="Bash"' in log


# --- #1064: unverifiable payloads fail closed for removals only -------------
#
# These feed the hook payloads it cannot parse. Nothing here ever executes a
# command: the hook reads stdin and prints a decision, so the `rm -rf` text
# below is inert data used to steer that decision.


def test_unparseable_payload_naming_a_removal_is_denied(tmp_path: Path) -> None:
    """A removal the hook could not inspect must not be allowed through.

    The payload is cut off mid-JSON and stdin is left open, which is the exact
    shape that used to reach an unconditional `return 0`.
    """
    truncated = '{"tool_name":"Bash","tool_input":{"command":"rm -rf \\"$SP\\"/'

    result = _run_hook_with_open_stdin(tmp_path, truncated)

    assert result.returncode == 2, result.stdout
    hook_output = json.loads(result.stdout)["hookSpecificOutput"]
    assert hook_output["permissionDecision"] == "deny"
    assert "removal" in hook_output["permissionDecisionReason"]


def test_unparseable_payload_naming_a_removal_after_a_newline_is_denied(
    tmp_path: Path,
) -> None:
    """A newline inside the command is `\\n` in the payload, not whitespace.

    A probe that demanded real whitespace before `rm` missed every removal
    that began a line, which is the ordinary shape of a multi-line command.
    """
    truncated = '{"tool_name":"Bash","tool_input":{"command":"cd /tmp\\nrm -rf $SP/'

    result = _run_hook_with_open_stdin(tmp_path, truncated)

    assert result.returncode == 2, result.stdout
    assert '"deny"' in result.stdout


def test_unparseable_payload_without_a_removal_is_still_allowed(
    tmp_path: Path,
) -> None:
    """The anti-wedge property: a broken payload is not a reason to block.

    A regression here is worse than the bug #1064 fixed, because it would wall
    off every tool call whenever the hook hiccups.
    """
    for truncated in (
        '{"tool_name":"Bash","tool_input":{"command":"cargo build --rel',
        '{"tool_name":"Bash","tool_input":{"command":"docker run --rm ubuntu',
        "{not-json but mentions armv7 and form/",
    ):
        result = _run_hook_with_open_stdin(tmp_path, truncated)

        assert result.returncode == 0, f"{truncated!r} -> {result.stdout!r}"
        assert "permissionDecision" not in result.stdout


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX pipe fixture")
def test_complete_safe_tool_payload_is_processed_with_held_open_stdin(tmp_path: Path) -> None:
    home = tmp_path / "home"
    payload = json.dumps(
        {"tool_name": "Bash", "tool_input": {"command": "safe-rm build"}, "cwd": str(tmp_path)}
    )
    script = (
        f"{{ printf %s {shlex.quote(payload)}; sleep 2; }} "
        f"| {shlex.quote(str(_block_bad_cmd_binary()))}"
    )
    result = process.run(
        ["bash", "-c", script],
        stdout=process.PIPE,
        stderr=process.PIPE,
        text=True,
        env=_hook_env(home),
        timeout=30,
    )
    assert result.returncode == 0, result
    output = json.loads(result.stdout)["hookSpecificOutput"]
    assert output["permissionDecision"] == "allow"
    log = (home / ".clud" / "tools" / "hooks" / "block-bad-cmd.log").read_text(encoding="utf-8")
    assert "stdin_read_incomplete" in log


@pytest.mark.parametrize(
    ("command", "expected"),
    [
        ("r" + "m -rf build", "safe-rm -rf build"),
        ("cd build && r" + "m -f cache", "cd build && safe-rm -f cache"),
        ("find build -name '*.tmp' -delete", None),
    ],
)
def test_deletion_hook_rewrites_or_refuses_with_fields_preserved(
    tmp_path: Path, command: str, expected: str | None
) -> None:
    tool_input = {"command": command, "timeout": 120_000, "future_field": {"preserve": True}}
    payload = json.dumps({"tool_name": "Bash", "tool_input": tool_input, "cwd": str(tmp_path)})
    result = _run_hook_with_open_stdin(tmp_path, payload)
    output = json.loads(result.stdout)["hookSpecificOutput"]
    if expected is None:
        assert result.returncode == 2, result
        assert output["permissionDecision"] == "deny"
        assert "safe-rm" in output["permissionDecisionReason"]
    else:
        assert result.returncode == 0, result
        assert output["permissionDecision"] == "allow"
        assert output["updatedInput"] == {**tool_input, "command": expected}


@pytest.mark.parametrize(
    "command",
    ["rm -rf build", "rmdir old", "unlink file", "find build -name '*.tmp' -delete"],
)
def test_unsafe_session_leaves_deletion_commands_unchanged(tmp_path: Path, command: str) -> None:
    payload = json.dumps(
        {"tool_name": "Bash", "tool_input": {"command": command}, "cwd": str(tmp_path)}
    )
    result = _run_hook_with_open_stdin(tmp_path, payload, extra_env={"CLUD_UNSAFE_MODE": "1"})
    assert result.returncode == 0, result
    assert result.stdout == ""


def test_deletion_hook_preserves_camel_case_codex_payload(tmp_path: Path) -> None:
    tool_input = {"command": "r" + "m -v 'my file'", "timeoutMs": 30_000}
    payload = json.dumps({"toolName": "Bash", "toolInput": tool_input, "cwdPath": str(tmp_path)})
    result = _run_hook_with_open_stdin(tmp_path, payload)
    assert result.returncode == 0, result
    output = json.loads(result.stdout)["hookSpecificOutput"]
    assert output["permissionDecision"] == "allow"
    assert output["updatedInput"] == {
        "command": "safe-rm -v 'my file'",
        "timeoutMs": 30_000,
    }


def test_normal_command_remains_silent_instead_of_emitting_bare_allow(
    tmp_path: Path,
) -> None:
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "echo ordinary"},
            "cwd": str(tmp_path),
        }
    )

    result = _run_hook_with_open_stdin(tmp_path, payload)

    assert result.returncode == 0
    assert result.stdout == ""


def test_telemetry_hook_reads_payload_without_waiting_for_stdin_eof(
    tmp_path: Path,
) -> None:
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
        }
    )

    result = _run_hook_with_open_stdin(
        tmp_path,
        payload,
        argv=[sys.executable, str(TELEMETRY)],
        extra_env={"CLUD_DAEMON_HTTP_SERVER": "not-a-valid-url"},
    )

    assert result.returncode == 0


def test_tracked_soldr_hooks_read_payload_without_waiting_for_stdin_eof(
    tmp_path: Path,
) -> None:
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
        }
    )

    scripts = [script for script in (CLAUDE_SOLDR_HOOK, CODEX_SOLDR_HOOK) if script.is_file()]
    assert scripts, "at least one tracked soldr hook must exist"
    for script in scripts:
        result = _run_hook_with_open_stdin(
            tmp_path,
            payload,
            argv=[sys.executable, str(script)],
        )
        assert result.returncode == 0, script


def test_bad_command_denial_includes_rule_provenance(tmp_path: Path) -> None:
    """#525: a config `bad_commands` denial cites the matched token, normalized
    program, rule id, and `<file>#/bad_commands/<index>` source in
    `permissionDecisionReason`, and writes a structured `bad_cmd_denied` event
    to the hook log."""
    repo = tmp_path / "repo"
    (repo / ".clud").mkdir(parents=True)
    (repo / ".git").mkdir()
    (repo / ".clud" / "settings.json").write_text(
        json.dumps(
            {
                "bad_commands": [
                    {
                        "id": "manual-check",
                        "match": "clud-manual-bad-command",
                        "replacement": "echo use-the-approved-command",
                        "reason": "manual verification rule triggered",
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "clud-manual-bad-command --example"},
            "cwd": str(repo),
        }
    )

    result = _run_hook_with_open_stdin(tmp_path, payload)
    assert result.returncode == 2, result.stderr
    out = result.stdout
    assert "deny" in out
    # Provenance appended to permissionDecisionReason.
    assert "Blocked" in out
    assert "clud-manual-bad-command" in out
    assert "normalized:" in out
    assert "by rule `manual-check`" in out
    assert "#/bad_commands/0`" in out

    # Structured forensic event in the hook log.
    log = tmp_path / "home" / ".clud" / "tools" / "hooks" / "block-bad-cmd.log"
    assert log.is_file(), "hook log should exist"
    log_text = log.read_text(encoding="utf-8")
    assert "bad_cmd_denied" in log_text
    assert "manual-check" in log_text


@pytest.mark.parametrize("denied", [False, True])
@pytest.mark.parametrize("quote", ["'", '"'])
def test_hook_log_redaction_keeps_the_hook_decision(
    tmp_path: Path, denied: bool, quote: str
) -> None:
    fake = "fake-hook-password"
    command = f"printf '%s' {quote}{fake}{quote} | gh secret set FAKE_TEST"
    if denied:
        command += "; echo bad" + " cmd"
    payload = json.dumps(
        {"tool_name": "Bash", "tool_input": {"command": command}, "cwd": str(tmp_path)}
    )
    result = _run_hook_with_open_stdin(tmp_path, payload)
    assert result.returncode == (2 if denied else 0)
    log_path = tmp_path / "home" / ".clud" / "tools" / "hooks" / "block-bad-cmd.log"
    log = log_path.read_text(encoding="utf-8")
    assert fake not in log
    assert "[REDACTED:gh-secret]" in log


def test_hook_log_redaction_preserves_ordinary_commands(tmp_path: Path) -> None:
    command = (
        "echo /tmp/ordinary-path 0123456789abcdef0123456789abcdef01234567 "
        "01234567-89ab-cdef-0123-456789abcdef abcd-efgh-ijkl-mnop; "
        "login --password-file /tmp/password-file"
    )
    payload = json.dumps(
        {"tool_name": "Bash", "tool_input": {"command": command}, "cwd": str(tmp_path)}
    )
    result = _run_hook_with_open_stdin(tmp_path, payload)
    assert result.returncode == 0
    log_path = tmp_path / "home" / ".clud" / "tools" / "hooks" / "block-bad-cmd.log"
    assert command in log_path.read_text(encoding="utf-8")
