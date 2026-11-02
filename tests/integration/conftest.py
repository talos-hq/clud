"""Fixtures for integration tests with mock agents."""

from __future__ import annotations

import os
import platform
import shutil
import sys
from pathlib import Path

import pytest

from tests import process

ROOT = Path(__file__).resolve().parent.parent.parent

# Make ci.env importable so we can reuse its MSVC-forcing env on Windows.
sys.path.insert(0, str(ROOT))


def _cargo_argv(subcommand: list[str]) -> list[str]:
    """Return the cargo argv, pinning the MSVC toolchain on Windows.

    Windows rustc installations from chocolatey ship a GNU-host rustc which
    links C++ deps (whisper-rs) against MinGW runtime DLLs
    (libstdc++-6.dll, libgcc_s_seh-1.dll, libwinpthread-1.dll). Those DLLs
    aren't present on stock Windows, so the resulting debug binary fails
    with STATUS_ENTRYPOINT_NOT_FOUND when launched as a process.

    `ci.env.build_env()` selects the rustup-managed MSVC toolchain, which
    links against VCRUNTIME140.dll / MSVCP140.dll. The `soldr` branch below is
    only an emergency fallback if this file is run outside the repo.
    """
    if sys.platform == "win32":
        try:
            from ci.env import build_env, cargo_argv  # type: ignore[import-not-found]

            return cargo_argv(subcommand, env=build_env())
        except Exception:
            pass
        soldr = shutil.which("soldr")
        if soldr:
            return [soldr, "cargo", *subcommand]
    return ["cargo", *subcommand]


def _cargo_build_env() -> dict[str, str]:
    """Return the env used for building clud/mock-agent in tests.

    On Windows, reuse ci.env.build_env() which pins RUSTUP_TOOLCHAIN and
    CARGO_BUILD_TARGET to the MSVC variants, matching the wheel build.
    """
    if sys.platform != "win32":
        return os.environ.copy()
    try:
        from ci.env import build_env  # type: ignore[import-not-found]

        return build_env()
    except Exception:
        return os.environ.copy()


def _build_env_without_rustc_wrapper() -> dict[str, str]:
    """Cargo build env with an inherited compiler wrapper disabled.

    A persistent wrapper daemon can inherit subprocess stdio pipe handles and
    keep them open for its whole idle lifetime, so `capture_output=True` never
    sees EOF on Windows runners. The CI orchestrator already builds workspace
    binaries and passes their paths through the environment; this fallback is
    normally a local no-op incremental build. See #37.
    """
    env = _cargo_build_env()
    env.pop("RUSTC_WRAPPER", None)
    return env


# Re-export the canonical helpers from _subprocess_helpers so the unit-test
# file at the top of `tests/` can import the same implementation that the
# integration suite uses — issue #55. The helpers live in a regular module
# (rather than inline here) because pytest treats `conftest.py` as a special
# plugin file, which is awkward to import from arbitrary tests.
#
# pytest puts each `conftest.py`'s directory on `sys.path` automatically, so
# a top-level `import _subprocess_helpers` resolves to the file in this
# directory.
from _subprocess_helpers import (  # type: ignore[import-not-found]  # noqa: E402
    add_windows_create_no_window as _add_windows_create_no_window,
)
from _subprocess_helpers import (  # type: ignore[import-not-found]  # noqa: E402
    windows_no_window_flags,
)
from integration._daemon_helpers import (  # type: ignore[import-not-found]  # noqa: E402
    copy_launcher,
    stop_daemons_below,
)

__all__ = ["windows_no_window_flags"]


