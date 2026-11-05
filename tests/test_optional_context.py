"""Optional Hermes context must remain a reference in each native harness prompt."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from tests import process


@pytest.fixture
def clud_binary() -> Path:
    configured = os.environ.get("CLUD_TEST_BINARY")
    suffix = ".exe" if sys.platform == "win32" else ""
    binary = (
        Path(configured)
        if configured
        else Path(__file__).resolve().parents[1] / "target" / "debug" / f"clud{suffix}"
    )
    if not binary.is_file():
        pytest.skip("clud binary unavailable; build it or set CLUD_TEST_BINARY")
    return binary.resolve()


@pytest.mark.parametrize("harness", ["--claude", "--codex"])
@pytest.mark.parametrize("enabled", [True, False])
def test_personality_reference_preserves_native_system_prompt(
    clud_binary: Path, tmp_path: Path, harness: str, enabled: bool
) -> None:
    home = tmp_path / "home"
    settings = home / ".clud" / "settings.json"
    settings.parent.mkdir(parents=True)
    settings.write_text(json.dumps({"context": {"hermes": {"enabled": enabled}}}))
    instance = tmp_path / "custom hermes instance"
    instance.mkdir()
    soul = instance / "SOUL.md"
    soul.write_text("PRIVATE_PERSONA_CONTENT_NOT_TO_BE_INJECTED", encoding="utf-8")
    env = {
        **os.environ,
        "HOME": str(home),
        "USERPROFILE": str(home),
        "HERMES_HOME": str(instance),
        "CODEX_HOME": str(home / ".codex"),
        "CLAUDE_CONFIG_DIR": str(home / ".claude"),
    }
    result = process.run(
        [str(clud_binary), "--no-daemon", "--dry-run", harness, "-p", "describe the project"],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    command = json.loads(result.stdout)["command"]
    assert "--system-prompt" not in command
    assert not any(value.startswith("model_instructions_file=") for value in command)
    if harness == "--claude":
        instructions = command[command.index("--append-system-prompt") + 1]
    else:
        instructions = next(
            value for value in command if value.startswith("developer_instructions=")
        )
    assert ("Optional personality context" in instructions) is enabled
    assert ("SOUL.md" in instructions) is enabled
    assert "PRIVATE_PERSONA_CONTENT_NOT_TO_BE_INJECTED" not in instructions
    assert "describe the project" in command


def test_context_config_set_validates_before_proposing_a_change(
    clud_binary: Path, tmp_path: Path
) -> None:
    env = {**os.environ, "HOME": str(tmp_path), "USERPROFILE": str(tmp_path)}
    for value, expected in [("true", 0), ("false", 0), ("yes", 1)]:
        result = process.run(
            [str(clud_binary), "--dry-run", "config", "set", f"context.hermes.enabled={value}"],
            cwd=tmp_path,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == expected, result.stderr
