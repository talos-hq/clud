"""The suite is immune to `CLUD_*` session variables exported by a clud session (#1423)."""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from ci import env as ci_env
from tests import process

ROOT = Path(__file__).resolve().parents[1]

# What a clud session in clud's own repo exports into every child shell.
SESSION_VARS = {
    "CLUD_SKIP_RM_IDENTITY": "1",
    "CLUD_ROUTE_CONTEXT": '{"delegation":{"cost_policy":"prefer_the_cheapest"}}',
    "CLUD_RM_DRY_RUN": "1",
}


@pytest.mark.parametrize("name", sorted(SESSION_VARS))
def test_session_vars_are_scrubbed(name: str) -> None:
    assert ci_env.is_clud_session_var(name)
    env = {name: SESSION_VARS[name], "PATH": "/bin"}
    assert ci_env.scrub_clud_session_env(env) == [name]
    assert env == {"PATH": "/bin"}


@pytest.mark.parametrize(
    "name",
    [
        "CLUD_TEST_BINARY",
        "CLUD_CANDIDATE_ARTIFACT",
        "CLUD_PUBLIC_MODE",
        "CLUD_PUBLIC_RELEASE_TAG",
        "CLUD_PUBLIC_EVIDENCE",
        "CLUD_INSTALLER_CI_FIXTURE_DIR",
        "CLUD_TEST_BLOCK_BAD_CMD_BINARY",
        "CLUD_HARNESS_CLUD",
        "CLUD_INTEGRATION_TESTS",
        "CLUD_REAL_CLAUDE_TESTS",
        "CLUD_USE_SOLDR_SHIMS",
        "CLUD_NO_UNLOCK",
        "PATH",
    ],
)
def test_harness_config_survives(name: str) -> None:
    assert not ci_env.is_clud_session_var(name)


def test_session_vars_do_not_reach_tests() -> None:
    for name in SESSION_VARS:
        assert name not in os.environ, f"{name} leaked into the test process"


def test_rm_identity_tests_pass_under_exported_session_vars() -> None:
    """Re-run the tests #1423 broke with the session vars exported."""
    hook = os.environ.get("CLUD_TEST_BLOCK_BAD_CMD_BINARY")
    if not (hook and Path(hook).is_file()) and not (ROOT / "target/debug/clud-shim").is_file():
        pytest.skip("hook binaries not built")
    env = os.environ.copy() | SESSION_VARS
    result = process.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-p",
            "no:cacheprovider",
            "tests/test_rm_shim.py::test_hook_rewrite_still_applies_repository_command_policy",
            "tests/test_rm_shim.py::test_hook_rewrite_refuses_deletion_environment_changes",
        ],
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        timeout=300,
    )
    assert result.returncode == 0, result.stdout + result.stderr


# #1817: a clud session prepends its shim directories to PATH; its `rm` shim
# then answered the installers and shims under test.
SESSION_PATH_DIRS = [
    str(Path("home", ".clud", "state", "rm-shim")),
    str(Path("home", ".clud", "state", "shims")),
    str(Path("home", ".clud", "state", "versions", "v1-abc", "rm-shim")),
    str(Path("home", ".clud", "state", "versions", "v1-abc", "shims")),
]


def test_session_shim_dirs_are_scrubbed_from_path() -> None:
    keep = str(Path("usr", "bin"))
    env = {"PATH": os.pathsep.join([*SESSION_PATH_DIRS, keep])}
    assert ci_env.scrub_clud_session_path(env) == SESSION_PATH_DIRS
    assert env == {"PATH": keep}
    assert not ci_env.is_clud_session_path_dir(str(Path("home", ".clud", "bin")))


def test_session_shim_dirs_do_not_reach_tests() -> None:
    for entry in os.environ.get("PATH", "").split(os.pathsep):
        assert not ci_env.is_clud_session_path_dir(entry), f"{entry} leaked onto PATH"


def test_suite_runs_without_an_inherited_rm_shim(tmp_path: Path) -> None:
    """Re-run the PATH check with a session's shim directories exported."""
    shim_dir = tmp_path / ".clud" / "state" / "rm-shim"
    shim_dir.mkdir(parents=True)
    env = os.environ.copy()
    env["PATH"] = os.pathsep.join([str(shim_dir), env.get("PATH", "")])
    result = process.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-p",
            "no:cacheprovider",
            "tests/test_clud_session_env_isolation.py::test_session_shim_dirs_do_not_reach_tests",
        ],
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        timeout=300,
    )
    assert result.returncode == 0, result.stdout + result.stderr