@pytest.fixture(scope="session", autouse=True)
def _suppress_windows_console_windows():
    """Hide console windows for integration-test subprocesses on Windows.

    The fixture is session-scoped so it applies to the build-time helpers as
    well as the actual tests. Issue #55: add CREATE_NO_WINDOW to ordinary
    ``process.run`` / ``process.Popen`` calls. Ctrl+Break tests that
    request ``CREATE_NEW_PROCESS_GROUP`` are left visible because hidden
    process groups do not reliably receive Ctrl+Break on Windows runners.
    """
    if sys.platform != "win32":
        yield
        return

    patch = pytest.MonkeyPatch()
    original_run = process.run
    original_popen = process.Popen

    def run(*args, **kwargs):
        kwargs = dict(kwargs)
        # Issue #55: hide ordinary child processes on Windows. Ctrl+Break
        # tests keep their process-group creation flags unchanged.
        _add_windows_create_no_window(kwargs)
        return original_run(*args, **kwargs)

    class Popen(original_popen):
        """Preserve `Popen` as a class for asyncio/Playwright consumers."""

        def __init__(self, *args, **kwargs):
            kwargs = dict(kwargs)
            # Issue #55: same fix as `run` above.
            _add_windows_create_no_window(kwargs)
            super().__init__(*args, **kwargs)

    patch.setattr(process, "run", run)
    patch.setattr(process, "Popen", Popen)
    try:
        yield
    finally:
        patch.undo()


def _target_search_dirs() -> list[Path]:
    """Directories where a built binary might land, ordered by preference."""
    ext = ".exe" if sys.platform == "win32" else ""
    dirs = []
    if sys.platform == "win32":
        dirs.extend(
            [
                ROOT / "target" / "x86_64-pc-windows-msvc" / "debug",
                ROOT / "target" / "aarch64-pc-windows-msvc" / "debug",
            ]
        )
    dirs.append(ROOT / "target" / "debug")
    return [d for d in dirs if (d / f"probe{ext}") or d.is_dir()]


def _find_built_binary(name: str) -> Path | None:
    ext = ".exe" if sys.platform == "win32" else ""
    for d in _target_search_dirs():
        candidate = d / f"{name}{ext}"
        if candidate.is_file():
            return candidate
    return None


def _cargo_build_inherit_stdio(package: str) -> None:
    """Build a workspace package, streaming cargo output to our own stdout
    and stderr rather than capturing via pipes.

    Capturing cargo's output through pipes on Windows GHA runners caused an
    indefinite hang (#37): `communicate()` waited for pipe EOF but a
    long-lived grand-child wrapper daemon kept the inheritable pipe handles
    open. Letting cargo's output go straight to the CI log sidesteps the issue
    entirely and still produces the artifacts at a deterministic path.
    """
    result = process.run(
        _cargo_argv(["build", "-p", package]),
        cwd=ROOT,
        env=_build_env_without_rustc_wrapper(),
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"cargo build -p {package} exited with code {result.returncode}"
        )


def _find_clud() -> Path:
    """Build the current repo's clud binary and return its path."""
    env_binary = os.environ.get("CLUD_TEST_BINARY")
    if env_binary:
        candidate = Path(env_binary)
        if candidate.is_file():
            return candidate

    _cargo_build_inherit_stdio("clud")
    binary = _find_built_binary("clud")
    if binary is None:
        raise RuntimeError("clud binary not found after build")
    return binary


def _build_mock_agent() -> Path:
    """Build the mock-agent binary and return its path."""
    env_binary = os.environ.get("CLUD_TEST_MOCK_AGENT_BINARY")
    if env_binary:
        candidate = Path(env_binary)
        if candidate.is_file():
            return candidate

    _cargo_build_inherit_stdio("mock-agent")
    binary = _find_built_binary("mock-agent")
    if binary is None:
        raise RuntimeError("mock-agent binary not found after build")
    return binary


@pytest.fixture(scope="session")
def mock_agent_binary() -> Path:
    """Build mock-agent once per test session."""
    return _build_mock_agent()


