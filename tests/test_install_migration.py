"""Credential migration selects providers before copying secrets into Hermes."""

from __future__ import annotations

import os
from pathlib import Path

from tests import process


FAKE_KEY = "sk-or-v1-" + "0123456789abcdef" * 4


def _world(tmp_path: Path) -> tuple[str, dict[str, str]]:
    binary = os.environ.get("CLUD_TEST_BINARY")
    assert binary, "run with the built CI bundle"
    home = tmp_path / "home"
    home.mkdir()
    excluded = {
        "OPENROUTER_API_KEY",
        "OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
        "KIMI_API_KEY",
        "CODEX_HOME",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    }
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("CLUD_", "HERMES_")) and key not in excluded
    }
    env.update(
        {
            "HOME": str(home),
            "USERPROFILE": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/clud-test-no-user-manager",
            "CLUD_SERVER_SETTINGS": "0",
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_STORE_DIR": str(tmp_path / "vault"),
            "OPENROUTER_API_KEY": FAKE_KEY,
        }
    )
    return binary, env


def test_explicit_provider_subset_keeps_codex_and_its_service_unconfigured(tmp_path: Path) -> None:
    binary, env = _world(tmp_path)
    env.update(
        {
            "OPENAI_API_KEY": "fake-openai-key",
            "CLUD_ALLOW_METERED": "1",
            "CLUD_METERED_PROVIDER": "codex",
        }
    )
    result = process.run(
        [
            binary,
            "install",
            "hermes",
            "--configure-only",
            "--yes",
            "--migrate-credentials",
            "--migrate-provider",
            "openrouter",
        ],
        env=env,
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "clud-codex" in result.stdout
    text = (Path(env["HOME"]) / ".hermes" / "config.yaml").read_text()
    assert "openrouter:" in text and FAKE_KEY in text
    assert "clud-codex:" not in text
    assert FAKE_KEY not in result.stdout + result.stderr
    assert not (Path(env["XDG_CONFIG_HOME"]) / "systemd/user/clud-serve-codex.service").exists()


def test_requested_unavailable_provider_copies_nothing(tmp_path: Path) -> None:
    binary, env = _world(tmp_path)
    result = process.run(
        [
            binary,
            "install",
            "hermes",
            "--configure-only",
            "--yes",
            "--migrate-provider",
            "clud-codex",
        ],
        env=env,
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 1, result.stdout + result.stderr
    assert "no available credential" in result.stderr
    assert not (Path(env["HOME"]) / ".hermes" / "config.yaml").exists()
