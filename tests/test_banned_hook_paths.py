"""Tests for the portable-hook-config lint (#1334)."""

import json

from ci.banned_hook_paths import is_hook_config, main, scan


def _config(command: str) -> str:
    handler = {"type": "command", "command": command}
    return json.dumps({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [handler]}]}})


def test_rejects_the_committed_dev_build_path_that_shipped() -> None:
    """The exact shape #1320 committed to main."""
    assert scan(_config("'/home/niteris/dev/clud2-do/target/debug/clud-cmd-scan'"))


def test_rejects_windows_pinned_helper() -> None:
    assert scan(_config("& 'C:\\Users\\me\\.local\\bin\\clud-cmd-scan.exe'; exit $LASTEXITCODE"))


def test_rejects_any_absolute_clud_binary() -> None:
    assert scan(_config("/usr/local/bin/clud-cmd-scan"))


def test_rejects_a_relative_target_dir() -> None:
    assert scan(_config("target/release/clud-cmd-scan"))


def test_accepts_portable_commands() -> None:
    assert not scan(_config("clud-cmd-scan"))
    assert not scan(_config("clud-cmd-scan; exit $LASTEXITCODE"))
    assert not scan(_config("python .codex/hooks/check-soldr.py"))
    assert not scan(_config('"$CLUD_EXE" tool run hooks/block-bad-cmd.py'))


def test_unparsable_config_is_scanned_as_text() -> None:
    assert scan('{"command": "/x/target/debug/clud-cmd-scan",')


def test_hook_config_selection() -> None:
    assert is_hook_config(".claude/settings.json")
    assert is_hook_config("sub/.codex/hooks.json")
    assert is_hook_config(".claude/settings.local.json")
    assert not is_hook_config("docs/settings.json")
    assert not is_hook_config(".claude/agents/grind-editor.md")


def test_this_repo_hook_configs_are_portable() -> None:
    assert main() == 0
