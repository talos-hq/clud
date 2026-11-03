"""CLI regression for resumable watchdog exit status (#1431)."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from tests import process


@pytest.mark.skipif(sys.platform == "win32", reason="the uv fixture uses /bin/sh")
@pytest.mark.parametrize(
    ("command_ms", "progress_ms", "reason"),
    [(100, None, "command_timeout"), (3000, 100, "progress_timeout")],
)
def test_resumable_bundled_tool_watchdog_exits_124(
    tmp_path: Path, command_ms: int, progress_ms: int | None, reason: str
) -> None:
    clud = Path(os.environ["CLUD_TEST_BINARY"])
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    uv = fake_bin / "uv"
    uv.write_text("#!/bin/sh\nsleep 1\nexit 0\n", encoding="utf-8")
    uv.chmod(0o755)

    env = os.environ.copy()
    env["HOME"] = str(tmp_path)
    env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
    env["CLUD_TEST_TOOL_COMMAND_TIMEOUT_MS"] = str(command_ms)
    if progress_ms is not None:
        env["CLUD_TEST_TOOL_PROGRESS_TIMEOUT_MS"] = str(progress_ms)
    else:
        env.pop("CLUD_TEST_TOOL_PROGRESS_TIMEOUT_MS", None)

    result = process.run(
        [str(clud), "tool", "run", "github/pr_merge_watch.py", "1431", "--repo", "zackees/clud"],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        timeout=20,
    )
    # Match on the parsed record, not a key-order prefix: clud serializes
    # JSON in insertion order (serde_json preserve_order, #2225).
    terminals = [
        record
        for line in result.stderr.splitlines()
        if line.startswith("{")
        for record in [json.loads(line)]
        if "elapsed_ms" in record
    ]
    assert result.returncode == 124, result.stderr
    assert len(terminals) == 1, result.stderr
    assert terminals[0]["status"] == "in-progress"
    assert terminals[0]["reason"] == reason
    assert terminals[0]["resume_hint"]
