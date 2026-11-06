"""Execute the installer's printed commands without inheriting an activated PATH."""
from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from tests.process import run as run_process
from tests.test_installer_native_entry import (
    clud_binary,
    installer_target_scope,
    isolated_env,
    required_shell,
)


def install_commands(env: dict[str, str]) -> dict[str, str]:
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes", "--no-modify-path"],
        env=env, capture_output=True, timeout=30, check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    assert b"activation required" in result.stderr
    labels = {"Bash/zsh", "fish", "PowerShell", "Git Bash",
              "Command Prompt (new shell in this terminal)"}
    commands = {}
    for line in result.stderr.decode("utf-8").splitlines():
        label, separator, command = line.partition(": ")
        if separator and label in labels:
            commands[label] = command
    assert commands, result.stderr.decode(errors="replace")
    return commands


@pytest.fixture(scope="module")
def windows_install(tmp_path_factory: pytest.TempPathFactory):
    """Install once for every Windows shell test; none of them changes the tree.

    Each shell still activates in its own process from the same printed output,
    which is the behaviour under test, and the install was most of the cost.
    """
    for env, destination in installer_target_scope(tmp_path_factory.mktemp("install")):
        yield env, destination, install_commands(env)


@pytest.fixture(scope="module")
def clud_version() -> bytes:
    return run_process([str(clud_binary()), "--version"], capture_output=True, check=True).stdout


@pytest.mark.skipif(sys.platform == "win32", reason="requires POSIX shells")
@pytest.mark.parametrize(
    ("shell", "flags", "label"),
    [("bash", ["--noprofile", "--norc"], "Bash/zsh"),
     ("zsh", ["-f"], "Bash/zsh"), ("fish", ["--no-config"], "fish")],
)
def test_printed_posix_activation_command(tmp_path: Path, shell: str, flags: list[str], label: str):
    executable = required_shell(shell)
    home = tmp_path / "home with spaces 'quotes' $cash !bang 雪"
    home.mkdir(mode=0o700)
    env = isolated_env(home)
    env["SHELL"] = executable
    env["PATH"] = "/usr/bin:/bin"
    commands = install_commands(env)
    result = run_process(
        [executable, *flags, "-c", commands[label] + "; command -v clud; clud --version"],
        env=env, capture_output=True, timeout=15, check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    version = run_process([str(clud_binary()), "--version"], capture_output=True, check=True).stdout
    assert result.stdout.splitlines() == [os.fsencode(home / ".local/bin/clud"), version.strip()]


@pytest.mark.skipif(sys.platform != "win32", reason="requires Windows PowerShell")
@pytest.mark.parametrize("shell", ["powershell", "pwsh"])
def test_printed_powershell_activation_command(windows_install, clud_version: bytes, shell: str):
    env, destination, commands = windows_install
    command = (
        "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); "
        + commands["PowerShell"] + "; (Get-Command clud).Source; clud --version"
    )
    result = run_process(
        [required_shell(shell), "-NoLogo", "-NoProfile", "-Command", command],
        env=env, capture_output=True, timeout=30, check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    lines = result.stdout.decode("utf-8").splitlines()
    assert Path(lines[0]).resolve() == destination.resolve()
    assert lines[1] == clud_version.decode().strip()


@pytest.mark.skipif(sys.platform != "win32", reason="requires Windows Command Prompt")
def test_printed_cmd_activation_opens_activated_child(windows_install, clud_version: bytes):
    env, destination, commands = windows_install
    command = commands["Command Prompt (new shell in this terminal)"]
    result = run_process(
        [required_shell("cmd"), "/D", "/V:ON", "/C", command],
        input=b"where clud\r\nclud --version\r\nexit\r\n",
        env=env, capture_output=True, timeout=30, check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    lines = result.stdout.decode("utf-8", errors="replace").splitlines()
    paths = [line.strip() for line in lines if line.strip().lower().endswith("\\clud.exe")]
    assert paths, result.stdout.decode(errors="replace")
    assert Path(paths[0]).resolve() == destination.resolve()
    assert clud_version.decode().strip() in lines


@pytest.mark.skipif(sys.platform != "win32", reason="requires Git for Windows")
def test_printed_git_bash_activation_command(windows_install, clud_version: bytes):
    from tests.installer_windows_shells import git_bash_executable

    env, destination, commands = windows_install
    bash = git_bash_executable(Path(required_shell("git")))
    command = commands["Git Bash"] + '; cygpath -w "$(command -v clud)"; clud --version'
    result = run_process(
        [str(bash), "--noprofile", "--norc", "-c", command],
        env=env, capture_output=True, timeout=30, check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    lines = result.stdout.decode("utf-8").splitlines()
    assert Path(lines[0]).resolve() == destination.resolve()
    assert lines[1] == clud_version.decode().strip()
