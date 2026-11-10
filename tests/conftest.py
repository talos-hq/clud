"""Shared pytest configuration for clud tests."""

from __future__ import annotations

import importlib.util
import os
import re
import sys
from pathlib import Path

import pytest

from ci import aliases, tracked_files
from ci.env import scrub_clud_session_env, scrub_clud_session_path
from ci.pytest_tmp_retention import (  # noqa: F401  (pytest hooks, #1686)
    pytest_sessionfinish,
    pytest_terminal_summary,
)

# A clud session exports state such as CLUD_SKIP_RM_IDENTITY (auto-on in
# clud's own repo) and CLUD_ROUTE_CONTEXT. Inherited by the hook binaries
# under test, they flip the behavior those tests assert (#1423). Tests set
# only the CLUD_* variables they need; everything else starts clean.
scrub_clud_session_env(os.environ)
# Its shim directories on PATH would answer `rm`/`python` for the installers
# and shims under test (#1817).
scrub_clud_session_path(os.environ)
# Test children may only use fake secret acquisition, never the user's desktop.
os.environ["CLUD_TEST_NO_SECRET_UI"] = "1"
for _display_variable in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"):
    os.environ.pop(_display_variable, None)

_REPO_ROOT = Path(__file__).resolve().parents[1]


def pytest_sessionstart(session: pytest.Session) -> None:
    """Link `clud`'s argv[0] aliases beside the `clud` under test (#1551).

    `clud-cmd-scan`, `clud-block-bad-cmd` and `clud-shim` are names of the one
    `clud` binary, not build outputs, and the suites resolve them as siblings
    of `clud`. Linking here covers direct `pytest` runs; CI harnesses link
    them themselves before the run. Missing or read-only builds are skipped,
    and so is the exact installer candidate artifact (#1718).
    """
    suffix = ".exe" if sys.platform == "win32" else ""
    value = os.environ.get("CLUD_TEST_BINARY")
    clud = Path(value) if value else _REPO_ROOT / "target" / "debug" / f"clud{suffix}"
    if clud.is_file() and aliases.links_allowed_beside(clud, os.environ):
        aliases.materialize(clud)


@pytest.fixture(scope="session", autouse=True)
def checkout_tracked_files_unchanged():
    """Fail the run if any test wrote a tracked file in the checkout (#1426).

    Compares before vs after, so a checkout that was already dirty passes.
    Skipped when the tests do not run from a git checkout.
    """
    before = tracked_files.snapshot(_REPO_ROOT)
    yield
    if before is None:
        return
    after = tracked_files.snapshot(_REPO_ROOT)
    if after is None:
        return
    changed = tracked_files.changed_paths(before, after)
    if changed:
        pytest.fail(tracked_files.describe(changed, during="the pytest run"), pytrace=False)


# #2356: a module that refers to one of these runs a built binary, so its tests
# are Tier 1 (`daemon`) and stay out of the host-only `bash test --host` lane.
_BINARY_REFERENCES = (
    "CLUD_TEST_BINARY",
    "CLUD_TEST_BLOCK_BAD_CMD_BINARY",
    "CLUD_TEST_MOCK_AGENT_BINARY",
    "target/debug",
    '"target", "debug"',
    # Explicit opt-in for a module whose binary use the scan cannot see,
    # such as a tool under test that spawns clud itself.
    "pytestmark = pytest.mark.daemon",
)
_module_runs_binary: dict[Path, bool] = {}
# `from test_hello import CLUD`, `from tests.test_x import y`, `import _helpers`.
_LOCAL_IMPORT = re.compile(r"^\s*(?:from|import)\s+(?:tests\.)?(_?[A-Za-z0-9_]+)", re.MULTILINE)
_TESTS_ROOT = Path(__file__).resolve().parent


