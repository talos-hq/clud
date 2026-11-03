"""Verify core clud CLI behavior: --help, --version, --dry-run, pipe mode."""

from __future__ import annotations

import json
import os
import re
import shutil
import socket
import sys
import tempfile
import time
from pathlib import Path

import pytest

from tests import process
from tests.metered_consent import consent_for

ROOT = Path(__file__).resolve().parent.parent

# Don't use tomllib: it's py311-only and `requires-python = ">=3.10"` promises
# py310 works. A regex is sufficient for the trivial `version = "x.y.z"` line
# in [project] and avoids the tomli backport dep.
_VERSION_RE = re.compile(r'^version\s*=\s*"([^"]+)"', re.MULTILINE)


def _project_version() -> str:
    text = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
    match = _VERSION_RE.search(text)
    if match is None:
        raise RuntimeError("project.version missing from pyproject.toml")
    return match.group(1)


def _cargo_argv(subcommand: list[str]) -> list[str]:
    """Return the cargo argv, pinning the MSVC toolchain on Windows.

    Windows rustc installations from chocolatey ship a GNU-host rustc which
    links C/C++ deps against MinGW runtime DLLs (libstdc++-6.dll,
    libgcc_s_seh-1.dll, libwinpthread-1.dll). Those DLLs aren't on stock
    Windows, so a test that launches the resulting binary fails with
    STATUS_ENTRYPOINT_NOT_FOUND (0xC0000139). `ci.env.build_env()` forces the
    MSVC target via the rustup-managed toolchain. Mirrors
    `tests/integration/conftest.py::_cargo_argv`.
    """
    if sys.platform == "win32":
        try:
            from ci.env import build_env, cargo_argv

            return cargo_argv(subcommand, env=build_env())
        except Exception:
            pass
        soldr = shutil.which("soldr")
        if soldr:
            return [soldr, "cargo", *subcommand]
    return ["cargo", *subcommand]


