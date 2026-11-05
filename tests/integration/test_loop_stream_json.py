"""Integration test: `clud loop` renders claude's stream-json events as
human-readable progress lines instead of dumping raw JSON.

This test does NOT call real claude — it uses the mock-agent's
``--mock-stream-json <path>`` flag to emit a canned sequence of stream-json
events, exactly the way real claude would when invoked with
``--output-format stream-json --verbose``. The renderer under test then has
to turn those JSON lines into ``[claude] ...`` / ``[tool] ...`` progress
lines.

Why this matters: on Windows, ``clud loop`` runs claude in subprocess launch
mode (PTY mode hangs under loops, see #38). Without the renderer the user
sees ``[clud] iteration 1/50`` and then silence until the whole iteration
finishes, because ``claude -p`` buffers its final response. The renderer
restores the live-progress UX the old Python loop had.
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

import pytest

from tests import process

from ._daemon_helpers import copy_launcher

pytestmark = pytest.mark.integration

_TIMEOUT = 30


def _run(
    clud: Path,
    *args: str,
    env: dict[str, str],
    cwd: Path,
) -> process.CompletedProcess[str]:
    # Copy clud into the test's tmpdir so the Windows trampoline rename
    # dance never targets a file outside the temp scope. Matches the
    # pattern in test_mock_agents.py.
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as temp_dir:
        launch = Path(temp_dir) / clud.name
        copy_launcher(clud, launch)
        return process.run(
            [str(launch), *args],
            capture_output=True,
            text=True,
            timeout=_TIMEOUT,
            env=env,
            cwd=cwd,
        )


def _write_canned_events(path: Path) -> None:
    """Write a small but representative sequence of stream-json events."""
    events = [
        {
            "type": "system",
            "subtype": "init",
            "session_id": "abc123",
            "tools": ["Bash", "Read", "Edit"],
            "model": "claude-opus-4-7",
            "permissionMode": "default",
        },
        {
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "I'll start by reading the file."}]},
        },
        {
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "Bash",
                        "input": {"command": "cargo test --lib"},
                    }
                ]
            },
        },
        {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "duration_ms": 4321,
            "num_turns": 2,
            "total_cost_usd": 0.0456,
        },
    ]
    path.write_text("\n".join(json.dumps(e) for e in events), encoding="utf-8")


_WIN_ONLY_SKIP = (
    "POSIX clud loop runs in PTY mode where streaming already works; "
    "the subprocess-mode stream-json renderer is the Windows-only path."
)


@pytest.mark.skipif(sys.platform != "win32", reason=_WIN_ONLY_SKIP)
def test_loop_renders_stream_json_as_progress_lines(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """`clud loop` (subprocess mode) must render stream-json events as
    `[claude] ...` / `[tool] ...` lines instead of leaking raw JSON.
    """
    script = tmp_path / "events.jsonl"
    _write_canned_events(script)

    # `--no-done` so we don't wait for a DONE marker. `--loop-count 1` so the
    # loop terminates after the single canned run. We force `--subprocess`
    # so the test is platform-independent (the renderer is subprocess-only).
    result = _run(
        clud_binary,
        "--subprocess",
        "loop",
        "--no-done",
        "--loop-count",
        "1",
        "make progress",
        "--",
        "--mock-stream-json",
        str(script),
        env=mock_env,
        cwd=tmp_path,
    )

    assert result.returncode == 0, (
        f"clud exited {result.returncode}\nstderr:\n{result.stderr}\nstdout:\n{result.stdout}"
    )

    # The renderer emits one progress line per event. We collect output from
    # both streams because the runtime is free to print progress on stderr
    # (alongside the existing `[clud] iteration X/Y` line) or stdout.
    combined = result.stdout + "\n" + result.stderr

    # Assistant text event → `[claude] I'll start by reading the file.`
    assert "[claude] I'll start by reading the file." in combined, (
        f"missing assistant text render in:\n{combined}"
    )

    # Tool-use event for Bash → `[tool] Bash: cargo test --lib`
    assert "[tool] Bash: cargo test --lib" in combined, f"missing tool_use render in:\n{combined}"

    # Result event → `[claude] done · 4.3s · $0.05 · 2 turns`
    assert "[claude] done" in combined, f"missing result render in:\n{combined}"
    assert "4.3s" in combined, f"missing duration in result render:\n{combined}"

    # The raw JSON event must NOT have been passed through verbatim — that's
    # the whole point of the renderer. If we see the literal `"type":` keys
    # for the assistant event, the renderer didn't run.
    assert '"type":"assistant"' not in combined, (
        f"raw assistant JSON leaked into output:\n{combined}"
    )


@pytest.mark.skipif(sys.platform != "win32", reason=_WIN_ONLY_SKIP)
def test_loop_passes_through_non_json_stderr_lines(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """Lines that are not valid JSON (e.g. a stray panic line that claude or
    a wrapper printed to stderr) must NOT be silently dropped — the
    renderer passes them through verbatim so the operator still sees
    diagnostic messages.
    """
    script = tmp_path / "events.jsonl"
    script.write_text(
        # An assistant event, plus a non-JSON line, plus a result event.
        '{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}\n'
        "npm WARN deprecated foo@1.0\n"
        '{"type":"result","subtype":"success","is_error":false,"duration_ms":100,"num_turns":1}\n',
        encoding="utf-8",
    )

    result = _run(
        clud_binary,
        "--subprocess",
        "loop",
        "--no-done",
        "--loop-count",
        "1",
        "task",
        "--",
        "--mock-stream-json",
        str(script),
        env=mock_env,
        cwd=tmp_path,
    )
    assert result.returncode == 0, f"stderr: {result.stderr}"

    combined = result.stdout + "\n" + result.stderr
    assert "[claude] hello" in combined
    assert "npm WARN deprecated foo@1.0" in combined, (
        f"non-JSON stderr line was dropped:\n{combined}"
    )


@pytest.mark.parametrize("launch_mode", ["--subprocess", "--pty"])
def test_codex_loop_preserves_output_in_both_launch_modes(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, launch_mode: str
) -> None:
    """Captured runs retain the whole transcript without a dashboard taking over."""
    script = tmp_path / "history.txt"
    script.write_text("\n".join(f"history-{n:03}" for n in range(250)) + "\n", encoding="utf-8")
    result = _run(
        clud_binary,
        "--no-daemon",
        "--codex",
        launch_mode,
        "loop",
        "--no-done",
        "--loop-count",
        "1",
        "test scrolling output",
        "--",
        "--mock-ansi-script",
        str(script),
        env=mock_env,
        cwd=tmp_path,
    )
    combined = result.stdout + result.stderr
    assert result.returncode == 0, combined
    for n in range(250):
        assert f"history-{n:03}" in combined
    assert "\x1b[?1049h" not in combined
    assert "\x1b[2J" not in combined