def _runs_built_binary(path: Path) -> bool:
    """Does this module, or a sibling test/helper module it imports, run a binary?"""
    path = path.resolve()
    if path in _module_runs_binary:
        return _module_runs_binary[path]
    _module_runs_binary[path] = False  # breaks import cycles
    try:
        source = path.read_text(encoding="utf-8")
    except OSError:
        source = ""
    runs = any(ref in source for ref in _BINARY_REFERENCES) or any(
        _runs_built_binary(sibling)
        for name in _LOCAL_IMPORT.findall(source)
        if name != "conftest"
        for sibling in (_TESTS_ROOT / f"{name}.py", path.parent / f"{name}.py")
        if sibling.is_file() and sibling.resolve() != path
    )
    _module_runs_binary[path] = runs
    return runs


def pytest_ignore_collect(collection_path: Path, config: pytest.Config) -> bool | None:
    """Tier 0 never imports a `daemon` module (#2356).

    Some resolve their binary at import time, which would fail collection
    before the marker could deselect them.
    """
    if os.environ.get("CLUD_TEST_TIER") != "0" or collection_path.suffix != ".py":
        return None
    return True if _runs_built_binary(collection_path) else None


def mark_binary_tests_daemon(items: list[pytest.Item]) -> None:
    """Mark every test in a module that runs a built binary as `daemon`."""
    for item in items:
        if _runs_built_binary(Path(str(item.path))):
            item.add_marker(pytest.mark.daemon)


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    """Classify Tier 1 tests; skip integration tests unless CLUD_INTEGRATION_TESTS=1."""
    mark_binary_tests_daemon(items)
    if os.environ.get("CLUD_INTEGRATION_TESTS", "").lower() in {"1", "true", "yes"}:
        return
    skip_integration = pytest.mark.skip(reason="set CLUD_INTEGRATION_TESTS=1 to run")
    for item in items:
        if "integration" in item.keywords:
            item.add_marker(skip_integration)


@pytest.fixture
def bridge(monkeypatch):
    """Load `src/clud/mcp_server.py` as an importable module.

    Shared here (not in the test module) so both the original bridge tests and
    the session-surface tests drive the same live module.
    """
    name = "clud_test_mcp_server"
    script = Path(__file__).resolve().parents[1] / "src" / "clud" / "mcp_server.py"
    # Exec lanes test sources without installing the project wheel. Resolve
    # sibling package modules from that same source tree.
    monkeypatch.syspath_prepend(str(script.parents[1]))
    spec = importlib.util.spec_from_file_location(name, script)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    try:
        yield module
    finally:
        sys.modules.pop(name, None)


@pytest.fixture(autouse=True)
def session_rm_path(request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch):
    """Hook process tests run in the same trusted-rm PATH contract as sessions.

    Identity-negative tests supply their own explicit PATH. Keep this scoped to
    hook consumers so unrelated subprocess tests retain their original env.
    """
    modules = {
        "test_hook_stdin.py",
        "test_block_cd_hook.py",
        "test_cwd_changed_hook.py",
        "test_extern_hook_trust.py",
        "test_uv_run_hook_guard.py",
        "test_clud_hooks_dispatch.py",
        "test_gated_session.py",
    }
    if request.path.name not in modules:
        return
    import shutil

    tmp_path = request.getfixturevalue("tmp_path")
    suffix = ".exe" if sys.platform == "win32" else ""
    clud = os.environ.get("CLUD_TEST_BINARY")
    hook = os.environ.get("CLUD_TEST_BLOCK_BAD_CMD_BINARY")
    sibling = (
        Path(clud or hook).with_name("clud-shim" + suffix)
        if clud or hook
        else Path(__file__).resolve().parents[1] / "target/debug" / ("clud-shim" + suffix)
    )
    assert sibling.is_file(), f"build packaged shim for hook tests: {sibling}"
    directory = tmp_path / "session-rm-shim"
    directory.mkdir()
    shutil.copy2(sibling, directory / ("rm" + suffix))
    tap = sibling.with_name("tap" + suffix)
    if tap.is_file():
        shutil.copyfile(tap, directory / ("tap" + suffix))
        (directory / ("tap" + suffix)).chmod(0o755)
    monkeypatch.setenv("PATH", str(directory) + os.pathsep + os.environ.get("PATH", ""))