def _clud_binary() -> str:
    """Build the current repo's clud binary and return its path."""
    env_binary = os.environ.get("CLUD_TEST_BINARY")
    if env_binary and Path(env_binary).is_file():
        return env_binary

    result = process.run(
        _cargo_argv(["build", "-p", "clud", "--message-format=json"]),
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if result.returncode != 0:
        raise RuntimeError(f"Failed to build clud:\n{result.stderr}")

    for line in result.stdout.splitlines():
        msg = json.loads(line)
        if (
            msg.get("reason") == "compiler-artifact"
            and msg.get("target", {}).get("name") == "clud"
            and msg.get("executable")
        ):
            return msg["executable"]

    ext = ".exe" if sys.platform == "win32" else ""
    # The MSVC-pinned env lands artifacts in a triple-qualified subdir; bare
    # cargo lands in target/debug. Check both.
    for fallback in (
        ROOT / "target" / "x86_64-pc-windows-msvc" / "debug" / f"clud{ext}",
        ROOT / "target" / "aarch64-pc-windows-msvc" / "debug" / f"clud{ext}",
        ROOT / "target" / "debug" / f"clud{ext}",
    ):
        if fallback.is_file():
            return str(fallback)
    raise RuntimeError("clud binary not found after build")


CLUD = _clud_binary()
PROJECT_VERSION = _project_version()


def copied_clud_env(_source: Path) -> dict[str, str]:
    """Return an environment that can launch a copied clud binary."""
    env = os.environ.copy()
    # Suppress unlock_exe()'s in-place rename trampoline when the
    # launched binary lives in a tmpdir (every _run() invocation).
    # Without this, the trampoline renames `<tmpdir>/clud.exe` to
    # `<tmpdir>/clud.exe.old.<rand>` and races against
    # `TemporaryDirectory.__exit__`'s shutil.rmtree on Windows —
    # SECTION release on process exit is async, so the tmpdir cleanup
    # gets WinError 32 on the still-locked `.old.<rand>` file. The
    # trampoline brings zero benefit here (the tmpdir binary is
    # never `pip install`'d at this path). See issues #331, #333.
    env["CLUD_NO_UNLOCK"] = "1"
    # Pin the built-in server settings so a launch never
    # fetches the served copy from GitHub during a test (#1192).
    env["CLUD_SERVER_SETTINGS"] = "0"
    return env


def _copied_clud_tempdir() -> tempfile.TemporaryDirectory:
    """Return a tempdir safe for copied Windows executable launches."""
    return tempfile.TemporaryDirectory(ignore_cleanup_errors=sys.platform == "win32")


def _copy_clud_for_test(temp_dir: str) -> Path:
    source = Path(CLUD)
    launch = Path(temp_dir) / source.name
    shutil.copy2(source, launch)
    return launch


#: Must stay >= MIN_UNIFIED_CLAUDE_CODE_VERSION in
#: crates/clud-bin/src/backend_bootstrap.rs. A fake `claude` that answers
#: nothing to `--version` is rejected by the discovery version gate, and clud
#: exits 2 before announcing anything -- which is not what these tests are about.
_FAKE_CLAUDE_VERSION = "2.1.223 (Claude Code)"


def _fake_claude_on_path(bin_dir: Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    if sys.platform == "win32":
        fake = bin_dir / "claude.cmd"
        fake.write_text(
            f'@echo off\r\nif "%1"=="--version" echo {_FAKE_CLAUDE_VERSION}\r\nexit /b 0\r\n',
            encoding="utf-8",
        )
    else:
        fake = bin_dir / "claude"
        fake.write_text(
            "#!/usr/bin/env sh\n"
            f'if [ "$1" = "--version" ]; then echo "{_FAKE_CLAUDE_VERSION}"; fi\n'
            "exit 0\n",
            encoding="utf-8",
        )
        fake.chmod(0o755)


def _fake_harnesses_on_path(bin_dir: Path) -> None:
    """Install logging Claude/Codex/DeepSeek shims for POSIX picker tests."""
    bin_dir.mkdir(parents=True, exist_ok=True)
    for executable in ("claude", "codex", "dsh"):
        fake = bin_dir / executable
        fake.write_text(
            f'#!/bin/sh\nprintf "%s\\n" "{executable}" >> "$HARNESS_LOG"\n',
            encoding="utf-8",
        )
        fake.chmod(0o755)


def _run(
    *args: str,
    input_data: str | None = None,
    env_overrides: dict[str, str] | None = None,
) -> process.CompletedProcess[str]:
    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        home = Path(temp_dir) / "home"
        state_dir = Path(temp_dir) / "state"
        home.mkdir()
        env = _isolated_clud_env(source, home, state_dir)
        if env_overrides:
            env.update(env_overrides)
        # Existing launch tests opt into metered routes explicitly (#2529).
        env = consent_for(env, args)
        return process.run(
            [str(launch), *args],
            capture_output=True,
            text=True,
            timeout=10,
            input=input_data,
            env=env,
        )


def _isolated_clud_env(source: Path, home: Path, state_dir: Path) -> dict[str, str]:
    env = copied_clud_env(source)
    env["HOME"] = str(home)
    env["USERPROFILE"] = str(home)
    env["LOCALAPPDATA"] = str(home / "local-app-data")
    env["XDG_STATE_HOME"] = str(home / ".local" / "state")
    env["XDG_CACHE_HOME"] = str(home / ".cache")
    env["CLUD_HOOK_HOME"] = str(home)
    env["CLUD_DAEMON_STATE_DIR"] = str(state_dir)
    env["CLUD_DATA_DB"] = str(state_dir / "data.redb")
    # `clud do` asks GitHub whether an issue is a meta issue. Tests stay
    # offline: the override answers instead; the refusal test removes it.
    env["CLUD_DO_KIND"] = "single"
    return env


def _shutdown_daemon(state_dir: Path) -> None:
    info_path = state_dir / "daemon.json"
    if not info_path.exists():
        return
    info = json.loads(info_path.read_text(encoding="utf-8"))
    pid = int(info.get("pid") or 0)
    try:
        with socket.create_connection(("127.0.0.1", int(info["port"])), timeout=2) as sock:
            sock.sendall(b'{"op":"shutdown"}\n')
            sock.recv(4096)
    except OSError:
        return
    deadline = time.monotonic() + 10
    while (info_path.exists() or _pid_is_alive(pid)) and time.monotonic() < deadline:
        time.sleep(0.05)


def _pid_is_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    if sys.platform == "win32":
        try:
            result = process.run(
                ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
                capture_output=True,
                text=True,
                timeout=5,
            )
        except process.TimeoutExpired:
            return True
        return result.returncode == 0 and f'"{pid}"' in result.stdout
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _snapshot_tree(root: Path) -> dict[str, tuple[bool, int, int | None, bytes | None]]:
    snapshot: dict[str, tuple[bool, int, int | None, bytes | None]] = {}
    for path in sorted(root.rglob("*")):
        stat = path.stat()
        rel = path.relative_to(root).as_posix()
        if path.is_file():
            snapshot[rel] = (False, stat.st_mtime_ns, stat.st_size, path.read_bytes())
        else:
            snapshot[rel] = (True, stat.st_mtime_ns, None, None)
    return snapshot


def test_help() -> None:
    result = _run("--help")
    assert result.returncode == 0
    assert "YOLO" in result.stdout
    assert "--prompt" in result.stdout
    assert "--safe" in result.stdout
    assert "loop" in result.stdout
    assert "--kitty-term" in result.stdout


def test_kitty_term_rejects_clud_subcommands() -> None:
    result = _run("--kitty-term", "auth", "status")
    assert result.returncode == 2
    assert "--kitty-term only applies to a backend launch" in result.stderr


def test_kitty_term_reports_unavailable_host_or_companion(tmp_path: Path) -> None:
    missing_binary = tmp_path / "missing-wezterm-gui.exe"
    assert not missing_binary.exists()
    result = _run(
        "--kitty-term",
        "--codex",
        "-p",
        "hello",
        env_overrides={"CLUD_KITTY_TERM_BINARY": str(missing_binary)},
    )
    assert result.returncode == 1
    if sys.platform != "win32":
        assert "supported on Windows only" in result.stderr
    else:
        assert (
            "does not support Windows ARM64" in result.stderr
            or "currently requires x86_64 Windows" in result.stderr
            or f"WezTerm GUI is missing at {missing_binary}" in result.stderr
        )


def test_version() -> None:
    result = _run("--version")
    assert result.returncode == 0
    assert result.stdout.strip() == f"clud {PROJECT_VERSION}"


def test_gc_bare_prints_help_without_touching_clud_dir() -> None:
    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        home = Path(temp_dir) / "home"
        state_dir = Path(temp_dir) / "daemon-state"
        clud_dir = home / ".clud"
        marker = clud_dir / "sentinel.txt"
        marker.parent.mkdir(parents=True)
        marker.write_text("do not touch\n", encoding="utf-8")
        before = _snapshot_tree(clud_dir)

        result = process.run(
            [str(launch), "gc"],
            capture_output=True,
            text=True,
            timeout=10,
            env=_isolated_clud_env(source, home, state_dir),
        )

        assert result.returncode == 0, result.stderr
        assert "Commands:" in result.stdout or "SUBCOMMANDS:" in result.stdout
        assert "KINDS:" in result.stdout
        assert "uv-cache" in result.stdout
        assert "all" in result.stdout
        assert _snapshot_tree(clud_dir) == before


def test_gc_all_prunes_uv_cache_and_registered_trash() -> None:
    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        home = Path(temp_dir) / "home"
        state_dir = Path(temp_dir) / "daemon-state"
        env = _isolated_clud_env(source, home, state_dir)

        uv_env = home / ".clud" / "cache" / "uv" / "environments-v2" / "stale-env"
        uv_env.mkdir(parents=True)
        (uv_env / "pyvenv.cfg").write_text("stale\n", encoding="utf-8")
        old = time.time() - 8 * 24 * 60 * 60
        os.utime(uv_env, (old, old))

        victim = home / "victim.txt"
        victim.parent.mkdir(parents=True, exist_ok=True)
        victim.write_text("trash me\n", encoding="utf-8")
        # This test is about pruning *registered* trash, so a daemon has to be
        # up before the trash happens: `clud trash` is a utility mode and never
        # starts one, so with no daemon the row is skipped and there is nothing
        # to prune later. `gc list` is the non-destructive way to bring one up.
        warmup = process.run(
            [str(launch), "gc", "list"],
            capture_output=True,
            text=True,
            timeout=30,
            env=env,
        )
        assert warmup.returncode == 0, warmup.stderr
        trash = process.run(
            [str(launch), "trash", "--cross-volume", str(victim)],
            capture_output=True,
            text=True,
            timeout=20,
            env=env,
        )
        try:
            assert trash.returncode == 0, trash.stderr
            assert not victim.exists()
            trash_root = home / ".clud" / "trash"
            assert any(trash_root.iterdir())

            result = process.run(
                [str(launch), "gc", "all"],
                capture_output=True,
                text=True,
                timeout=30,
                env=env,
            )

            assert result.returncode == 0, result.stderr
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                trash_empty = not trash_root.exists() or not any(trash_root.iterdir())
                if not uv_env.exists() and trash_empty:
                    break
                time.sleep(0.1)
            assert not uv_env.exists(), result.stdout
            assert not trash_root.exists() or not any(trash_root.iterdir()), result.stdout
        finally:
            _shutdown_daemon(state_dir)


def test_top_once_json_arg_surface() -> None:
    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        home = Path(temp_dir) / "home"
        state_dir = Path(temp_dir) / "daemon-state"
        env = _isolated_clud_env(source, home, state_dir)
        try:
            result = process.run(
                [
                    str(launch),
                    "top",
                    "--once",
                    "--json",
                    "--flat",
                    "--sort",
                    "rss",
                    "--limit",
                    "5",
                    "--since",
                    "5s",
                    "--originator",
                    "CLUD:0",
                ],
                capture_output=True,
                text=True,
                timeout=20,
                env=env,
            )

            assert result.returncode == 0, result.stderr
            data = json.loads(result.stdout)
            assert data["schema_version"] == 1
            assert data["interval_ms"] >= 250
            assert isinstance(data["rows"], list)
            assert "summary" in data
        finally:
            _shutdown_daemon(state_dir)


def test_linux_binary_does_not_require_libasound_at_startup() -> None:
    if not sys.platform.startswith("linux"):
        return
    ldd = shutil.which("ldd")
    if ldd is None:
        return

    result = process.run(
        [ldd, CLUD],
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert result.returncode == 0, result.stderr
    assert "libasound" not in (result.stdout + result.stderr).lower()


def test_dry_run_prompt() -> None:
    result = _run("--dry-run", "-p", "hello")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert data["backend"] == "claude"
    assert data["launch_mode"] == "subprocess"
    assert "--dangerously-skip-permissions" in data["command"]
    assert "-p" in data["command"]
    assert "hello" in data["command"]
    assert data["iterations"] == 1
    assert data["model_provider"] == "claude"
    assert data["requested_harness"] == "default"
    assert data["effective_harness"] == "claude"
    assert data["provider_source"] == "built_in_default"
    assert data["harness_source"] == "built_in_default"


def test_unsafe_dry_run_is_explicit_and_does_not_inherit_a_marker() -> None:
    inherited = _run("--dry-run", "-p", "hello", env_overrides={"CLUD_UNSAFE_MODE": "1"})
    assert inherited.returncode == 0, inherited
    assert json.loads(inherited.stdout)["unsafe_mode"] is False

    for provider in ("--claude", "--codex"):
        result = _run("--dry-run", provider, "--unsafe", "-p", "hello")
        assert result.returncode == 0, result
        data = json.loads(result.stdout)
        assert data["unsafe_mode"] is True
        assert "safe-rm" not in json.dumps(data["command"])

    conflicting = _run("--dry-run", "--safe", "--unsafe", "-p", "hello")
    assert conflicting.returncode != 0


def _dry_run_coauthor(*args: str, env_value: str | None = None) -> dict:
    with _copied_clud_tempdir() as temp_dir:
        launch = _copy_clud_for_test(temp_dir)
        home = Path(temp_dir) / "home"
        home.mkdir()
        env = _isolated_clud_env(Path(CLUD), home, Path(temp_dir) / "state")
        env.pop("CLUD_COAUTHOR", None)
        if env_value is not None:
            env["CLUD_COAUTHOR"] = env_value
        result = process.run(
            [str(launch), "--dry-run", *args, "-p", "hello"],
            capture_output=True,
            text=True,
            timeout=10,
            env=env,
        )
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert not any(arg.startswith("--coauthor") for arg in data["command"])
    assert data["command"][-1] == "hello"
    return data["coauthor"]


def test_dry_run_coauthor_is_hidden_by_default() -> None:
    """#1317: Claude attribution is off unless the launch opts in."""
    assert _dry_run_coauthor() == {"mode": "hidden"}


def test_dry_run_coauthor_flag_opts_in() -> None:
    assert _dry_run_coauthor("--coauthor") == {"mode": "harness"}
    assert _dry_run_coauthor("--coauthor=Made with clud") == {
        "mode": "tag",
        "tag": "Made with clud",
    }


def test_dry_run_coauthor_env_var() -> None:
    assert _dry_run_coauthor(env_value="1") == {"mode": "harness"}
    assert _dry_run_coauthor(env_value="0") == {"mode": "hidden"}
    assert _dry_run_coauthor(env_value="Bot <b@x>") == {"mode": "tag", "tag": "Bot <b@x>"}


def test_dry_run_continue_keeps_native_continue_for_scripts() -> None:
    """#922: `-c` without a terminal (scripts, --dry-run) still forwards
    Claude's own `--continue`; only an interactive launch opens the picker."""
    result = _run("--dry-run", "-c")
    assert result.returncode == 0, result.stderr
    command = json.loads(result.stdout)["command"]
    assert "--continue" in command


def test_session_picker_flags_are_clud_options_not_passthrough() -> None:
    for argv in (["--last"], ["-c", "--resume-mode", "portable"], ["-c", "--resume-mode=native"]):
        result = _run("--dry-run", *argv)
        assert result.returncode == 0, (argv, result.stderr)
        command = json.loads(result.stdout)["command"]
        assert not any(arg.startswith(("--last", "--resume-mode")) for arg in command), (
            argv,
            command,
        )


def test_resume_mode_rejects_unknown_values() -> None:
    result = _run("--dry-run", "-c", "--resume-mode", "sideways")
    assert result.returncode != 0
    assert "sideways" in result.stderr


def test_cliproxyapi_paren_thinking_suffix_is_accepted_on_the_model_id() -> None:
    """CLIProxyAPI spells a thinking selection ``<model>(<value>)``
    (internal/thinking/suffix.go). clud accepts it beside its own
    ``<model>@<effort>``: a numeric budget resolves onto the same effort ladder
    as a stated ``thinking.budget_tokens`` (16384 -> high)."""
    result = _run("--dry-run", "--codex", "--model", "terra(16384)", "-p", "hello")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    selection = data["model_selection"]
    assert selection["model"] == "codex-terra"
    assert selection["wire_model"] == "gpt-5.6-terra"
    assert selection["effort"] == "high"
    assert selection["effort_source"] == "legacy_model_suffix"
    assert 'model_reasoning_effort="high"' in data["command"]


def test_dry_run_codex() -> None:
    result = _run("--dry-run", "--codex", "-p", "hello")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert data["backend"] == "codex"
    assert data["launch_mode"] == "subprocess"
    assert data["model_provider"] == "codex"
    assert data["requested_harness"] == "default"
    assert data["effective_harness"] == "codex"
    assert data["provider_source"] == "cli"
    assert data["harness_source"] == "built_in_default"
    assert data["model_selection"]["model"] == "codex-sol"
    assert data["model_selection"]["wire_model"] == "gpt-6-sol"
    assert data["codex_model_source"] in {"published", "last_good", "built_in"}
    assert data["model_selection"]["effort"] == "low"
    assert data["model_selection"]["model_source"] == "catalog_default"
    assert data["model_selection"]["effort_source"] == "catalog_default"
    assert data["command"].count("gpt-6-sol") == 1
    assert 'model_reasoning_effort="low"' in data["command"]
    assert not any("terra" in arg or arg == "medium" for arg in data["command"])


@pytest.mark.parametrize(
    ("verb", "target", "prompt_fragment"),
    [
        ("do", "https://github.com/zackees/clud/issues/1036", "Read the issue and implement it"),
        ("up", None, "codeup"),
        ("rebase", None, "git fetch"),
        ("fix", None, "linting"),
    ],
)
def test_codex_builtins_seed_interactive_dry_run(
    verb: str,
    target: str | None,
    prompt_fragment: str,
) -> None:
    argv = ["--dry-run", "--codex", verb]
    if target is not None:
        argv.append(target)
    result = _run(*argv)
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["backend"] == "codex"
    assert data["launch_mode"] == "subprocess"  # no terminal under pytest (DD-086)
    assert "exec" not in data["command"]
    assert prompt_fragment in data["command"][-1]


def test_grind_seeds_the_native_loop_on_the_claude_harness() -> None:
    """#1173: `grind` is one interactive Claude-harness PTY session whose
    prompt routes to the `/grind` skill; clud adds no loop machinery."""
    result = _run("--dry-run", "grind", "https://github.com/zackees/clud/issues")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["effective_harness"] == "claude"
    assert data["launch_mode"] == "subprocess"  # no terminal under pytest (DD-086)
    assert data["command"][-1].startswith("/grind https://")
    assert "-p" not in data["command"]
    assert data["iterations"] == 1


def test_grind_is_refused_on_the_codex_harness() -> None:
    """Codex has no native interactive `/loop`; #1173 fails the launch rather
    than falling back to a clud-managed loop."""
    result = _run("--dry-run", "--codex", "grind", "https://github.com/zackees/clud/issues")
    assert result.returncode == 2
    assert result.stdout == ""
    assert "requires the Claude harness" in result.stderr


def test_openrouter_grind_dry_run_is_one_credential_free_claude_session() -> None:
    """#1808: `clud --openrouter grind --dry-run` resolves the OpenRouter
    route onto the Claude harness as one interactive `/grind` session, with
    the catalog default model, and never reads the vault or the environment's
    key (this HOME has no stored key, so a vault read would hang or error)."""
    env_key = "sk-or-v1-" + "fedcba9876543210" * 2
    url = "https://github.com/zackees/clud/issues/1807"
    result = _run(
        "--dry-run",
        "--openrouter",
        "grind",
        url,
        env_overrides={"OPENROUTER_API_KEY": env_key},
    )
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["model_provider"] == "openrouter"
    assert data["effective_harness"] == "claude"
    assert data["provider_source"] == "cli"
    selection = data["model_selection"]
    assert selection["provider"] == "openrouter"
    assert selection["wire_model"] == "~anthropic/claude-sonnet-latest"
    assert selection["model_source"]
    assert selection["wire_model"] in data["allowed_models"]
    assert data["command"][-1] == f"/grind {url}"
    assert "-p" not in data["command"]
    assert data["iterations"] == 1
    assert data["loop_markers"] is None
    assert env_key not in result.stdout
    assert env_key not in result.stderr
    assert "sk-" not in result.stdout


@pytest.mark.parametrize(
    ("argv", "needle"),
    [
        (
            ("--openrouter", "--harness", "codex"),
            "OpenRouter provider cannot use the Codex harness",
        ),
        (("--openrouter", "--subprocess"), "requires an interactive PTY"),
    ],
)
def test_openrouter_grind_refuses_unsupported_sessions(argv: tuple[str, ...], needle: str) -> None:
    """#1808: each refusal exits 2 before any session or vault access.
    (`--detach` conflicts with `--dry-run` in clap, so its grind refusal is
    pinned by the Rust unit test instead.)"""
    result = _run("--dry-run", *argv, "grind", "https://github.com/zackees/clud/issues")
    assert result.returncode == 2, (result.stdout, result.stderr)
    assert result.stdout == ""
    assert needle in result.stderr


def test_a_fresh_openrouter_grind_launch_is_pinned_to_its_resolved_model() -> None:
    """#1809: with no `--model` and nothing saved, the catalog default is the
    launch's pin (DD-077), so a direct OpenRouter `/grind` session has gateway
    discovery off; only an explicit `--model` changes which id is pinned."""
    url = "https://github.com/zackees/clud/issues/1807"
    fresh = _run("--dry-run", "--openrouter", "grind", url)
    assert fresh.returncode == 0, fresh.stderr
    data = json.loads(fresh.stdout)
    assert data["model_selection"]["model_source"] == "catalog_default"
    assert data["allowed_models"] == ["~anthropic/claude-sonnet-latest"]
    assert data["pinned_from_previous_selection"] is True
    pinned = _run(
        "--dry-run", "--openrouter", "--model", "~anthropic/claude-haiku-latest", "grind", url
    )
    assert pinned.returncode == 0, pinned.stderr
    data = json.loads(pinned.stdout)
    assert data["allowed_models"] == ["~anthropic/claude-haiku-latest"]
    assert data["pinned_from_previous_selection"] is False
    assert data["command"][-1] == f"/grind {url}"


def test_openrouter_free_ids_are_checked_against_the_offline_catalog() -> None:
    """#1833: a `:free` id is a cost promise. The embedded catalog proves the
    nemotron free row is free; an id it cannot prove free is refused before
    any vault read, request or saved default."""
    free = _run(
        "--dry-run", "--openrouter", "--model", "nvidia/nemotron-3-ultra-550b-a55b:free", "-p", "hi"
    )
    assert free.returncode == 0, free.stderr
    assert json.loads(free.stdout)["free_check"] == "free"
    assert "nvidia/nemotron-3-ultra-550b-a55b:free: free (named on the command line)" in (
        free.stderr
    )
    unknown = _run("--dry-run", "--openrouter", "--model", "acme/not-a-model:free", "-p", "hi")
    assert unknown.returncode == 2, (unknown.stdout, unknown.stderr)
    assert unknown.stdout == ""
    assert "cannot confirm it is free" in unknown.stderr
    paid = _run(
        "--dry-run", "--openrouter", "--model", "nvidia/nemotron-3-ultra-550b-a55b", "-p", "hi"
    )
    assert paid.returncode == 0, paid.stderr
    assert json.loads(paid.stdout)["free_check"] == "not_requested"
    assert "per million input/output tokens (named on the command line)" in paid.stderr


def test_openrouter_privacy_flags_reach_both_routes() -> None:
    policies = [("--zdr",), ("--no-data-collection",), ("--zdr", "--no-data-collection")]
    routes = [("--openrouter",), ("--unified", "--model", "openrouter-claude-sonnet")]
    for flags in policies:
        for route_flags in routes:
            route = _dry_run_route(*route_flags, *flags)["route"]
            routing = route["upstream_routing"]
            assert routing["allow_fallbacks"] is True
            if "--zdr" in flags:
                assert routing["zdr"] is True
            if "--no-data-collection" in flags:
                assert routing["data_collection"] == "deny"
            pinned = _dry_run_route(
                *route_flags, *flags, "--provider-only", "parasail/fp8"
            )["route"]["upstream_routing"]
            assert pinned["only"] == ["parasail/fp8"]
            assert pinned["allow_fallbacks"] is False
        refused = _run("--dry-run", *flags, "-p", "hi")
        assert refused.returncode == 2, (refused.stdout, refused.stderr)
        assert "OpenRouter" in refused.stderr


def test_provider_only_pins_openrouter_upstream_routing() -> None:
    """`--provider-only` reaches the plan for a direct OpenRouter launch and is
    refused everywhere else, or for a malformed slug, before any session."""
    model = ("--model", "deepseek/deepseek-v4.1-flash")
    pinned = _run(
        "--dry-run", "--openrouter", *model, "--provider-only", "parasail/fp8", "-p", "hi"
    )
    assert pinned.returncode == 0, pinned.stderr
    assert json.loads(pinned.stdout)["provider_only"] == ["parasail/fp8"]
    plain = _run("--dry-run", "--openrouter", *model, "-p", "hi")
    assert plain.returncode == 0, plain.stderr
    assert json.loads(plain.stdout)["provider_only"] == []
    for argv, needle in [
        (("--provider-only", "parasail/fp8"), "only to an OpenRouter launch"),
        (("--openrouter", *model, "--provider-only", "Parasail"), "not an OpenRouter provider"),
    ]:
        refused = _run("--dry-run", *argv, "-p", "hi")
        assert refused.returncode == 2, (refused.stdout, refused.stderr)
        assert refused.stdout == ""
        assert needle in refused.stderr


def test_provider_only_works_on_the_unified_gateway() -> None:
    """#1863: the gateway owns the request path, so it injects the same object
    the direct route hands the harness through CLAUDE_CODE_EXTRA_BODY."""
    unified = _dry_run_route(
        "--unified",
        "--model",
        "openrouter-claude-sonnet",
        "--provider-only",
        "parasail/fp8",
    )["route"]
    assert unified["backend"] == "unified"
    assert unified["upstream_routing"] == {
        "only": ["parasail/fp8"],
        "allow_fallbacks": False,
    }
    # The direct route builds the identical value: one decision, two renderers.
    direct = _dry_run_route(
        "--openrouter", "--provider-only", "parasail/fp8"
    )["route"]
    assert direct["upstream_routing"] == unified["upstream_routing"]
    # And where the object cannot be carried, the check says so.
    bridge = _dry_run_route("--codex")["route"]
    checks = {check["name"]: check for check in bridge["checks"]}
    assert checks["provider_only"]["applicability"]["state"] == "not_applicable"


def _dry_run_route(*args: str) -> dict:
    result = _run("--dry-run", *args, "-p", "hi")
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def test_dry_run_exposes_the_resolved_route() -> None:
    """#1855: one `route` object carries every routing decision the launch made
    -- backend, slots, allowlist, context, effort, upstream routing and the
    launch checks with their verdicts -- so routing is auditable without a
    paid request."""
    route = _dry_run_route("--openrouter")["route"]
    assert route["backend"] == "direct"
    assert route["provider"] == "openrouter"
    assert route["slots"]["main"]["wire_id"] == "~anthropic/claude-sonnet-latest"
    # The catalog's reviewed window for the reviewed Sonnet row.
    assert route["context"]["max_tokens"] == 1000000
    assert "per_turn_capability" in route["effort"]
    assert route["upstream_routing"] is None
    assert route["timeout_ms"] == 600000
    assert [check["name"] for check in route["checks"]] == [
        "request_path_features",
        "openrouter_free_cost",
        "provider_only",
        "secret_redaction",
    ]
    # D21: the request-path features are named, and the direct route says why
    # they are not offered here.
    request_path = route["checks"][0]
    assert request_path["applicability"] == {
        "state": "not_applicable",
        "reason": "clud is not in the request path on a direct route",
    }
    assert "no clud egress by design" in request_path["verdict"]
    assert all(check["verdict"] for check in route["checks"])
    # The upstream is the descriptor's, and the credential it names is an
    # identifier -- never a key.
    assert route["upstreams"][0]["base_url"] == "https://openrouter.ai/api"
    assert route["upstreams"][0]["credential"] == {
        "source": "vault",
        "value": "openrouter",
    }


def test_dry_run_route_reflects_a_pin_and_provider_only() -> None:
    pinned = _dry_run_route(
        "--openrouter",
        "--allow-model",
        "openrouter-claude-sonnet",
        "--provider-only",
        "parasail/fp8",
    )["route"]
    assert pinned["allowlist"] == ["openrouter-claude-sonnet"]
    # A pin turns discovery off: clud does not advertise rows the boundary
    # would then have to be enforced against (DD-054).
    assert pinned["discovery"]["policy"] == "off"
    slots = ("main", "opus", "sonnet", "haiku", "fable", "subagent")
    assert len({pinned["slots"][slot]["wire_id"] for slot in slots}) == 1
    assert pinned["upstream_routing"] == {"only": ["parasail/fp8"], "allow_fallbacks": False}


def test_a_plain_claude_launch_resolves_to_the_native_backend() -> None:
    """A plain Claude launch has no provider overlay, so it re-derives nothing;
    the route says so instead of inventing slots it never sets."""
    route = _dry_run_route("--claude")["route"]
    assert route["backend"] == "native"
    assert route["upstreams"] == []
    assert all(
        route["slots"][slot] is None
        for slot in ("main", "opus", "sonnet", "haiku", "fable", "subagent")
    )
    assert route["scrub"] == []


def test_do_missing_target_never_consumes_piped_input_or_prompts() -> None:
    for argv in [
        ("--dry-run", "--codex", "do"),
        ("--codex", "do"),
    ]:
        result = _run(*argv, input_data="must remain unread\n")
        assert result.returncode == 2
        assert result.stdout == ""
        assert "requires a URL or goal in non-interactive mode" in result.stderr
        assert "Enter an issue URL or goal" not in result.stderr


def test_codex_builtin_rejects_bare_resume_without_a_session() -> None:
    result = _run(
        "--dry-run",
        "--codex",
        "--resume",
        "do",
        "https://github.com/zackees/clud/issues/1036",
    )
    assert result.returncode == 2
    assert "--resume=<session>" in result.stderr
    assert result.stdout == ""


def test_do_dotted_free_form_goal_is_not_misclassified_as_a_url() -> None:
    result = _run("--dry-run", "--codex", "do", "README.md")
    assert result.returncode == 0, result.stderr
    prompt = json.loads(result.stdout)["command"][-1]
    assert prompt.startswith("/goal ")
    assert "The goal: README.md" in prompt
    assert "Read the issue and implement it" not in prompt


def test_dry_run_deepseek() -> None:
    result = _run("--dry-run", "--deepseek", "-p", "hello")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["backend"] == "claude"
    assert data["model_provider"] == "deepseek"
    assert data["requested_harness"] == "default"
    assert data["effective_harness"] == "claude"
    assert data["provider_source"] == "cli"
    # #1192: the served DeepSeek model name (built-in copy here, because the
    # harness sets CLUD_SERVER_SETTINGS=0) selects DeepSeek V4.1 Flash.
    assert data["model_selection"]["model"] == "deepseek-flash"
    assert data["model_selection"]["wire_model"] == "deepseek-flash[1m]"
    assert data["model_selection"]["model_source"] == "server_default"
    assert data["model_selection"]["context_window"] == "1m"
    # --dry-run must never touch the native credential vault: this isolated
    # HOME has no stored key, and a vault read/prompt would either hang on
    # stdin (killed by _run's timeout) or surface a vault error instead of a
    # clean dry-run JSON payload.
    assert "sk-" not in result.stdout
    assert "sk-" not in result.stderr


def test_bare_and_explicit_run_dry_runs_remain_credential_free() -> None:
    # Credential-aware routing is a live-launch fallback, never a dry-run
    # vault or external-auth-status probe.
    for argv in (("--dry-run",), ("run", "--dry-run")):
        result = _run(*argv)
        assert result.returncode == 0, result.stderr
        data = json.loads(result.stdout)
        assert data["model_provider"] == "claude"
        assert data["provider_source"] == "built_in_default"


_INLINE_DEEPSEEK_KEY = "sk-" + "0123456789abcdef" * 2


@pytest.mark.parametrize(
    "flag", ["-deepseek", "-\u2013deepseek", "\u2014deepseek", "\u2212deepseek"]
)
def test_near_miss_exact_deepseek_aliases_select_provider(flag: str) -> None:
    result = _run("--dry-run", flag, _INLINE_DEEPSEEK_KEY)
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["model_provider"] == "deepseek"
    assert data["provider_source"] == "cli"
    assert _INLINE_DEEPSEEK_KEY not in result.stdout + result.stderr


@pytest.mark.parametrize("flag", ["-deepseek", "-\u2013deepseek"])
def test_deepseek_alias_equals_form_extracts_key(flag: str) -> None:
    result = _run("--dry-run", f"{flag}={_INLINE_DEEPSEEK_KEY}")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["model_provider"] == "deepseek"
    assert data["provider_source"] == "cli"
    assert _INLINE_DEEPSEEK_KEY not in result.stdout + result.stderr


@pytest.mark.parametrize("flag", ["--deepskeek", "-\u2013deepskeek"])
def test_provider_flag_typo_with_key_fails_without_echoing_key(flag: str) -> None:
    result = _run("--dry-run", flag, _INLINE_DEEPSEEK_KEY)
    assert result.returncode == 2
    assert flag in result.stderr
    assert "--deepseek" in result.stderr
    assert _INLINE_DEEPSEEK_KEY not in result.stdout + result.stderr


def test_dry_run_masks_explicit_backend_key_argument() -> None:
    result = _run("--dry-run", "--", _INLINE_DEEPSEEK_KEY)
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["command"][-1] == "****cdef"
    assert _INLINE_DEEPSEEK_KEY not in result.stdout


def test_bare_subcommand_near_miss_is_only_a_notice() -> None:
    result = _run("--dry-run", "looop")
    assert result.returncode == 0, result.stderr
    assert "did you mean 'loop'" in result.stderr
    assert "looop" in json.loads(result.stdout)["command"]


def test_dry_run_deepseek_inline_key_is_never_forwarded() -> None:
    """`clud --deepseek <API_KEY>` must not hand the key to the backend as a
    prompt (the 2.8.1 bug: Windows users' keys were never recognized). A dry
    run also never touches the vault, so the key appears nowhere."""
    result = _run("--dry-run", "--deepseek", _INLINE_DEEPSEEK_KEY)
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["model_provider"] == "deepseek"
    assert _INLINE_DEEPSEEK_KEY not in result.stdout
    assert _INLINE_DEEPSEEK_KEY not in result.stderr
    assert all(_INLINE_DEEPSEEK_KEY not in part for part in data["command"])


def test_dry_run_deepseek_inline_key_keeps_the_real_prompt() -> None:
    result = _run("--dry-run", "--deepseek", _INLINE_DEEPSEEK_KEY, "fix the bug")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["command"][-1] == "fix the bug"
    assert _INLINE_DEEPSEEK_KEY not in result.stdout


@pytest.mark.parametrize(
    ("flag", "key"),
    [
        ("--kimi", "sk-" + "a1b2c3d4" * 4),
        ("--openrouter", "sk-or-v1-" + "0123456789abcdef" * 2),
    ],
)
def test_dry_run_inline_keys_for_other_api_key_providers(flag: str, key: str) -> None:
    result = _run("--dry-run", flag, key)
    assert result.returncode == 0, result.stderr
    assert key not in result.stdout
    assert key not in result.stderr


@pytest.mark.parametrize(
    ("flag", "key", "provider"),
    [
        ("--deepseek", _INLINE_DEEPSEEK_KEY, "deepseek"),
        ("--kimi", "sk-" + "a1b2c3d4" * 4, "kimi"),
        ("--openrouter", "sk-or-v1-" + "0123456789abcdef" * 2, "openrouter"),
    ],
)
def test_dry_run_inline_key_equals_form_matches_the_space_form(
    flag: str, key: str, provider: str
) -> None:
    """#1197: `--deepseek=<key>` launched plain Claude and forwarded the key to
    the harness as an unknown flag."""
    spaced = _run("--dry-run", flag, key, "-p", "hi")
    equals = _run("--dry-run", f"{flag}={key}", "-p", "hi")
    assert spaced.returncode == 0, spaced.stderr
    assert equals.returncode == 0, equals.stderr
    spaced_data = json.loads(spaced.stdout)
    equals_data = json.loads(equals.stdout)
    assert equals_data["model_provider"] == provider
    assert equals_data["model_selection"] == spaced_data["model_selection"]
    assert equals_data["command"][-2:] == ["-p", "hi"]
    assert all(key not in part for part in equals_data["command"])
    assert key not in equals.stdout
    assert key not in equals.stderr


_OPENROUTER_KEY = "sk-or-v1-" + "0123456789abcdef" * 2


def test_dry_run_shows_the_launch_context_record() -> None:
    """#1675: the dry run previews the record the launch would write."""
    result = _run("--dry-run", "--openrouter", _OPENROUTER_KEY, "-p", "hi")
    assert result.returncode == 0, result.stderr
    record = json.loads(result.stdout)["launch_context"]
    assert record["route"] == "direct"
    assert record["provider"] == "openrouter"
    assert record["harness"] == "claude"
    assert record["session"] is None
    assert record["max_context_tokens"]["source"] in {"catalog", "served", "unset"}
    assert _OPENROUTER_KEY not in json.dumps(record)


def test_dry_run_launch_context_reports_an_ambient_max_context() -> None:
    result = _run(
        "--dry-run",
        "--openrouter",
        _OPENROUTER_KEY,
        "-p",
        "hi",
        env_overrides={"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "4242"},
    )
    assert result.returncode == 0, result.stderr
    record = json.loads(result.stdout)["launch_context"]
    assert record["max_context_tokens"] == {"value": 4242, "source": "ambient"}


def test_dry_run_launch_context_native_claude_is_unset() -> None:
    result = _run("--dry-run", "--claude", "-p", "hi")
    assert result.returncode == 0, result.stderr
    record = json.loads(result.stdout)["launch_context"]
    assert record["route"] == "native"
    assert record["max_context_tokens"] == {"value": None, "source": "unset"}


def test_dry_run_inline_key_equals_form_rejects_a_non_key_value() -> None:
    result = _run("--dry-run", "--deepseek=not-a-key", "-p", "hi")
    assert result.returncode == 2, result.stderr
    assert "--deepseek does not take a value" in result.stderr
    assert "not-a-key" not in result.stderr
    assert "not-a-key" not in result.stdout


def test_dry_run_openrouter_is_keyless_and_uses_stable_provider_identity() -> None:
    result = _run("--dry-run", "--openrouter", "-p", "hello")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["backend"] == "claude"
    assert data["model_provider"] == "openrouter"
    assert data["effective_harness"] == "claude"
    assert data["model_selection"]["provider"] == "openrouter"
    assert data["model_selection"]["model"] == "openrouter-claude-sonnet"
    assert data["model_selection"]["wire_model"] == "~anthropic/claude-sonnet-latest"
    assert "sk-or-" not in result.stdout
    assert "sk-or-" not in result.stderr


def test_dry_run_exposes_the_failover_ladder() -> None:
    """Routing must be auditable without a paid request, and a ladder is
    routing: it decides which account serves the turn after the first declines.
    """
    result = _run(
        "--dry-run", "--unified", "--failover", "claude-opus-4-1,codex-terra", "-p", "hello"
    )
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["routing_mode"] == "unified"
    assert data["failover"] == "claude-opus-4-1,codex-terra"
    assert data["failover_allow_metered"] is False


def test_dry_run_accepts_a_kimi_rung_now_that_the_gateway_routes_kimi() -> None:
    """#937 Phase 4: Kimi has a unified route, so a `kimi-k3` rung parses.
    Before, it failed the launch as unroutable."""
    result = _run("--dry-run", "--unified", "--failover", "kimi-k3", "-p", "hello")
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout)["failover"] == "kimi-k3"


def test_dry_run_kimi() -> None:
    result = _run("--dry-run", "--kimi", "-p", "hello")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["backend"] == "claude"
    assert data["model_provider"] == "kimi"
    assert data["effective_harness"] == "claude"
    assert data["provider_source"] == "cli"
    assert data["model_selection"]["provider"] == "kimi"
    assert data["model_selection"]["model"] == "kimi-k3"
    assert data["model_selection"]["wire_model"] == "kimi-k3[1m]"
    # #1097: direct-provider launches default to low effort on the session.
    assert data["model_selection"]["effort"] == "low"
    assert data["model_selection"]["context_window"] == "1m"
    # Same vault contract as DeepSeek: this isolated HOME has no Kimi key, so
    # any vault read or login prompt would hang or error instead.
    assert "sk-" not in result.stdout + result.stderr


def test_dry_run_kimi_rejects_codex_harness_and_conflicting_flags() -> None:
    result = _run("--dry-run", "--kimi", "--harness", "codex", "-p", "hello")
    assert result.returncode == 2
    assert "kimi" in result.stderr.lower()
    assert "cannot use the codex harness" in result.stderr.lower()
    for other in ("--claude", "--codex", "--deepseek", "--unified"):
        result = _run("--dry-run", "--kimi", other, "-p", "hello")
        assert result.returncode != 0, other
        assert result.stdout == "", other


def test_failover_on_a_direct_launch_warns_that_it_does_nothing() -> None:
    """A direct launch has no gateway to fail over inside; say so rather than
    silently accepting a flag that cannot take effect."""
    result = _run("--dry-run", "--claude", "--failover", "claude-opus-4-1", "-p", "hello")
    assert result.returncode == 0, result.stderr
    assert "--failover only applies to `--unified`" in result.stderr


def test_dry_run_openrouter_rejects_codex_harness() -> None:
    result = _run("--dry-run", "--openrouter", "--harness", "codex", "-p", "hello")
    assert result.returncode == 2
    output = (result.stdout or "") + (result.stderr or "")
    assert "openrouter" in output.lower()
    assert "cannot use the codex harness" in output.lower()


def test_dry_run_deepseek_rejects_codex_harness() -> None:
    result = _run("--dry-run", "--deepseek", "--harness", "codex", "-p", "hello")
    assert result.returncode == 2
    assert "deepseek" in result.stderr.lower()
    assert "cannot use the codex harness" in result.stderr.lower()


def test_dry_run_deepseek_rejects_model_override() -> None:
    result = _run("--dry-run", "--deepseek", "--model", "opus", "-p", "hello")
    assert result.returncode == 2
    # The provider resolver rejects a cross-provider model before bootstrap and
    # names both the offending model's provider and the requested one (#904).
    # Check the combined streams: running-process versions differ on whether a
    # diagnostic lands on stdout or stderr.
    output = (result.stdout or "") + (result.stderr or "")
    assert "opus" in output
    assert "deepseek" in output


def test_dry_run_deepseek_conflicts_with_claude_and_codex_flags() -> None:
    for other in ("--claude", "--codex"):
        result = _run("--dry-run", "--deepseek", other, "-p", "hello")
        assert result.returncode != 0
        assert result.stdout == ""


def test_dry_run_reports_independent_provider_and_harness() -> None:
    result = _run(
        "--dry-run",
        "--codex",
        "--harness",
        "claude",
        "-p",
        "hello",
    )
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["model_provider"] == "codex"
    assert data["requested_harness"] == "claude"
    assert data["effective_harness"] == "claude"
    assert data["provider_source"] == "cli"
    assert data["harness_source"] == "cli"
    assert data["backend"] == "claude"
    assert "--harness" not in data["command"]
    model_index = data["command"].index("--model")
    assert data["model_selection"]["model"] == "codex-sol"
    assert data["model_selection"]["wire_model"] == "gpt-6-sol"
    assert data["codex_model_source"] in {"published", "last_good", "built_in"}
    assert data["model_selection"]["effort"] == "low"
    assert data["model_selection"]["model_source"] == "catalog_default"
    assert data["model_selection"]["effort_source"] == "catalog_default"
    assert data["command"][model_index + 1] == "clud-claude-codex-sol"
    settings = json.loads(data["command"][data["command"].index("--settings") + 1])
    assert settings["effortLevel"] == "low"
    assert "--effort" not in data["command"]
    assert not any("terra" in arg or arg == "medium" for arg in data["command"])


@pytest.mark.parametrize("harness", ["codex", "claude"])
def test_saved_codex_profile_overrides_sol_low_default(tmp_path: Path, harness: str) -> None:
    home = tmp_path / harness / "home"
    state_dir = tmp_path / harness / "state"
    settings_dir = home / ".clud"
    settings_dir.mkdir(parents=True)
    (settings_dir / "settings.json").write_text(
        json.dumps({"providers": {"codex": {"model": "codex-terra", "effort": "high"}}}),
        encoding="utf-8",
    )
    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        result = process.run(
            [
                str(launch),
                "--dry-run",
                "--codex",
                "--harness",
                harness,
                "-p",
                "hello",
            ],
            capture_output=True,
            text=True,
            timeout=10,
            env=_isolated_clud_env(source, home, state_dir),
        )
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["model_selection"]["model"] == "codex-terra"
    assert data["model_selection"]["wire_model"] == "gpt-5.6-terra"
    assert data["model_selection"]["effort"] == "high"
    assert data["model_selection"]["model_source"] == "provider_setting"
    assert data["model_selection"]["effort_source"] == "provider_setting"
    if harness == "codex":
        assert "gpt-5.6-terra" in data["command"]
        assert 'model_reasoning_effort="high"' in data["command"]
    else:
        assert "clud-claude-codex-terra" in data["command"]
        settings = json.loads(data["command"][data["command"].index("--settings") + 1])
        assert settings["effortLevel"] == "high"


def test_dry_run_deepseek_carries_low_catalog_effort_on_the_session_flag() -> None:
    """DD-059: the direct DeepSeek default effort rides `--effort`, not an env pin."""
    result = _run("--dry-run", "--deepseek", "-p", "hello")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["model_selection"]["effort"] == "low"
    settings = json.loads(data["command"][data["command"].index("--settings") + 1])
    assert settings["effortLevel"] == "low"
    assert "--effort" not in data["command"]


def test_dry_run_keeps_sol_selected_through_the_claude_loop_path() -> None:
    """Issue #955: loop/repeat planning must not lose the discovery address."""
    result = _run(
        "--dry-run",
        "--codex",
        "--harness",
        "claude",
        "--model",
        "codex-sol",
        "--effort",
        "low",
        "loop",
        "--loop-count",
        "1",
        "--no-done",
        "reply once",
    )
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["iterations"] == 1
    assert data["model_selection"]["wire_model"] == "gpt-6-sol"
    model_index = data["command"].index("--model")
    assert data["command"][model_index + 1] == "clud-claude-codex-sol"
    settings = json.loads(data["command"][data["command"].index("--settings") + 1])
    assert settings["effortLevel"] == "low"
    assert "--effort" not in data["command"]


def test_dry_run_uses_global_harness_and_cli_can_override_it(tmp_path: Path) -> None:
    home = tmp_path / "home"
    state_dir = tmp_path / "state"
    settings_dir = home / ".clud"
    settings_dir.mkdir(parents=True)
    (settings_dir / "settings.json").write_text(
        json.dumps(
            {
                "backend": {"default": "codex"},
                "harness": {"default": "claude"},
            }
        ),
        encoding="utf-8",
    )

    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        env = _isolated_clud_env(source, home, state_dir)
        saved = process.run(
            [str(launch), "--dry-run", "-p", "hello"],
            capture_output=True,
            text=True,
            timeout=10,
            env=env,
        )
        overridden = process.run(
            [
                str(launch),
                "--dry-run",
                "--codex",
                "--harness",
                "codex",
                "-p",
                "hello",
            ],
            capture_output=True,
            text=True,
            timeout=10,
            env=env,
        )

    assert saved.returncode == 0, saved.stderr
    saved_data = json.loads(saved.stdout)
    assert "Harness override:" not in saved.stderr
    assert saved_data["model_provider"] == "codex"
    assert saved_data["provider_source"] == "global_setting"
    assert saved_data["requested_harness"] == "claude"
    assert saved_data["effective_harness"] == "claude"
    assert saved_data["harness_source"] == "global_setting"

    assert overridden.returncode == 0, overridden.stderr
    overridden_data = json.loads(overridden.stdout)
    assert overridden_data["effective_harness"] == "codex"
    assert overridden_data["harness_source"] == "cli"


def test_saved_harness_override_is_announced_once_in_green_on_tty(
    tmp_path: Path,
) -> None:
    if sys.platform == "win32":
        # Python's stdlib has no ConPTY API. The pure Rust policy test covers
        # Windows; this subprocess wiring test runs on Unix CI with openpty.
        return
    # The launch-scoped Codex bridge preflight (#1095) refuses without a
    # credential, and the fixture's isolated HOME has none by construction —
    # the launch dies at admission on every machine, so the test has been
    # red since #1095. Re-enable once the upstream is mocked hermetically
    # (the mock-agent integration suite is the model).
    pytest.skip(
        "requires a Codex bridge credential in the isolated env; "
        "hermetic upstream mock is a follow-up"
    )

    import errno
    import pty
    import select

    repo = tmp_path / "repo"
    home = tmp_path / "home"
    state_dir = tmp_path / "state"
    fake_bin = tmp_path / "bin"
    repo.mkdir()
    home.mkdir()
    _fake_claude_on_path(fake_bin)
    settings_dir = home / ".clud"
    settings_dir.mkdir()
    (settings_dir / "settings.json").write_text(
        json.dumps(
            {
                "backend": {"default": "codex"},
                "harness": {"default": "claude"},
            }
        ),
        encoding="utf-8",
    )

    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        env = _isolated_clud_env(source, home, state_dir)
        env["PATH"] = str(fake_bin) + os.pathsep + env.get("PATH", "")
        master, slave = pty.openpty()
        try:
            child = process.Popen(
                [
                    str(launch),
                    "--no-daemon",
                    "--no-fix-hooks",
                    "--no-cpu-banner",
                    "--no-dnd",
                    "--subprocess",
                    "-p",
                    "hello",
                ],
                cwd=repo,
                env=env,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                close_fds=True,
            )
        finally:
            os.close(slave)

        output = bytearray()
        deadline = time.monotonic() + 15
        try:
            while time.monotonic() < deadline:
                readable, _, _ = select.select([master], [], [], 0.1)
                if readable:
                    try:
                        output.extend(os.read(master, 4096))
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                        break
                if child.poll() is not None and not readable:
                    break
            returncode = child.wait(timeout=1)
        finally:
            os.close(master)

    text = output.decode(errors="replace") or str(child.stdout.read())
    notice = "[clud] Harness override: Claude (global setting)"
    assert returncode == 0, text
    assert text.count(notice) == 1
    assert f"\x1b[32m{notice}\x1b[0m" in text


def _run_on_tty(launch: Path, env: dict[str, str], cwd: Path, args: list[str]) -> tuple[int, str]:
    """Run `clud` with a real pty on all three stdio fds, return (rc, output).

    The plan-mode and harness-override notices are both TTY-gated, so a piped
    subprocess cannot observe them at all — the pty is the point of this helper,
    not an incidental detail.
    """
    import errno
    import pty
    import select

    master, slave = pty.openpty()
    try:
        child = process.Popen(
            [str(launch), *args],
            cwd=cwd,
            env=env,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            close_fds=True,
        )
    finally:
        os.close(slave)

    output = bytearray()
    deadline = time.monotonic() + 15
    try:
        while time.monotonic() < deadline:
            readable, _, _ = select.select([master], [], [], 0.1)
            if readable:
                try:
                    output.extend(os.read(master, 4096))
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    break
            if child.poll() is not None and not readable:
                break
        returncode = child.wait(timeout=1)
    finally:
        os.close(master)

    return returncode, output.decode(errors="replace") or str(child.stdout.read())


def _run_picker_on_tty(
    launch: Path,
    env: dict[str, str],
    cwd: Path,
) -> tuple[int, str]:
    """Run a bare launch through the harness picker."""
    terminal_before = cwd / f"terminal-before-{time.time_ns()}"
    terminal_after = cwd / f"terminal-after-{time.time_ns()}"
    child_env = dict(env)
    child_env["CLUD_LAUNCH"] = str(launch)
    child_env["TERM_BEFORE"] = str(terminal_before)
    child_env["TERM_AFTER"] = str(terminal_after)
    child_env["PICKER_SHELL"] = """
stty -g > "$TERM_BEFORE"
"$CLUD_LAUNCH" --no-daemon --no-fix-hooks --no-cpu-banner --no-dnd --subprocess
launch_status=$?
stty -g > "$TERM_AFTER"
exit "$launch_status"
"""
    python_command = """
import os
import pty

status = pty.spawn(["sh", "-c", os.environ["PICKER_SHELL"]])
raise SystemExit(os.waitstatus_to_exitcode(status))
"""
    result = process.run(
        [sys.executable, "-c", python_command],
        cwd=cwd,
        env=child_env,
        capture_output=True,
        text=True,
        timeout=20,
    )

    assert terminal_before.read_text(encoding="utf-8") == terminal_after.read_text(encoding="utf-8")
    return result.returncode, (result.stdout or "") + (result.stderr or "")


def test_installed_harness_picker_launches_default_and_restores_terminal(
    tmp_path: Path,
) -> None:
    if sys.platform == "win32":
        # Python's stdlib has no ConPTY API; the picker state machine and the
        # Windows crossterm build are covered by the Rust suite.
        pytest.skip("running-process has no supported Windows test PTY")
    pytest.skip(
        "disabled: fake harness log records claude, codex, claude instead of "
        "claude alone; re-enable once the picker's extra launches are understood"
    )

    repo = tmp_path / "repo"
    home = tmp_path / "home"
    state_dir = tmp_path / "state"
    fake_bin = tmp_path / "bin"
    harness_log = tmp_path / "harness.log"
    repo.mkdir()
    home.mkdir()
    _fake_harnesses_on_path(fake_bin)

    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        env = _isolated_clud_env(source, home, state_dir)
        env["PATH"] = str(fake_bin) + os.pathsep + "/usr/bin" + os.pathsep + "/bin"
        env["HARNESS_LOG"] = str(harness_log)

        returncode, output = _run_picker_on_tty(launch, env, repo)
        assert returncode == 0, output
        assert harness_log.read_text(encoding="utf-8").splitlines() == ["claude"]


def test_plan_mode_suppression_is_announced_once_in_green_on_tty(
    tmp_path: Path,
) -> None:
    """Covers the `main.rs` wiring of the DD-033 notice, not just its formatter.

    `plan_mode_suppression_notice` has a Rust unit test for the string and its
    gating. What only a real launch can catch is the call site: a dropped or
    duplicated block, a wrong `stderr_is_terminal`, or the block drifting above
    the `--dry-run` early-exit and polluting the JSON.
    """
    if sys.platform == "win32":
        # Python's stdlib has no ConPTY API. The pure Rust policy test covers
        # Windows; this subprocess wiring test runs on Unix CI with openpty.
        return
    # The launch-scoped Codex bridge preflight (#1095) refuses without a
    # credential, and the fixture's isolated HOME has none by construction —
    # the launch dies at admission on every machine, so the test has been
    # red since #1095. Re-enable once the upstream is mocked hermetically
    # (the mock-agent integration suite is the model).
    pytest.skip(
        "requires a Codex bridge credential in the isolated env; "
        "hermetic upstream mock is a follow-up"
    )

    repo = tmp_path / "repo"
    home = tmp_path / "home"
    state_dir = tmp_path / "state"
    fake_bin = tmp_path / "bin"
    repo.mkdir()
    home.mkdir()
    _fake_claude_on_path(fake_bin)

    notice_marker = "[clud] Plan mode disabled on the non-Claude bridge"
    base = [
        "--no-daemon",
        "--no-fix-hooks",
        "--no-cpu-banner",
        "--no-dnd",
        "--subprocess",
    ]
    bridge = ["--codex", "--harness", "claude"]

    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        env = _isolated_clud_env(source, home, state_dir)
        env["PATH"] = str(fake_bin) + os.pathsep + env.get("PATH", "")

        def run(args: list[str]) -> tuple[int, str]:
            return _run_on_tty(launch, env, repo, args)

        rc, text = run([*base, *bridge, "-p", "hello"])
        assert rc == 0, text
        # `count == 1` is the assertion that matters: a substring check would
        # pass just as happily on a duplicated emission.
        assert text.count(notice_marker) == 1, text
        assert f"\x1b[32m{notice_marker}" in text, text
        # The override has to reach the user in the message itself, not only in
        # `--help` — the notice is the only place they learn it exists.
        assert "--allow-plan-mode" in text, text

        # Opted out: nothing suppressed, so nothing announced.
        rc, text = run([*base, *bridge, "--allow-plan-mode", "-p", "hello"])
        assert rc == 0, text
        assert notice_marker not in text, text

        # The rule is scoped to the bridge. A plain Claude launch keeps plan
        # mode and must stay silent.
        rc, text = run([*base, "-p", "hello"])
        assert rc == 0, text
        assert notice_marker not in text, text

        # `--dry-run` exits before the notice block, keeping machine-readable
        # output clean. This pins that ordering.
        rc, text = run([*base, *bridge, "--dry-run", "-p", "hello"])
        assert rc == 0, text
        assert notice_marker not in text, text


def test_dry_run_rejects_unsupported_claude_provider_via_codex_harness() -> None:
    result = _run(
        "--dry-run",
        "--claude",
        "--harness",
        "codex",
        "-p",
        "hello",
    )
    assert result.returncode != 0
    assert (
        "unsupported launch target: Claude provider cannot use the Codex harness" in result.stderr
    )


def test_fix_hooks_rejects_unsupported_claude_provider_via_codex_harness() -> None:
    result = _run(
        "--dry-run",
        "--fix-hooks",
        "--claude",
        "--harness",
        "codex",
    )
    assert result.returncode == 2
    assert (
        "unsupported launch target: Claude provider cannot use the Codex harness" in result.stderr
    )


def test_settings_list_reports_model_and_harness_choices(tmp_path: Path) -> None:
    home = tmp_path / "home"
    state_dir = tmp_path / "state"
    settings_dir = home / ".clud"
    settings_dir.mkdir(parents=True)
    (settings_dir / "settings.json").write_text(
        json.dumps(
            {
                "backend": {"default": "codex"},
                "harness": {"default": "claude"},
            }
        ),
        encoding="utf-8",
    )

    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        result = process.run(
            [str(launch), "settings", "--list"],
            capture_output=True,
            text=True,
            timeout=10,
            env=_isolated_clud_env(source, home, state_dir),
        )

    assert result.returncode == 0, result.stderr
    assert "backend.default = codex" in result.stdout
    assert "harness.default = claude" in result.stdout


def test_dry_run_codex_reports_project_doc_fallback(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    home = tmp_path / "home"
    repo.mkdir()
    home.mkdir()
    (repo / "CODEX.md").write_text("codex fallback", encoding="utf-8")

    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        env = copied_clud_env(source)
        env["HOME"] = str(home)
        env["USERPROFILE"] = str(home)
        env["CLUD_HOOK_HOME"] = str(home)
        result = process.run(
            [str(launch), "--dry-run", "--codex", "-p", "hello"],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            timeout=10,
        )

    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert 'project_doc_fallback_filenames=["CODEX.md"]' in data["command"]
    # #1847: Codex launches route approvals to Codex auto review.
    assert 'approvals_reviewer="auto_review"' in data["command"]


def test_dry_run_pty_override() -> None:
    result = _run("--dry-run", "--pty", "-p", "hello")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert data["backend"] == "claude"
    assert data["launch_mode"] == "pty"


def test_dry_run_safe_mode() -> None:
    result = _run("--dry-run", "--safe", "-p", "hello")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert "--dangerously-skip-permissions" not in data["command"]


def test_dry_run_model() -> None:
    result = _run("--dry-run", "--model", "opus", "-p", "hello")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert "--model" in data["command"]
    assert "opus" in data["command"]


def test_dry_run_without_a_model_pins_only_the_previous_selection() -> None:
    """No --model means the boundary is inherited from whatever the previous
    selection resolved to -- and only that (#1257)."""
    result = _run("--dry-run", "-p", "hello")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    selection = data["model_selection"]
    if selection is None:
        # Nothing resolved: nothing pinned, nothing to announce.
        assert data["allowed_models"] == []
        assert data["pinned_from_previous_selection"] is False
    else:
        expected = selection.get("wire_model") or selection.get("model")
        assert data["allowed_models"] == [expected]
        assert data["pinned_from_previous_selection"] is True


def test_dry_run_openrouter_default_is_pinned_and_inherited() -> None:
    result = _run("--dry-run", "--openrouter", "-p", "hello")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["allowed_models"] == ["~anthropic/claude-sonnet-latest"]
    assert data["pinned_from_previous_selection"] is True


def test_dry_run_explicit_model_is_a_typed_boundary_not_an_inherited_pin() -> None:
    """The #1257 repro: an uncataloged OpenRouter id resolves, becomes the
    whole boundary, and is not reported as an inherited pin."""
    result = _run("--dry-run", "--openrouter", "--model", "xiaomi/mimo-v2.6-flash", "-p", "hello")
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["model_selection"]["wire_model"] == "xiaomi/mimo-v2.6-flash"
    assert data["allowed_models"] == ["xiaomi/mimo-v2.6-flash"]
    assert data["pinned_from_previous_selection"] is False


def test_dry_run_repeated_allow_model_builds_the_boundary() -> None:
    result = _run(
        "--dry-run",
        "--openrouter",
        "--model",
        "openrouter-claude-sonnet",
        "--allow-model",
        "openrouter-claude-sonnet",
        "--allow-model",
        "openrouter-claude-opus",
        "-p",
        "hello",
    )
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["allowed_models"] == [
        "openrouter-claude-sonnet",
        "openrouter-claude-opus",
    ]
    assert data["pinned_from_previous_selection"] is False


def test_dry_run_a_pin_outside_an_explicit_allow_list_fails_at_launch() -> None:
    """Validation runs before bootstrap: the failure names the boundary, not
    the first turn that tripped over it."""
    result = _run(
        "--dry-run",
        "--openrouter",
        "--model",
        "xiaomi/mimo-v2.6-flash",
        "--allow-model",
        "openrouter-claude-sonnet",
        "-p",
        "hello",
    )
    assert result.returncode == 2
    output = (result.stdout or "") + (result.stderr or "")
    assert "xiaomi/mimo-v2.6-flash" in output
    assert "--allow-model" in output
    assert "openrouter-claude-sonnet" in output


def test_dry_run_continue() -> None:
    result = _run("--dry-run", "-c")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert "--continue" in data["command"]


def test_dry_run_message() -> None:
    result = _run("--dry-run", "-m", "fix bug")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert "-m" in data["command"]
    assert "fix bug" in data["command"]


def test_dry_run_up() -> None:
    result = _run("--dry-run", "up")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    prompt = data["command"][-1]
    assert "lint" in prompt.lower()
    assert "codeup" in prompt.lower()


def test_dry_run_up_with_message() -> None:
    result = _run("--dry-run", "up", "-m", "bump version")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    prompt = data["command"][-1]
    assert 'codeup -m "bump version"' in prompt


def test_dry_run_up_with_publish() -> None:
    result = _run("--dry-run", "up", "--publish")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    prompt = data["command"][-1]
    assert "-p" in prompt.split("codeup")[1]


def test_dry_run_rebase() -> None:
    result = _run("--dry-run", "rebase")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    prompt = data["command"][-1]
    assert "git fetch" in prompt
    assert "rebase" in prompt.lower()


def test_dry_run_do_seeds_the_rendered_goal_for_a_single_issue() -> None:
    result = _run("--dry-run", "--codex", "do", "https://github.com/o/r/issues/1")
    assert result.returncode == 0, result.stderr
    seed = json.loads(result.stdout)["command"][-1]
    assert seed.startswith("/goal ")
    assert "rendered for this checkout" in seed
    assert "https://github.com/o/r/issues/1" in seed
    assert "Read the issue and implement it" in seed
    assert not seed.startswith("/goal /do ")


def _run_do_with_kind(
    kind: str | None, target: str, *, harness: str = "codex"
) -> process.CompletedProcess[str]:
    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        home = Path(temp_dir) / "home"
        home.mkdir()
        env = _isolated_clud_env(source, home, Path(temp_dir) / "state")
        env.pop("CLUD_DO_KIND")
        if kind is not None:
            env["CLUD_DO_KIND"] = kind
        return process.run(
            [str(launch), "--dry-run", "--codex", "--harness", harness, "do", target],
            capture_output=True,
            text=True,
            timeout=10,
            env=env,
        )


def test_dry_run_do_delivers_meta_handoff_through_claude() -> None:
    result = _run_do_with_kind("meta", "https://github.com/o/r/issues/1", harness="claude")
    assert result.returncode == 0, result.stderr
    command = json.loads(result.stdout)["command"]
    goal = command[-1]
    assert goal.startswith("/goal ")
    # Claude caps the /goal condition at 4000 chars (#2372); the rendered
    # procedure rides in the appended system prompt instead.
    assert len(goal) - len("/goal ") < 4000
    assert not goal.startswith("/goal /grind ")
    seed = command[command.index("--append-system-prompt") + 1]
    assert "rendered for this checkout" in seed
    assert "This is a meta issue" in seed
    assert "Workflow" in seed


@pytest.mark.parametrize("harness", ["codex", "deepseek"])
def test_do_meta_refuses_harnesses_without_workflow(harness: str) -> None:
    result = _run_do_with_kind("meta", "https://github.com/o/r/issues/1", harness=harness)
    assert result.returncode == 2
    assert result.stdout == ""
    assert "requires the Claude harness" in result.stderr
    assert "Workflow" in result.stderr


def test_do_refuses_when_it_cannot_tell_whether_an_issue_is_meta() -> None:
    # A non-GitHub issue cannot be queried for sub-issues, so clud refuses
    # rather than guessing. No network is involved.
    result = _run_do_with_kind(None, "https://gitlab.com/g/p/-/issues/3")
    assert result.returncode == 2
    assert result.stdout == ""
    assert "cannot tell whether" in result.stderr
    assert "CLUD_DO_KIND" in result.stderr


def test_dry_run_fix() -> None:
    result = _run("--dry-run", "fix")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    prompt = data["command"][-1].lower()
    assert "linting" in prompt
    assert "unit tests" in prompt


def test_dry_run_fix_with_url() -> None:
    result = _run("--dry-run", "fix", "https://github.com/user/repo/actions/runs/123")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    prompt = data["command"][-1]
    assert "github.com/user/repo/actions/runs/123" in prompt
    assert "gh run view" in prompt


def test_dry_run_loop() -> None:
    result = _run("--dry-run", "loop", "--loop-count", "5", "do stuff")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert data["iterations"] == 5
    # Prompt is the last arg (Claude's `-p <prompt>`), with the DONE-marker
    # contract appended. Original task is preserved at the start.
    prompt = data["command"][-1]
    assert prompt.startswith("do stuff")
    # Issue #95: contract now embeds the absolute marker path; the relative
    # suffix is still present with platform-native separators.
    normalized_prompt = prompt.replace("\\", "/")
    assert ".clud/loop/DONE" in normalized_prompt
    assert ".clud/loop/BLOCKED" in normalized_prompt
    assert data["loop_markers"] is not None
    assert data["loop_markers"]["done_path"].replace("\\", "/").endswith(".clud/loop/DONE")
    assert data["loop_markers"]["blocked_path"].replace("\\", "/").endswith(".clud/loop/BLOCKED")


@pytest.mark.parametrize("backend", ["--claude", "--codex"])
def test_loop_cumulative_completion_contract(backend: str) -> None:
    task = (
        "once the pr lands in the merge queue you are done for the iteration. "
        "loop terminates after 50 issues have been fixed"
    )
    result = _run(backend, "--dry-run", "loop", "--loop-count", "50", task)
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    prompt = data["command"][-1]
    assert prompt.startswith(task)
    assert data["iterations"] == 50
    assert "DONE terminates the entire loop" in prompt
    assert "Persist cumulative progress" in prompt
    assert "only if the original task explicitly" in prompt
    assert "write DONE with the PR URL as soon as" not in " ".join(prompt.split())


def test_dry_run_loop_no_done() -> None:
    result = _run("--dry-run", "loop", "--no-done", "do stuff")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert data["command"][-1] == "do stuff"
    assert data["loop_markers"] is None


def test_dry_run_loop_repeat_implies_no_done() -> None:
    result = _run("--dry-run", "loop", "--repeat", "1h", "do stuff")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert data["command"][-1] == "do stuff"
    assert data["loop_markers"] is None
    assert data["repeat_interval_secs"] == 3600


def test_dry_run_loop_repeat_emits_warning_to_stderr() -> None:
    # Issue #61 acceptance: when `--repeat` is supplied without `--done`, the
    # CLI must tell the user it's auto-disabling DONE-marker injection.
    result = _run("--dry-run", "loop", "--repeat", "1h", "do stuff")
    assert result.returncode == 0
    assert "--repeat" in result.stderr
    assert "--no-done" in result.stderr
    assert "DONE marker" in result.stderr


def test_dry_run_loop_repeat_with_done_path_no_warning() -> None:
    # Issue #61: --done <path> overrides the implicit --no-done; no warning.
    result = _run("--dry-run", "loop", "--repeat", "1h", "--done", "DONE.md", "do stuff")
    assert result.returncode == 0
    assert "implies `--no-done`" not in result.stderr


def test_dry_run_loop_repeat_with_explicit_no_done_no_warning() -> None:
    # Issue #61: explicit --no-done already opted out; don't badger the user.
    result = _run("--dry-run", "loop", "--repeat", "1h", "--no-done", "do stuff")
    assert result.returncode == 0
    assert "implies `--no-done`" not in result.stderr


def test_dry_run_loop_no_repeat_no_warning() -> None:
    # Issue #61: plain `clud loop` without --repeat must not emit the warning.
    result = _run("--dry-run", "loop", "do stuff")
    assert result.returncode == 0
    assert "implies `--no-done`" not in result.stderr


def test_dry_run_loop_repeat_with_done_override() -> None:
    result = _run("--dry-run", "loop", "--repeat", "1h", "--done", "DONE.md", "do stuff")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    prompt = data["command"][-1]
    assert "DONE.md" in prompt
    assert "BLOCKED.md" in prompt
    assert data["loop_markers"]["done_path"].replace("\\", "/").endswith("DONE.md")
    assert data["loop_markers"]["blocked_path"].replace("\\", "/").endswith("BLOCKED.md")


def test_dry_run_loop_repeat_invalid_duration_errors() -> None:
    # Issue #61: bogus duration values must fail with a clear error and a
    # non-zero exit code, not crash or silently succeed.
    result = _run("--dry-run", "loop", "--repeat", "30d", "do stuff")
    assert result.returncode != 0
    assert "invalid --repeat" in result.stderr or "unsupported" in result.stderr.lower()


def test_dry_run_loop_default_count() -> None:
    result = _run("--dry-run", "loop", "task")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert data["iterations"] == 50


def test_dry_run_passthrough_flags() -> None:
    result = _run("--dry-run", "--unknown-flag", "-p", "hello")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert "--unknown-flag" in data["command"]


def test_pipe_mode() -> None:
    result = _run("--dry-run", input_data="piped prompt")
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert "-p" in data["command"]
    assert "piped prompt" in data["command"]


def test_startup_refreshes_stale_managed_bundled_tool(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    home = tmp_path / "home"
    fake_bin = tmp_path / "bin"
    repo.mkdir()
    home.mkdir()
    _fake_claude_on_path(fake_bin)

    hook = home / ".clud" / "tools" / "hooks" / "block-bad-cmd.py"
    hook.parent.mkdir(parents=True)
    hook.write_text("# managed-by: clud\nprint('stale hook')\n", encoding="utf-8")

    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        env = copied_clud_env(source)
        env["HOME"] = str(home)
        env["USERPROFILE"] = str(home)
        env["CLUD_HOOK_HOME"] = str(home)
        env["PATH"] = str(fake_bin) + os.pathsep + env.get("PATH", "")
        result = process.run(
            [
                str(launch),
                "--no-daemon",
                "--no-fix-hooks",
                "--no-cpu-banner",
                "--no-dnd",
                "--subprocess",
                "-p",
                "hello",
            ],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )

    assert result.returncode == 0, (
        f"startup launch failed (rc={result.returncode}): "
        f"stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    updated = hook.read_text(encoding="utf-8")
    assert "clud-block-bad-cmd" in updated
    assert "compatibility shim" in updated.lower()
    assert "stale hook" not in updated


def test_dry_run_does_not_refresh_stale_managed_bundled_tool(tmp_path: Path) -> None:
    home = tmp_path / "home"
    home.mkdir()
    hook = home / ".clud" / "tools" / "hooks" / "block-bad-cmd.py"
    hook.parent.mkdir(parents=True)
    stale = "# managed-by: clud\nprint('stale hook')\n"
    hook.write_text(stale, encoding="utf-8")

    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = _copy_clud_for_test(temp_dir)
        env = copied_clud_env(source)
        env["HOME"] = str(home)
        env["USERPROFILE"] = str(home)
        env["CLUD_HOOK_HOME"] = str(home)
        result = process.run(
            [str(launch), "--dry-run", "-p", "hello"],
            env=env,
            capture_output=True,
            text=True,
            timeout=10,
        )

    assert result.returncode == 0, result.stderr
    assert hook.read_text(encoding="utf-8") == stale


def test_clean_worktrees_dry_run_smoke() -> None:
    """Issue #83: `clud --clean-worktrees --dry-run` must enumerate worktrees
    without crashing and without removing anything. We don't assert on the
    exact set of worktrees (the host running the test may have any number),
    only that the binary returns successfully and prints the dry-run banner.
    The binary itself lives inside a git repo (this one), so the worktree
    list is non-empty in practice — but even on a fresh clone with zero
    extra worktrees the command must succeed.
    """
    result = _run("--clean-worktrees", "--dry-run", "--yes")
    assert result.returncode == 0, (
        f"--clean-worktrees --dry-run failed (rc={result.returncode}): "
        f"stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    # Must mention worktrees in the output — sanity check that we ran the
    # worktrees code path and not e.g. the launcher path.
    combined = result.stdout + result.stderr
    assert "Worktrees" in combined or "worktree" in combined.lower()


def test_clean_worktrees_rejects_invalid_stale_after() -> None:
    """Bogus `--stale-after` values must fail with a clear error before any
    git invocation happens."""
    result = _run("--clean-worktrees", "--stale-after", "30x", "--dry-run")
    assert result.returncode != 0
    assert "invalid --stale-after" in result.stderr or "unsupported" in result.stderr.lower()


def test_fix_hooks_dry_run_plans_without_writing(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    home = tmp_path / "home"
    (repo / ".git").mkdir(parents=True)
    (repo / ".claude").mkdir()
    (repo / ".claude" / "settings.json").write_text(
        json.dumps(
            {
                "hooks": {
                    "PreToolUse": [
                        {
                            "matcher": "Bash",
                            "hooks": [{"type": "command", "command": "python check.py"}],
                        }
                    ]
                }
            }
        ),
        encoding="utf-8",
    )
    home.mkdir()

    with _copied_clud_tempdir() as temp_dir:
        source = Path(CLUD)
        launch = Path(temp_dir) / source.name
        shutil.copy2(source, launch)
        env = copied_clud_env(source)
        env["HOME"] = str(home)
        env["USERPROFILE"] = str(home)
        env["CLUD_HOOK_HOME"] = str(home)
        result = process.run(
            [str(launch), "--dry-run", "--fix-hooks"],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            timeout=10,
        )

    assert result.returncode == 0
    assert "hook health dry-run" in result.stdout
    assert "Claude PreToolUse hooks exist" in result.stdout
    assert "claude->codex" in result.stdout
    assert "matcher `Bash`" in result.stdout
    assert "add Codex project trust" not in result.stdout
    assert not (home / ".codex" / "config.toml").exists()


def test_video_dry_run_loads_plugin_only_for_that_session() -> None:
    """#1851: `clud video` names the video-use plugin wrapper; a plain launch does not."""
    result = _run("--dry-run", "video", env_overrides={"ELEVENLABS_API_KEY": "canary-1851"})
    assert result.returncode == 0, result.stderr
    command = json.loads(result.stdout)["command"]
    index = command.index("--plugin-dir")
    assert command[index + 1].replace("\\", "/").endswith(".clud/extern/video-use-plugin")
    assert command[-1].startswith("/video-use ")
    assert "canary-1851" not in result.stdout + result.stderr

    plain = _run("--dry-run", "-p", "hello")
    assert "--plugin-dir" not in json.loads(plain.stdout)["command"]


def test_openrouter_deepseek_effort_works_without_claude_effort_flag() -> None:
    result = _run(
        "--dry-run", "--openrouter", "--model", "deepseek/deepseek-v4.1-flash",
        "--effort", "low", "-p", "hello",
    )
    assert result.returncode == 0, result.stderr
    command = json.loads(result.stdout)["command"]
    assert "--effort" not in command
    assert command[command.index("--model") + 1] == "deepseek/deepseek-v4.1-flash"
    settings = json.loads(command[command.index("--settings") + 1])
    assert settings["effortLevel"] == "low"
