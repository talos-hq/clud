"""Unit tests for how `ci/run_bundle.py` runs the PTY harness (#691).

The PTY tests used to skip silently whenever the harness's stdout was a pipe,
which is how CI ran every harness, so Windows had no coverage of clud under a
real console. Each `pty::` test now runs inside a pseudo-terminal with
`CLUD_REQUIRE_PTY=1`, which turns a canary failure into a red test (which
tests: `tests/test_harness_plan.py`).
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from ci import run_bundle
from ci.run_bundle import REQUIRE_PTY_ENV, run_in_terminal


class FakeConPty:
    """A terminal that behaves as ConPTY does: exit is known, EOF never comes.

    Each scripted read is text to return or an exception to raise; a spent script
    times out. `wait()` is only legal when `poll()` has no exit code to give.
    """

    def __init__(self, reads: list[str | type[Exception]], code: int | None) -> None:
        self.reads = reads
        self.code = code
        self.closed = False

    def read_text(self, timeout: float | None = None) -> str:
        item = self.reads.pop(0) if self.reads else TimeoutError
        if isinstance(item, str):
            return item
        raise item

    def poll(self) -> int | None:
        return self.code

    def wait(self, timeout: float | None = None) -> int:
        assert self.code is None, "wait() drains for 2 s when the exit code is already known"
        return 5

    def close(self) -> None:
        self.closed = True


def _run_fake(monkeypatch: pytest.MonkeyPatch, fake: FakeConPty) -> int:
    monkeypatch.setattr(run_bundle, "PseudoTerminalProcess", lambda *_a, **_k: fake)
    return run_in_terminal(["harness"], {})


def test_output_arriving_after_exit_is_echoed_without_waiting_for_eof(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    fake = FakeConPty(["running\n", TimeoutError, "test result: ok\n"], code=7)
    assert _run_fake(monkeypatch, fake) == 7
    assert capsys.readouterr().out == "running\ntest result: ok\n"
    assert fake.closed


def test_eof_before_exit_still_waits_for_the_code(monkeypatch: pytest.MonkeyPatch) -> None:
    assert _run_fake(monkeypatch, FakeConPty([EOFError], code=None)) == 5


def test_an_interrupted_child_still_stops_the_run(monkeypatch: pytest.MonkeyPatch) -> None:
    code = next(iter(run_bundle.KEYBOARD_INTERRUPT_EXIT_CODES))
    fake = FakeConPty([TimeoutError], code=code)
    with pytest.raises(KeyboardInterrupt):
        _run_fake(monkeypatch, fake)
    assert fake.closed


def test_require_pty_env_matches_the_rust_harness() -> None:
    root = Path(__file__).resolve().parents[1]
    common = root / "crates/clud-bin/tests/integration/common/mod.rs"
    assert f'REQUIRE_PTY_ENV: &str = "{REQUIRE_PTY_ENV}"' in common.read_text(encoding="utf-8")


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX tty check")
def test_run_in_terminal_gives_the_child_a_tty_and_the_require_flag(
    capsys: pytest.CaptureFixture[str],
) -> None:
    script = (
        "import os, sys; "
        "print('tty', sys.stdin.isatty() and sys.stdout.isatty()); "
        f"print('flag', os.environ.get('{REQUIRE_PTY_ENV}')); "
        "sys.exit(3)"
    )
    rc = run_in_terminal([sys.executable, "-c", script], dict(os.environ))
    out = capsys.readouterr().out
    assert rc == 3
    assert "tty True" in out
    assert "flag 1" in out