@pytest.fixture
def mock_env(mock_agent_binary: Path, tmp_path: Path) -> dict[str, str]:
    """Create a temp directory with mock claude/codex binaries on PATH.

    Returns an environment dict with PATH set so that `claude` and `codex`
    resolve to the mock-agent binary.
    """
    ext = ".exe" if platform.system() == "Windows" else ""

    # Copy mock-agent as both claude and codex
    claude_path = tmp_path / f"claude{ext}"
    codex_path = tmp_path / f"codex{ext}"
    copy_launcher(mock_agent_binary, claude_path)
    copy_launcher(mock_agent_binary, codex_path)

    if platform.system() != "Windows":
        claude_path.chmod(0o755)
        codex_path.chmod(0o755)

    # Build env with mock dir first on PATH
    env = os.environ.copy()
    env["PATH"] = str(tmp_path) + os.pathsep + env.get("PATH", "")
    # Isolate persisted preferences and credentials. The integration contract
    # assumes Claude is the default provider; inheriting a developer's
    # ~/.clud/settings.json can silently select Codex instead.
    home = tmp_path / "home"
    home.mkdir()
    env["HOME"] = str(home)
    # soldr#3193: clud probes `soldr shims --json` at startup; under a
    # throwaway HOME that would leave one soldr-broker process per test.
    env["SOLDR_BROKER_AUTOSPAWN"] = "0"
    env["USERPROFILE"] = str(home)
    env["LOCALAPPDATA"] = str(home / "local-app-data")
    env["XDG_STATE_HOME"] = str(home / ".local" / "state")
    env["XDG_CACHE_HOME"] = str(home / ".cache")
    env["CLUD_HOOK_HOME"] = str(home)
    # Mock-agent integration tests exercise the positive Codex-provider →
    # Claude-harness route. Supply a dummy API credential so launch admission
    # can proceed to the mock bridge; credential-absence behavior has its own
    # isolated Rust CLI regression that explicitly removes this variable.
    env["OPENAI_API_KEY"] = "test-openai-api-key"
    # The key is metered: the bridge reaches it only with both explicit
    # overrides (subscription-first policy). Never inherit a developer's.
    for name in ("CLUD_PROVIDER", "CLUD_SUBSCRIPTION_PROVIDER"):
        env.pop(name, None)
    env["CLUD_ALLOW_METERED"] = "1"
    env["CLUD_METERED_PROVIDER"] = "codex"
    # Prevent any VIRTUAL_ENV interference
    env.pop("VIRTUAL_ENV", None)

    # Enable PID logging for zombie detection
    pid_log = tmp_path / "child_pids.log"
    env["RUNNING_PROCESS_CHILD_PID_LOG_PATH"] = str(pid_log)

    # Skip the Windows exe-unlock dance (rename+copy+GC on every start).
    # The unlock path is under investigation in #37 as the suspected cause
    # of a pipe-EOF hang on Windows CI where process.run cannot flush
    # stdout/stderr of a `clud --version` process. Tests don't need the
    # hot-reload safety net anyway; the repo isn't pip-installing over a
    # running clud during tests.
    env["CLUD_NO_UNLOCK"] = "1"
    for name in (
        "CLUD_DAEMON_TEST_MAX_LIFETIME_SECS",
        "CLUD_DAEMON_TEST_IDLE_TIMEOUT_SECS",
        "CLUD_DAEMON_TEST_HOST_SCANS",
    ):
        env.pop(name, None)
    # Every daemon started by the integration suite is explicitly disposable.
    # The runtime profile disables host-wide scanners and provides a hard
    # lifetime if pytest is killed before fixture teardown can run.
    env["CLUD_DAEMON_TEST_MODE"] = "1"
    env["CLUD_DAEMON_STATE_DIR"] = str(tmp_path / "daemon-state")
    registry_dir = tmp_path / "registry"
    registry_dir.mkdir()
    env["CLUD_SESSION_DB"] = str(registry_dir / "sessions.redb")
    env["CLUD_SESSION_LOCK"] = str(registry_dir / "sessions.lock")

    return env


@pytest.fixture
def mock_env_codex_cmd(mock_agent_binary: Path, tmp_path: Path) -> dict[str, str]:
    """Windows-only env where `codex` resolves to a `.cmd` wrapper.

    Reproduces the npm-installed Codex layout: a `codex.cmd` launcher that
    shells out to a real executable with `%*`.
    """
    if sys.platform != "win32":
        pytest.skip("Windows-only codex.cmd wrapper fixture")

    mock_agent_name = f"mock-agent{'.exe' if sys.platform == 'win32' else ''}"
    mock_agent_path = tmp_path / mock_agent_name
    copy_launcher(mock_agent_binary, mock_agent_path)

    codex_wrapper = tmp_path / "codex.cmd"
    codex_wrapper.write_text(
        "@echo off\r\n"
        "\"%~dp0mock-agent.exe\" %*\r\n",
        encoding="utf-8",
    )

    env = os.environ.copy()
    env["PATH"] = str(tmp_path) + os.pathsep + env.get("PATH", "")
    env.pop("VIRTUAL_ENV", None)
    env["RUNNING_PROCESS_CHILD_PID_LOG_PATH"] = str(tmp_path / "child_pids.log")
    env["CLUD_NO_UNLOCK"] = "1"
    return env


