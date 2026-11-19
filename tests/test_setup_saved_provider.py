"""After `clud setup` selects OpenRouter, a bare `clud` is `clud --openrouter` (#2850)."""

from __future__ import annotations

import json
import os
from pathlib import Path

from tests import process

# A well-formed, obviously fake key, assembled so scanners do not flag it.
FAKE_KEY = "sk-or-v1-" + "0123456789abcdef" * 4
# Where a plan names who chose the route; the route itself must match.
SOURCE_FIELDS = ("provider_source", "harness_source")


def _world(tmp_path: Path) -> tuple[str, dict[str, str]]:
    binary = os.environ.get("CLUD_TEST_BINARY")
    assert binary, "run with the built CI bundle"
    home = tmp_path / "home"
    bin_dir = home / ".local" / "bin"
    bin_dir.mkdir(parents=True)
    # Setup finds Claude Code at its native install location and installs nothing.
    for name in ("claude", "claude.exe"):
        (bin_dir / name).write_bytes(b"fake frontend")
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("CLUD_", "HERMES_")) and key != "OPENROUTER_API_KEY"
    }
    env.update(
        {
            "HOME": str(home),
            "USERPROFILE": str(home),
            "LOCALAPPDATA": str(home / "local-app-data"),
            "XDG_STATE_HOME": str(home / ".local" / "state"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "CLUD_HOOK_HOME": str(home),
            "CLUD_DAEMON_STATE_DIR": str(tmp_path / "state"),
            "CLUD_NO_UNLOCK": "1",
            "CLUD_SERVER_SETTINGS": "0",
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_STORE_DIR": str(tmp_path / "vault"),
        }
    )
    return binary, env


def _run(binary: str, args: list[str], env: dict[str, str], cwd: Path) -> str:
    result = process.run(
        [binary, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=60
    )
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout


def _route(stdout: str) -> dict[str, object]:
    plan = json.loads(stdout)
    for field in SOURCE_FIELDS:
        assert field in plan, sorted(plan)
        plan.pop(field)
    return plan


def test_bare_launch_after_openrouter_setup_matches_the_openrouter_flag(tmp_path: Path) -> None:
    binary, env = _world(tmp_path)
    repo = tmp_path / "repo"
    repo.mkdir()
    setup = _run(
        binary,
        ["setup", "--yes", "--provider", "openrouter"],
        {**env, "OPENROUTER_API_KEY": FAKE_KEY},
        repo,
    )
    assert "default set to openrouter" in setup, setup
    assert FAKE_KEY not in setup

    bare = _route(_run(binary, ["--dry-run", "-p", "hello"], env, repo))
    flagged = _route(_run(binary, ["--dry-run", "--openrouter", "-p", "hello"], env, repo))

    assert bare["model_provider"] == "openrouter", bare
    assert bare == flagged
