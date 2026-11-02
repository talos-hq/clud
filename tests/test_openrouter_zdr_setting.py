"""Persistent OpenRouter privacy across independent launcher processes (#1998)."""

from __future__ import annotations

import json
import os
from pathlib import Path

from tests import process
from tests.metered_consent import consent_for


def test_saved_openrouter_zdr_survives_fresh_processes_and_directories(tmp_path: Path) -> None:
    binary = os.environ.get("CLUD_TEST_BINARY")
    assert binary, "run with the built CI bundle"
    home = tmp_path / "home"
    settings = home / ".clud" / "settings.json"
    settings.parent.mkdir(parents=True)
    settings.write_text(
        json.dumps({"providers": {"openrouter": {"zdr": True}}, "unrelated": "preserved"}),
        encoding="utf-8",
    )
    original = settings.read_bytes()
    env = {key: value for key, value in os.environ.items() if not key.startswith("CLUD_")}
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
        }
    )
    routes = [
        ["--openrouter", "--model", "~anthropic/claude-sonnet-latest"],
        ["--unified", "--model", "openrouter-claude-sonnet"],
    ]
    for index, route in enumerate(routes):
        cwd = tmp_path / f"repo-{index}"
        cwd.mkdir()
        result = process.run(
            [binary, "--dry-run", *route, "-p", "hello"],
            cwd=cwd,
            env=consent_for(env, route),
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        plan = json.loads(result.stdout)
        routing = plan["route"]["upstream_routing"] or {}
        assert routing.get("zdr") is True, plan["route"]
        assert settings.read_bytes() == original, "dry-run must not change saved settings"


def _setting_world(tmp_path: Path) -> tuple[str, dict[str, str], Path]:
    binary = os.environ.get("CLUD_TEST_BINARY")
    assert binary, "run with the built CI bundle"
    home = tmp_path / "home"
    settings = home / ".clud" / "settings.json"
    settings.parent.mkdir(parents=True)
    settings.write_text('{"unrelated": "preserved"}', encoding="utf-8")
    env = {key: value for key, value in os.environ.items() if not key.startswith("CLUD_")}
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
        }
    )
    return binary, env, settings


def _invoke(
    world: tuple[str, dict[str, str], Path], *args: str
) -> process.CompletedProcess[str]:
    binary, env, settings = world
    return process.run(
        [binary, *args],
        env=consent_for(env, args),
        cwd=settings.parent.parent.parent,
        capture_output=True,
        text=True,
        timeout=30,
    )


def _plan(world: tuple[str, dict[str, str], Path], *args: str) -> dict:
    result = _invoke(world, "--dry-run", *args, "-p", "hello")
    assert result.returncode == 0, result.stdout + result.stderr
    return json.loads(result.stdout)


def test_setter_persists_and_session_override_does_not_save(tmp_path: Path) -> None:
    world = _setting_world(tmp_path)
    settings = world[2]
    for value in [(), ("on",)]:
        result = _invoke(world, "--set-openrouter-zdr", *value)
        assert result.returncode == 0, result.stdout + result.stderr
        assert str(settings) in result.stdout
        saved = json.loads(settings.read_text())
        assert saved["providers"]["openrouter"]["zdr"] is True
        assert saved["unrelated"] == "preserved"
        routes = [("--openrouter",), ("--unified", "--model", "openrouter-claude-sonnet")]
        for route in routes:
            plan = _plan(world, *route)
            assert plan["route"]["upstream_routing"]["zdr"] is True
            assert plan["zdr"] is True
            assert plan["zdr_source"] == "provider_setting"
    result = _invoke(world, "--set-openrouter-zdr", "off")
    assert result.returncode == 0, result.stdout + result.stderr
    original = settings.read_bytes()
    assert json.loads(original)["providers"]["openrouter"]["zdr"] is False
    disabled = _plan(world, "--openrouter")
    assert disabled["zdr"] is False
    assert disabled["zdr_source"] == "provider_setting"
    assert disabled["route"]["upstream_routing"] is None
    override = _plan(world, "--openrouter", "--zdr")
    assert override["route"]["upstream_routing"]["zdr"] is True
    assert override["zdr_source"] == "cli"
    assert settings.read_bytes() == original


def test_saved_zdr_is_provider_scoped_and_composes_with_pins(tmp_path: Path) -> None:
    world = _setting_world(tmp_path)
    assert _invoke(world, "--set-openrouter-zdr").returncode == 0
    original = world[2].read_bytes()
    for provider in ["--claude", "--codex", "--deepseek"]:
        plan = _plan(world, provider)
        assert plan["zdr"] is False
        assert plan["zdr_source"] is None
        assert plan["route"]["upstream_routing"] is None
    pinned = _plan(world, "--openrouter", "--provider-only", "parasail/fp8")
    assert pinned["route"]["upstream_routing"] == {
        "only": ["parasail/fp8"],
        "allow_fallbacks": False,
        "zdr": True,
    }
    assert world[2].read_bytes() == original
    unsupported = _invoke(
        world,
        "--dry-run",
        "--openrouter",
        "--harness",
        "deepseek",
        "-p",
        "hello",
    )
    assert unsupported.returncode == 2, unsupported.stdout + unsupported.stderr
    assert "OpenRouter" in unsupported.stderr


def test_setter_exits_before_saved_terminal_and_rejects_invalid_inputs(tmp_path: Path) -> None:
    world = _setting_world(tmp_path)
    settings = world[2]
    settings.write_text('{"web_term": {"enabled": true}, "unrelated": "preserved"}')
    world[1]["CLUD_WEBTERM_BINARY"] = str(tmp_path / "must-not-launch")
    result = _invoke(world, "--set-openrouter-zdr")
    assert result.returncode == 0, result.stdout + result.stderr
    assert json.loads(settings.read_text())["web_term"]["enabled"] is True
    assert not (tmp_path / "state").exists(), "setter must not start a daemon"
    original = settings.read_bytes()
    for args in [
        ("--set-openrouter-zdr", "invalid"),
        ("--set-openrouter-zdr", "--dry-run"),
        ("--set-openrouter-zdr=on", "auth", "status"),
    ]:
        result = _invoke(world, *args)
        assert result.returncode == 2, result.stdout + result.stderr
        assert settings.read_bytes() == original


def test_missing_and_malformed_saved_policy_are_auditable(tmp_path: Path) -> None:
    world = _setting_world(tmp_path)
    original = world[2].read_bytes()
    plan = _plan(world, "--openrouter")
    assert plan["zdr"] is False
    assert plan["zdr_source"] == "built_in_default"
    assert world[2].read_bytes() == original
    world[2].write_text('{"providers": {"openrouter": {"zdr": "invalid"}}}')
    refused = _invoke(world, "--dry-run", "--openrouter", "-p", "hello")
    assert refused.returncode == 2, refused.stdout + refused.stderr
    assert "privacy policy" in refused.stderr
