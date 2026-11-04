"""Activation repair requires separate consent and permits at most one attempt."""
from __future__ import annotations

import errno
import json
import os
import re
import select
import sys
import time
from contextlib import contextmanager
from pathlib import Path

import pytest

from tests.process import run as run_process
from tests.test_installer_native_entry import (
    clud_binary,
    installer_json,
    isolated_env,
    required_shell,
)

if sys.platform != "win32":
    import pty

pytestmark = pytest.mark.skipif(sys.platform == "win32", reason="requires POSIX startup files")


class InstallerTerminal:
    def __init__(self, pid: int, master: int):
        self.pid = pid
        self.master = master
        self.output = bytearray()
        self.status: int | None = None

    def read(self) -> None:
        readable, _, _ = select.select([self.master], [], [], 0.05)
        if readable:
            try:
                self.output.extend(os.read(self.master, 8192))
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
        if self.status is None:
            waited, status = os.waitpid(self.pid, os.WNOHANG)
            if waited:
                self.status = status

    def expect(self, marker: bytes) -> None:
        deadline = time.monotonic() + 30
        while marker not in self.output and self.status is None and time.monotonic() < deadline:
            self.read()
        assert marker in self.output, self.output.decode(errors="replace")

    def finish(self) -> int:
        deadline = time.monotonic() + 30
        while self.status is None and time.monotonic() < deadline:
            self.read()
        assert self.status is not None, self.output.decode(errors="replace")
        return os.waitstatus_to_exitcode(self.status)


@contextmanager
def installer_terminal(env: dict[str, str], flags: tuple[str, ...] = ()):
    binary = str(clud_binary())
    pid, master = pty.fork()
    if pid == 0:
        os.execve(binary, [binary, "--installer", "--install-current", *flags], env)
    terminal = InstallerTerminal(pid, master)
    try:
        yield terminal
    finally:
        if terminal.status is None:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
        os.close(master)


def setup_home(tmp_path: Path) -> tuple[Path, dict[str, str]]:
    home = tmp_path / "home"
    home.mkdir(mode=0o700)
    env = isolated_env(home)
    env["SHELL"] = required_shell("bash")
    env["PATH"] = "/usr/bin:/bin"
    return home, env


@pytest.mark.parametrize("decision", ["approve", "decline", "conflict_again"])
@pytest.mark.parametrize("structured", [False, True])
def test_activation_repair_requires_separate_bounded_consent(
    tmp_path: Path, decision: str, structured: bool,
) -> None:
    home, env = setup_home(tmp_path)
    profile = home / ".bashrc"
    profile.write_text("# original\n")
    profile.chmod(0o600)
    changed = "# changed after initial plan\n"
    with installer_terminal(env, ("--installer-json",) if structured else ()) as terminal:
        terminal.expect(b"Install clud")
        profile.write_text(changed)
        os.write(terminal.master, b"\x1b[A\r")
        terminal.expect(b"Approve this additional activation repair?")
        if decision == "conflict_again":
            changed = "# changed after repair plan\n"
            profile.write_text(changed)
        os.write(terminal.master, b"\r" if decision == "decline" else b"\x1b[A\r")
        expected = 0 if decision == "approve" else 1
        if decision == "approve" and not structured:
            # A verified interactive install continues into the provider
            # setup `clud setup` runs (#2219). Down+Enter selects Claude.
            # Before #2605's follow-up, this read failed with EIO because
            # the interactive-shell probes kept the terminal's foreground.
            terminal.expect(b"Setup for:")
            os.write(terminal.master, b"\x1b[B\r")
        assert terminal.finish() == expected, terminal.output.decode(errors="replace")
        if decision == "approve" and not structured:
            assert b"provider: default set to claude" in terminal.output
        assert terminal.output.count(b"Additional activation repair plan:") == 1
        if structured:
            report = terminal_report(terminal)
            assert report["status"] == {
                "approve": "complete", "decline": "refused", "conflict_again": "failed",
            }[decision]
            assert report["exit_code"] == expected
            assert report["binary_committed"] is True
            assert report["activation_verified"] is (decision == "approve")
    assert (home / ".local/bin/clud").is_file()
    if decision == "approve":
        assert profile.read_text().startswith(changed)
        assert profile.read_text().count("# >>> clud installer PATH >>>") == 1
        assert b"verified fresh name-based lookup" in terminal.output
    else:
        assert profile.read_text() == changed
        assert b"verified fresh name-based lookup" not in terminal.output


@pytest.mark.parametrize("structured", [False, True])
def test_unattended_activation_failure_never_repairs(tmp_path: Path, structured: bool) -> None:
    home, env = setup_home(tmp_path)
    original = "clud() { printf '%s\\n' shadowed; }\n"
    profiles = [home / ".bash_profile", home / ".bashrc"]
    for profile in profiles:
        profile.write_text(original)
        profile.chmod(0o600)
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes",
         *(["--installer-json"] if structured else [])],
        env=env, capture_output=True, timeout=30, check=False,
    )
    assert result.returncode == 1, result.stderr.decode(errors="replace")
    assert b"activation failed" in result.stderr
    assert b"Additional activation repair plan:" not in result.stderr
    if structured:
        report = installer_json(result, "failed", 1, True)
        assert report["activation_verified"] is False
        assert report["reason"] == "activation_failed"
    for profile in profiles:
        assert profile.read_text() == original


def terminal_report(terminal: InstallerTerminal) -> dict:
    transcript = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", terminal.output)
    reports = [
        json.loads(line) for line in transcript.splitlines() if line.startswith(b'{"schema"')
    ]
    assert len(reports) == 1, terminal.output.decode(errors="replace")
    assert reports[0]["schema"] == "clud-installer-result/v1"
    return reports[0]


def test_installer_json_cancellation_precedes_commit(tmp_path: Path) -> None:
    home, env = setup_home(tmp_path)
    with installer_terminal(env, ("--installer-json",)) as terminal:
        terminal.expect(b"Install clud")
        os.write(terminal.master, b"\x03")
        assert terminal.finish() == 130, terminal.output.decode(errors="replace")
        report = terminal_report(terminal)
    assert report["status"] == "refused"
    assert report["exit_code"] == 130
    assert report["reason"] == "cancelled"
    assert report["binary_committed"] is False
    assert report["activation_verified"] is False
    assert not (home / ".local/bin/clud").exists()