@pytest.fixture
def mock_env_cmd_wrappers(mock_agent_binary: Path, tmp_path: Path) -> dict[str, str]:
    """Windows-only env where both backends resolve to `.cmd` wrappers."""
    if sys.platform != "win32":
        pytest.skip("Windows-only .cmd wrapper fixture")

    mock_agent_path = tmp_path / "mock-agent.exe"
    copy_launcher(mock_agent_binary, mock_agent_path)

    for backend in ("claude", "codex"):
        wrapper = tmp_path / f"{backend}.cmd"
        wrapper.write_text(
            "@echo off\r\n"
            "\"%~dp0mock-agent.exe\" %*\r\n",
            encoding="utf-8",
        )

    env = os.environ.copy()
    env["PATH"] = str(tmp_path) + os.pathsep + env.get("PATH", "")
    env.pop("VIRTUAL_ENV", None)
    env["RUNNING_PROCESS_CHILD_PID_LOG_PATH"] = str(tmp_path / "child_pids.log")
    env["CLUD_NO_UNLOCK"] = "1"
    return env


@pytest.fixture(scope="session")
def clud_binary() -> Path:
    """Return the path to the current repo's clud binary.

    Session-scoped so the build runs once per pytest session rather than
    once per test. Function scope caused every test (88 of them) to invoke
    `cargo build -p clud` for an incremental fingerprint check; on Windows
    that occasionally raced with a still-shutting-down clud.exe child from
    the previous test and failed with file-lock errors, cascading the rest
    of the suite into "cargo build -p clud exited with code 1" ERROR setups.
    On slow runners (Windows ARM) the cumulative cargo overhead also
    pushed the integration job past its 30-minute budget."""
    return _find_clud()


def _scan_for_clud_zombies() -> list[dict]:
    """Scan the system for orphaned CLUD-spawned processes."""
    ext = ".exe" if sys.platform == "win32" else ""
    candidates = [
        # MSVC-pinned builds put artifacts under target/<triple>/debug.
        ROOT
        / "target"
        / "x86_64-pc-windows-msvc"
        / "debug"
        / "examples"
        / f"scan_zombies{ext}",
        ROOT / "target" / "debug" / "examples" / f"scan_zombies{ext}",
    ]
    scan_bin = next((p for p in candidates if p.is_file()), None)
    if scan_bin is None:
        return []
    try:
        result = process.run(
            [str(scan_bin)],
            capture_output=True,
            text=True,
            timeout=10,
        )
        orphans = []
        for line in result.stdout.splitlines():
            if "ORPHAN" in line:
                orphans.append({"line": line.strip()})
        return orphans
    except Exception:
        return []


@pytest.fixture(scope="session")
def _daemon_cleanup_roots(clud_binary: Path):
    """Remember exact pytest roots for a final defense-in-depth sweep."""
    roots: list[Path] = []
    yield roots

    failures = []
    for root in roots:
        try:
            stop_daemons_below(root, clud_binary)
        except AssertionError as error:
            failures.append(f"{root}:\n{error}")
    if failures:
        pytest.fail(
            "daemon cleanup failed during the session-final sweep:\n"
            + "\n\n".join(failures)
        )


@pytest.fixture(autouse=True)
def _clean_daemons_and_check_no_zombies_after_test(
    tmp_path: Path,
    clud_binary: Path,
    _daemon_cleanup_roots: list[Path],
):
    """Stop daemons below this test's temp root, even when its body fails."""
    if tmp_path not in _daemon_cleanup_roots:
        _daemon_cleanup_roots.append(tmp_path)
    yield

    failures = []
    try:
        stop_daemons_below(tmp_path, clud_binary)
    except AssertionError as error:
        failures.append(f"daemon teardown failed:\n{error}")

    import time

    time.sleep(0.2)  # brief settle time for process cleanup
    orphans = _scan_for_clud_zombies()
    if orphans:
        details = "\n".join(f"  {orphan['line']}" for orphan in orphans)
        failures.append(f"CLUD zombie processes detected after test:\n{details}")
    if failures:
        pytest.fail("\n\n".join(failures))
