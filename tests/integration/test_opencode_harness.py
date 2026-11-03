"""OpenCode launch policy through the real clud CLI and a recording harness."""

from __future__ import annotations

import json
import os
import platform
import sys
from pathlib import Path

import pytest

from tests import process
from tests.metered_consent import consent_for

pytestmark = [
    pytest.mark.integration,
    pytest.mark.skipif(
        platform.system() == "Windows", reason="recording harness uses a POSIX script"
    ),
]

FAKE = """#!/usr/bin/env python
import json, os, sys
if '--version' in sys.argv:
    print('1.19.5')
else:
    with open(os.environ['OPENCODE_RECORD'], 'w', encoding='utf-8') as handle:
        config = json.loads(os.environ['OPENCODE_CONFIG_CONTENT'])
        json.dump({'argv': sys.argv[1:], 'config': config}, handle)
    print('OPENCODE_LAUNCHED')
"""


@pytest.fixture
def opencode_env(tmp_path: Path):
    env = os.environ.copy()
    home = tmp_path / "home"
    home.mkdir()
    binary_dir = tmp_path / "bin"
    binary_dir.mkdir()
    executable = binary_dir / "opencode"
    executable.write_text(FAKE, encoding="utf-8")
    executable.chmod(0o755)
    vault = home / "vault"
    vault.mkdir()
    (vault / "clud_deepseek--api_key_v1.secret").write_text(
        "sk-deepseek-canary-7251", encoding="utf-8"
    )
    env.update(
        {
            "HOME": str(home),
            "USERPROFILE": str(home),
            "CLUD_NO_UNLOCK": "1",
            "CLUD_HOOK_HOME": str(home),
            "SOLDR_BROKER_AUTOSPAWN": "0",
            "XDG_CONFIG_HOME": str(home / "config"),
            "XDG_STATE_HOME": str(home / "state"),
            "XDG_CACHE_HOME": str(home / "cache"),
            "XDG_DATA_HOME": str(home / "data"),
            "PATH": str(binary_dir) + os.pathsep + env["PATH"],
            "OPENCODE_RECORD": str(tmp_path / "record.json"),
            "ANTHROPIC_API_KEY": "claude-canary-7251",
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:1234",
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_STORE_DIR": str(vault),
            "CLUD_TEST_CREDENTIAL_PROBE_URL": "http://127.0.0.1:9/v1/key",
            "OPENCODE_CONFIG_CONTENT": json.dumps({"permission": {"bash": "deny"}}),
        }
    )
    return env


def _run(clud: Path, env: dict[str, str], *args: str):
    return process.run(
        [str(clud), "--harness", "opencode", *args],
        env=consent_for(env, args),
        capture_output=True,
        text=True,
        timeout=60,
    )


def test_safe_resume_launch_preserves_user_permissions(clud_binary: Path, opencode_env) -> None:
    result = _run(
        clud_binary, opencode_env, "--claude", "--safe", "--resume=ses_7251", "-p", "hello"
    )
    assert result.returncode == 0, result.stdout + result.stderr
    record = json.loads(Path(opencode_env["OPENCODE_RECORD"]).read_text(encoding="utf-8"))
    assert record["argv"][:3] == ["run", "--format", "json"]
    assert "--auto" not in record["argv"]
    assert "-p" not in record["argv"]
    assert "--session" in record["argv"]
    assert "ses_7251" in record["argv"]
    assert record["config"]["permission"]["bash"] == "deny"
    options = record["config"]["provider"]["clud"]["options"]
    assert options["apiKey"] == "claude-canary-7251"
    assert options["baseURL"] == "http://127.0.0.1:1234/v1"
    assert options["apiKey"] not in result.stdout + result.stderr


def test_deepseek_model_and_vault_key_reach_only_child_config(
    clud_binary: Path, opencode_env
) -> None:
    opencode_env.pop("DEEPSEEK_API_KEY", None)
    result = _run(
        clud_binary, opencode_env, "--deepseek", "--model", "deepseek-chat", "-p", "hello"
    )
    assert result.returncode == 0, result.stdout + result.stderr
    record = json.loads(Path(opencode_env["OPENCODE_RECORD"]).read_text(encoding="utf-8"))
    assert "clud/deepseek-chat" in record["argv"]
    assert "--auto" in record["argv"]
    assert record["config"]["provider"]["clud"]["options"]["apiKey"] == "sk-deepseek-canary-7251"
    assert "sk-deepseek-canary-7251" not in result.stdout + result.stderr


@pytest.mark.parametrize(
    "options", [["--resume"], ["--effort", "high"], ["--codex"], ["--unified"]]
)
def test_unsupported_options_fail_before_spawn(clud_binary: Path, opencode_env, options) -> None:
    result = _run(clud_binary, opencode_env, *options)
    assert result.returncode != 0
    assert "OpenCode" in result.stderr or "Claude harness" in result.stderr
    assert not Path(opencode_env["OPENCODE_RECORD"]).exists()


def test_dry_run_contains_no_provider_credentials(clud_binary: Path, opencode_env) -> None:
    result = _run(clud_binary, opencode_env, "--claude", "--dry-run", "-p", "hello")
    assert result.returncode == 0, result.stderr
    plan = json.loads(result.stdout)
    assert plan["command"][1:4] == ["run", "--format", "json"]
    assert "claude-canary-7251" not in result.stdout + result.stderr
    assert "OPENCODE_CONFIG_CONTENT" not in result.stdout
    assert not Path(opencode_env["OPENCODE_RECORD"]).exists()


def test_missing_executable_reports_install_command(clud_binary: Path, opencode_env) -> None:
    directory = Path(opencode_env["OPENCODE_RECORD"]).parent / "bin"
    (directory / "opencode").rename(directory / "not-opencode")
    opencode_env["PATH"] = str(directory) + os.pathsep + str(Path(sys.executable).parent)
    result = _run(clud_binary, opencode_env, "--claude", "-p", "hello")
    assert result.returncode != 0
    assert "opencode" in result.stderr
    assert "npm install -g opencode-ai@1.19.5" in result.stderr
