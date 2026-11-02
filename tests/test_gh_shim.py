"""Process regressions for the session-scoped gh alias (#1518)."""

from __future__ import annotations

import json
import os
import shutil
import sys
from pathlib import Path

import pytest

from tests import process
from tests.shim_env import session_env, session_key_names


def _binary(name: str) -> Path:
    suffix = ".exe" if sys.platform == "win32" else ""
    clud = os.environ.get("CLUD_TEST_BINARY")
    candidate = (
        Path(clud).with_name(name + suffix)
        if clud
        else Path(__file__).resolve().parents[1] / "target" / "debug" / (name + suffix)
    )
    assert candidate.is_file(), candidate
    return candidate


def _alias(tmp_path: Path, name: str = "gh") -> Path:
    """A session alias in its own directory, as `activate_rm` installs it.

    The real gh must live elsewhere: a target inside the alias directory is a
    stale session by contract (#1546) and passes through.
    """
    shim_dir = tmp_path / "shim"
    shim_dir.mkdir(exist_ok=True)
    alias = shim_dir / name
    shutil.copy2(_binary("clud-shim"), alias)
    return alias


def _session(alias: Path) -> dict[str, str]:
    return session_env(_binary("clud-shim"), alias.parent)


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX recording fixtures")
def test_pr_checks_watch_routes_to_bundled_watcher_and_preserves_exit(tmp_path: Path) -> None:
    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    shim = shim_dir / "gh"
    shutil.copy2(_binary("clud-shim"), shim)

    real_gh_log = tmp_path / "real-gh-args"
    real_gh = tmp_path / "real-gh"
    real_gh.write_text(
        '#!/bin/sh\nprintf "%s\\n" "$@" > "$REAL_GH_LOG"\nexit 93\n',
        encoding="utf-8",
    )
    real_gh.chmod(0o755)
    clud_log = tmp_path / "clud-args"
    fake_clud = tmp_path / "fake-clud"
    fake_clud.write_text(
        '#!/bin/sh\nprintf "%s\\n" "$@" > "$CLUD_LOG"\nexit 4\n',
        encoding="utf-8",
    )
    fake_clud.chmod(0o755)
    env = os.environ.copy()
    env.update(_session(shim))
    env.update(
        CLUD_GH_SHIM_TARGET=str(real_gh),
        CLUD_EXE=str(fake_clud),
        REAL_GH_LOG=str(real_gh_log),
        CLUD_LOG=str(clud_log),
        PATH=f"{shim_dir}{os.pathsep}{env['PATH']}",
    )

    result = process.run(
        [
            str(shim), "--repo", "zackees/clud", "pr", "checks", "123",
            "--watch", "--fail-fast", "--required", "-i", "7",
        ],
        env=env,
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=15,
    )

    assert result.returncode == 4, result
    assert not real_gh_log.exists()
    assert clud_log.read_text(encoding="utf-8").splitlines() == [
        "tool", "run", "github/pr_merge_watch.py", "123", "--cancel-on", "fail",
        "--repo", "zackees/clud", "--interval", "7",
    ]


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX recording fixtures")
def test_non_watch_gh_relays_arguments_and_exit(tmp_path: Path) -> None:
    alias = _alias(tmp_path)
    real_gh = tmp_path / "real-gh"
    real_gh.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\nexit 37\n', encoding="utf-8")
    real_gh.chmod(0o755)
    env = os.environ.copy() | _session(alias)
    env["CLUD_GH_SHIM_TARGET"] = str(real_gh)
    result = process.run(
        [str(alias), "api", "repos/zackees/clud", "--method", "GET"],
        env=env, cwd=tmp_path, capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == 37
    assert result.stdout.splitlines() == ["api", "repos/zackees/clud", "--method", "GET"]


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX recording fixtures")
@pytest.mark.parametrize(
    "selector", ["", "my-branch", "pr", "checks", "https://github.com/zackees/clud/pull/9"],
)
@pytest.mark.parametrize("repo", [["-R", "zackees/clud"], []], ids=["repo", "no-repo"])
def test_watch_hands_a_non_numeric_selector_to_the_watcher(
    tmp_path: Path, selector: str, repo: list[str]
) -> None:
    """Refs #1741: the shim does not look the PR up with a bare `gh pr view`,
    which in a fork resolves to the parent repository. The watcher resolves
    the selector against origin, with the one origin helper it already uses."""
    alias = _alias(tmp_path)
    lookup_log = tmp_path / "lookup-args"
    real_gh = tmp_path / "real-gh"
    real_gh.write_text(
        '#!/bin/sh\nprintf "%s\\n" "$@" > "$LOOKUP_LOG"\nprintf "42\\n"\n',
        encoding="utf-8",
    )
    real_gh.chmod(0o755)
    watcher_log = tmp_path / "watcher-args"
    fake_clud = tmp_path / "fake-clud"
    fake_clud.write_text(
        '#!/bin/sh\nprintf "%s\\n" "$@" > "$WATCHER_LOG"\nexit 5\n',
        encoding="utf-8",
    )
    fake_clud.chmod(0o755)
    env = os.environ.copy() | _session(alias)
    env.update(
        CLUD_GH_SHIM_TARGET=str(real_gh), CLUD_EXE=str(fake_clud),
        LOOKUP_LOG=str(lookup_log), WATCHER_LOG=str(watcher_log),
    )
    args = [str(alias), "pr", "checks"]
    if selector:
        args.append(selector)
    args.extend(["--watch", *repo])
    result = process.run(
        args, env=env, cwd=tmp_path, capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == 5, result
    assert not lookup_log.exists(), "the shim ran its own repo-less PR lookup"
    expected = ["tool", "run", "github/pr_merge_watch.py"]
    if selector:
        expected.append(selector)
    expected += ["--cancel-on", "fail"]
    if repo:
        expected += ["--repo", "zackees/clud"]
    assert watcher_log.read_text(encoding="utf-8").splitlines() == expected


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX recording fixtures")
@pytest.mark.parametrize("flag", ["--json", "--jq", "--template", "--web"])
def test_incompatible_watch_flags_fail_closed(tmp_path: Path, flag: str) -> None:
    alias = _alias(tmp_path)
    real_gh = tmp_path / "real-gh"
    real_gh.write_text("#!/bin/sh\nexit 93\n", encoding="utf-8")
    real_gh.chmod(0o755)
    env = os.environ.copy() | _session(alias)
    env.update(CLUD_GH_SHIM_TARGET=str(real_gh), CLUD_EXE=str(real_gh))
    result = process.run(
        [str(alias), "pr", "checks", "123", "--watch", flag],
        env=env, cwd=tmp_path, capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == 2, result
    assert "unsupported or ambiguous" in result.stderr


def _recording_gh(directory: Path) -> Path:
    directory.mkdir(exist_ok=True)
    real = directory / "gh"
    real.write_text('#!/bin/sh\nprintf "real %s\\n" "$*"\nexit 41\n', encoding="utf-8")
    real.chmod(0o755)
    return real


def _no_session(env: dict[str, str]) -> dict[str, str]:
    names = session_key_names(_binary("clud-shim"))
    return {key: value for key, value in env.items() if key not in names}


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX recording fixtures")
def test_missing_target_passes_through_to_the_next_real_gh(tmp_path: Path) -> None:
    """#1546: the alias outside a session is the real gh, never exit 127."""
    alias = _alias(tmp_path)
    real = _recording_gh(tmp_path / "real")
    env = _no_session(os.environ.copy())
    env["PATH"] = os.pathsep.join((str(alias.parent), str(real.parent)))
    result = process.run(
        [str(alias), "pr", "checks", "123", "--watch"],
        env=env, cwd=tmp_path, capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == 41, result
    assert result.stdout.strip() == "real pr checks 123 --watch"
    assert result.stderr == ""


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX recording fixtures")
def test_missing_target_without_a_real_gh_is_command_not_found(tmp_path: Path) -> None:
    alias = _alias(tmp_path)
    env = _no_session(os.environ.copy())
    env["PATH"] = str(alias.parent)
    result = process.run(
        [str(alias), "auth", "git-credential", "get"],
        env=env, cwd=tmp_path, capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == 127, result
    assert result.stderr.strip() == "gh: command not found"


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX executable modes")
def test_unexecutable_target_is_stale_and_passes_through(tmp_path: Path) -> None:
    alias = _alias(tmp_path)
    stale = tmp_path / "stale-gh"
    stale.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    stale.chmod(0o644)
    real = _recording_gh(tmp_path / "real")
    env = os.environ.copy() | _session(alias)
    env.update(
        CLUD_GH_SHIM_TARGET=str(stale),
        PATH=os.pathsep.join((str(alias.parent), str(real.parent))),
    )
    result = process.run(
        [str(alias), "pr", "view", "123"],
        env=env, cwd=tmp_path, capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == 41, result
    assert result.stdout.strip() == "real pr view 123"


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX shebang semantics")
def test_valid_target_that_cannot_exec_reports_exec_failure(tmp_path: Path) -> None:
    alias = _alias(tmp_path)
    broken = tmp_path / "broken-gh"
    broken.write_text("#!/nonexistent/interpreter\n", encoding="utf-8")
    broken.chmod(0o755)
    env = os.environ.copy() | _session(alias)
    env["CLUD_GH_SHIM_TARGET"] = str(broken)
    result = process.run(
        [str(alias), "pr", "view", "123"],
        env=env, cwd=tmp_path, capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == 126, result
    assert "failed to exec" in result.stderr


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX shell fixture")
def test_shim_reaches_real_bundled_watcher(tmp_path: Path) -> None:
    alias = _alias(tmp_path)
    real_gh = tmp_path / "real-gh"
    real_gh.write_text("#!/bin/sh\nexit 93\n", encoding="utf-8")
    real_gh.chmod(0o755)
    watcher = (
        Path(__file__).resolve().parents[1]
        / "crates/clud-bin/assets/tools/github/pr_merge_watch.py"
    )
    fake_clud = tmp_path / "fake-clud"
    fake_clud.write_text(
        '#!/bin/sh\n[ "$1" = tool ] && [ "$2" = run ] || exit 98\n'
        f'shift 3\nexec "{sys.executable}" "{watcher}" "$@"\n',
        encoding="utf-8",
    )
    fake_clud.chmod(0o755)
    env = os.environ.copy() | _session(alias)
    env.update(
        CLUD_GH_SHIM_TARGET=str(real_gh), CLUD_EXE=str(fake_clud),
        CLUD_PR_MERGE_WATCH_DRY_RUN="1",
    )
    result = process.run(
        [str(alias), "pr", "checks", "123", "--watch", "--repo", "zackees/clud"],
        env=env, cwd=tmp_path, capture_output=True, text=True, timeout=30,
    )
    assert result.returncode == 0, result
    assert "DRY-RUN pr=123 repo=zackees/clud" in result.stdout


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX shell fixture")
@pytest.mark.parametrize(
    ("scenario", "exit_code", "should_cancel"),
    # #1742: the upgrade is fail-fast only. A review exit (the FastLED/fbuild#1624
    # incident: an open CodeRabbit thread) must never cancel the PR's CI.
    [("fail", 1, True), ("review", 2, False), ("no_checks", 8, False)],
)
def test_shim_preserves_real_watcher_outcomes(
    tmp_path: Path, scenario: str, exit_code: int, should_cancel: bool
) -> None:
    alias = _alias(tmp_path)
    real_gh = tmp_path / "real-gh"
    real_gh.write_text("#!/bin/sh\nexit 93\n", encoding="utf-8")
    real_gh.chmod(0o755)
    harness = Path(__file__).resolve().parent / "fixtures/gh_watch_harness.py"
    fake_clud = tmp_path / "fake-clud"
    fake_clud.write_text(
        '#!/bin/sh\n[ "$1" = tool ] && [ "$2" = run ] || exit 98\n'
        f'shift 3\nexec "{sys.executable}" "{harness}" "$@"\n',
        encoding="utf-8",
    )
    fake_clud.chmod(0o755)
    cancel_log = tmp_path / "cancel.txt"
    env = os.environ.copy() | _session(alias)
    env.update(
        CLUD_GH_SHIM_TARGET=str(real_gh), CLUD_EXE=str(fake_clud),
        GH_WATCH_SCENARIO=scenario, GH_WATCH_CANCEL_LOG=str(cancel_log),
    )
    result = process.run(
        [str(alias), "pr", "checks", "123", "--watch", "--repo", "zackees/clud"],
        env=env, cwd=tmp_path, capture_output=True, text=True, timeout=30,
    )
    assert result.returncode == exit_code, result
    assert cancel_log.exists() is should_cancel
    if should_cancel:
        assert cancel_log.read_text(encoding="utf-8") == "123 abc123\n"


@pytest.mark.skipif(sys.platform != "win32", reason="Windows-native alias dispatch")
def test_windows_gh_alias_dispatches_watch_and_relay(tmp_path: Path) -> None:
    shim = _alias(tmp_path, "gh.exe")
    recorder = tmp_path / "real-gh.exe"
    shutil.copy2(Path(os.environ["CLUD_TEST_MOCK_AGENT_BINARY"]), recorder)
    recorded = tmp_path / "argv.json"
    env = os.environ.copy() | _session(shim)
    env.update(CLUD_GH_SHIM_TARGET=str(recorder), MOCK_RM_STUB_LOG=str(recorded))
    plain = process.run(
        [str(shim), "pr", "view", "123"], env=env, cwd=tmp_path,
        capture_output=True, text=True, timeout=30,
    )
    assert plain.returncode == 0, plain
    assert json.loads(recorded.read_text(encoding="utf-8")) == ["pr", "view", "123"]
    env["CLUD_EXE"] = str(recorder)
    watched = process.run(
        [str(shim), "pr", "checks", "123", "--watch"], env=env, cwd=tmp_path,
        capture_output=True, text=True, timeout=30,
    )
    assert watched.returncode == 0, watched
    assert json.loads(recorded.read_text(encoding="utf-8")) == [
        "tool", "run", "github/pr_merge_watch.py", "123", "--cancel-on", "fail",
    ]
