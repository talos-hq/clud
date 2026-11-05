"""Unit coverage for the bundled `wait/until.py` waiter (#2631).

The tool exists because a polling agent pays for the whole conversation on
every tick: one blocking call that prints only the delta replaces N of them.
These tests drive the real CLI against shell commands whose output changes on
a schedule, plus the pure helpers.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

TOOL = (
    Path(__file__).resolve().parents[1]
    / "crates"
    / "clud-bin"
    / "assets"
    / "tools"
    / "wait"
    / "until.py"
)


def run(args: list[str], timeout: float = 30.0) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(TOOL), *args],
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )


def counter_script(directory: Path) -> str:
    """A command whose output changes on the second run (and every run after)."""
    counter = directory / "count"
    return (
        f'n=$(cat "{counter}" 2>/dev/null || echo 0); n=$((n+1)); '
        f'printf "%s\\n" "$n" > "{counter}"; echo "value $n"'
    )


def steady_script() -> str:
    return "echo never-changes"


def test_output_is_bounded_with_an_explicit_marker() -> None:
    sys.path.insert(0, str(TOOL.parent))
    import until

    text = "x" * 500
    bounded = until.bounded(text, 100)
    assert bounded.startswith("x" * 100)
    assert "400 more characters omitted" in bounded
    assert until.bounded("short", 100) == "short"


def test_digest_changes_with_content() -> None:
    sys.path.insert(0, str(TOOL.parent))
    import until

    assert until.digest("a") != until.digest("b")
    assert until.digest("a") == until.digest("a")


def test_returns_as_soon_as_the_output_changes(tmp_path: Path) -> None:
    result = run(["--cmd", counter_script(tmp_path), "--interval", "0.1", "--timeout", "20"])
    assert result.returncode == 0, result.stderr
    assert "changed after" in result.stdout
    assert "value 2" in result.stdout
    assert "2 polls" in result.stdout


def test_nothing_is_printed_while_the_state_holds_still(tmp_path: Path) -> None:
    result = run(["--cmd", steady_script(), "--interval", "0.1", "--timeout", "1"])
    assert result.returncode == 124
    assert result.stdout.startswith("unchanged after")
    assert "never-changes" in result.stdout


def test_match_fires_on_the_first_run(tmp_path: Path) -> None:
    result = run(["--cmd", "echo state MERGED", "--interval", "0.1", "--match", "MERGED"])
    assert result.returncode == 0
    assert "matched on the first run" in result.stdout
    assert "MERGED" in result.stdout


def test_match_fires_when_a_later_poll_matches(tmp_path: Path) -> None:
    counter = tmp_path / "count"
    command = (
        f'n=$(cat "{counter}" 2>/dev/null || echo 0); n=$((n+1)); '
        f'printf "%s\\n" "$n" > "{counter}"; if [ "$n" -lt 2 ]; then echo waiting; '
        "else echo called MERGED; fi"
    )
    result = run(["--cmd", command, "--interval", "0.1", "--timeout", "20", "--match", "MERGED"])
    assert result.returncode == 0, result.stderr
    assert "matched after" in result.stdout


def test_long_output_is_capped_to_max_bytes(tmp_path: Path) -> None:
    counter = tmp_path / "c"
    script = (
        f'n=$(cat "{counter}" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "{counter}"; '
        'echo "run $n"; seq 1 400'
    )
    result = run(["--cmd", script, "--interval", "0.1", "--timeout", "20", "--max-bytes", "200"])
    assert result.returncode == 0, result.stderr
    assert "more characters omitted" in result.stdout
    assert len(result.stdout) < 600


def test_usage_errors_are_refused() -> None:
    assert run(["--interval", "0.1"]).returncode == 64
    assert run(["--cmd", "true", "--interval", "0"]).returncode == 64
    assert run(["--cmd", "true", "--match", "["]).returncode == 64


def test_the_tool_is_registered_as_resumable_and_quiet() -> None:
    source = (
        Path(__file__).resolve().parents[1] / "crates" / "clud-bin" / "src" / "tools.rs"
    ).read_text()
    assert 'rel_path: "wait/until.py"' in source
    assert 'include_str!("../assets/tools/wait/until.py")' in source
    entry = source.split('rel_path: "wait/until.py"', 1)[1].split("BundledTool {", 1)[0]
    assert "KillSemantics::Resumable" in entry
    assert "quiet_ok: true" in entry
