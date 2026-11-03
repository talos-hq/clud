"""Process coverage for the child deletion shim (#1461)."""

from __future__ import annotations

import json
import os
import shlex
import shutil
import sys
import time
from pathlib import Path

import psutil
import pytest

from tests import process
from tests.shim_env import session_env


def binary(name: str) -> Path:
    suffix = ".exe" if sys.platform == "win32" else ""
    value = os.environ.get("CLUD_TEST_BINARY")
    candidate = (
        Path(value).with_name(name + suffix)
        if value
        else Path(__file__).resolve().parents[1] / "target/debug" / (name + suffix)
    )
    assert candidate.is_file(), candidate
    return candidate


@pytest.fixture
def world(tmp_path: Path) -> tuple[Path, Path, dict[str, str], Path]:
    if sys.platform == "win32":
        pytest.skip("POSIX fixture")
    shim_dir = tmp_path / "shim"
    stub_dir = tmp_path / "stub"
    home = tmp_path / "home"
    for directory in (shim_dir, stub_dir, home):
        directory.mkdir()
    name = "r" + "m"
    shim = shim_dir / name
    shutil.copy2(binary("clud-shim"), shim)
    log = tmp_path / "handoff.json"
    stub = stub_dir / name
    stub.write_text(
        # Debian's system interpreter is outside clud's shim. python-name-lint: allow-next-line
        "#!/usr/bin/python3\nimport json, os, sys\n"
        'open(os.environ["RM_STUB_LOG"], "w").write(json.dumps(sys.argv[1:]))\n',
        encoding="utf-8",
    )
    stub.chmod(0o755)
    env = os.environ.copy()
    env.update(
        HOME=str(home),
        USERPROFILE=str(home),
        PATH=os.pathsep.join((str(shim_dir), str(stub_dir), "/usr/bin", "/bin")),
        RM_STUB_LOG=str(log),
        CLUD_RM_ROOTS=str(tmp_path / "nonexistent-root"),
    )
    # The floor is session-only (#1546): these cases run inside a session.
    env.update(session_env(binary("clud-shim"), shim_dir))
    return shim, home, env, log


def run(shim: Path, env: dict[str, str], *args: str, cwd: Path | None = None):
    return process.run(
        [str(shim), *args],
        cwd=str(cwd or shim.parent),
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_unsafe_session_hands_catastrophic_rm_to_the_real_binary(world) -> None:
    shim, home, env, log = world
    protected = run(shim, env, "-rf", str(home))
    assert protected.returncode == 2, protected
    assert not log.exists()
    env["CLUD_UNSAFE_MODE"] = "1"
    handed_off = run(shim, env, "-rf", str(home))
    assert handed_off.returncode == 0, handed_off
    assert json.loads(log.read_text(encoding="utf-8")) == ["-rf", str(home)]


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX backend fixture")
def test_new_launch_does_not_inherit_unsafe_from_parent(tmp_path: Path) -> None:
    home = tmp_path / "home"
    home.mkdir()
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    codex = bin_dir / "codex"
    codex.write_text(
        # A JSON-mode Codex returns its final answer as an agent_message event.
        "#!/bin/sh\n"
        'printf \'{"type":"item.completed","item":{"id":"probe",'
        '"type":"agent_message","text":"unsafe=%s"}}\\n\' '
        '"${CLUD_UNSAFE_MODE:-unset}"\n'
        'printf \'{"type":"turn.completed","usage":{}}\\n\'\n',
        encoding="utf-8",
    )
    codex.chmod(0o755)
    env = os.environ.copy()
    env.update(
        HOME=str(home),
        USERPROFILE=str(home),
        PATH=os.pathsep.join((str(bin_dir), "/usr/bin", "/bin")),
        CLUD_UNSAFE_MODE="1",
    )
    for flag, expected in (([], "unsafe=unset"), (["--unsafe"], "unsafe=1")):
        result = process.run(
            [str(binary("clud")), "--codex", "--no-daemon", *flag, "-p", "probe"],
            cwd=str(tmp_path),
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, result
        assert expected in result.stdout


def test_hook_rewrite_still_applies_repository_command_policy(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    settings = repo / ".clud"
    settings.mkdir(parents=True)
    (repo / ".git").mkdir()
    (settings / "settings.json").write_text(
        json.dumps(
            {
                "bad_commands": [
                    {
                        "id": "deny-safe-deletion",
                        "match": "safe-rm",
                        "replacement": "echo deletion-is-forbidden",
                        "reason": "repository forbids deletion",
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    home = tmp_path / "home"
    home.mkdir()
    env = os.environ.copy()
    env.update(HOME=str(home), USERPROFILE=str(home), CLUD_SKIP_RM_IDENTITY="1")
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "r" + "m -rf build"},
            "cwd": str(repo),
        }
    )
    result = process.run(
        [str(binary("clud-cmd-scan"))],
        input=payload,
        cwd=str(repo),
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 2, result
    output = json.loads(result.stdout)["hookSpecificOutput"]
    assert output["permissionDecision"] == "deny"
    assert "deny-safe-deletion" in output["permissionDecisionReason"]


@pytest.mark.parametrize(
    "prefix",
    [
        "PATH=/usr/bin",
        "CLUD_RM_ROOTS=/",
        "CLUD_RM_ROLE=user",
        "CLUD_UNSAFE_MODE=1",
        "export PATH=/usr/bin;",
    ],
)
def test_hook_rewrite_refuses_deletion_environment_changes(tmp_path: Path, prefix: str) -> None:
    home = tmp_path / "home"
    home.mkdir()
    env = os.environ.copy()
    env.update(HOME=str(home), USERPROFILE=str(home), CLUD_SKIP_RM_IDENTITY="1")
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": prefix + " " + "r" + "m -rf build"},
            "cwd": str(tmp_path),
        }
    )
    result = process.run(
        [str(binary("clud-cmd-scan"))],
        input=payload,
        cwd=str(tmp_path),
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 2, result
    output = json.loads(result.stdout)["hookSpecificOutput"]
    assert output["permissionDecision"] == "deny"
    assert "PATH" in output["permissionDecisionReason"]


@pytest.mark.parametrize(
    "command",
    [
        'grep -n "r' + 'm |mktemp" "$S/install.sh"',
        "cat > \"{scratch}/x.py\" <<'PY'\nr" + "m -f\nPY",
        'for n in a; do printf x > "{scratch}/a"; done',
    ],
)
def test_hook_leaves_investigation_false_positives_unchanged(tmp_path: Path, command: str) -> None:
    # #1999 refuses unresolved write destinations; this test isolates deletion identity.
    command = command.replace("{scratch}", tmp_path.as_posix())
    home = tmp_path / "home"
    home.mkdir()
    env = os.environ.copy()
    env.update(HOME=str(home), USERPROFILE=str(home), CLUD_SKIP_RM_IDENTITY="1")
    payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": str(tmp_path),
        }
    )
    result = process.run(
        [str(binary("clud-cmd-scan"))],
        input=payload,
        cwd=str(tmp_path),
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result
    assert result.stdout.strip() == "", result


@pytest.mark.parametrize("nounset_opt_out", [False, True], ids=["default", "opt-out"])
def test_launcher_restores_shim_path_in_codex_login_shell(
    tmp_path: Path, nounset_opt_out: bool
) -> None:
    if sys.platform == "win32":
        pytest.skip("Bash login-shell fixture")
    home = tmp_path / "home"
    home.mkdir()
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    codex = bin_dir / "codex"
    bash = shutil.which("bash")
    assert bash is not None, "Bash is required for the login-shell probe"
    encoder = (
        "import json,sys;"
        "print(json.dumps({'type':'item.completed','item':{'id':'probe',"
        "'type':'agent_message','text':sys.stdin.read()}}));"
        "print(json.dumps({'type':'turn.completed','usage':{}}))"
    )
    codex.write_text(
        "#!/bin/sh\nprobe=$("
        + shlex.quote(bash)
        + ' -lc \'printf "%s|%s|%s\\n" "${CLUD_RM_SHIM_DIR:-}" '
        '"$(command -v safe-rm)" "${UNSET_FOR_TEST:-}"\') || exit $?\n'
        'printf "%s\\n" "$probe" | '
        + shlex.quote(sys.executable)
        + " -c "
        + shlex.quote(encoder)
        + "\n",
        encoding="utf-8",
    )
    codex.chmod(0o755)
    env = os.environ.copy()
    env.update(HOME=str(home), PATH=os.pathsep.join((str(bin_dir), "/usr/bin", "/bin")))
    if nounset_opt_out:
        env["CLUD_NO_BASH_NOUNSET"] = "1"
    try:
        result = process.run(
            [str(binary("clud")), "--codex", "-p", "probe"],
            cwd=str(tmp_path),
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
    finally:
        # #1700: the launch starts a daemon whose state lives under this
        # `HOME`. Left running, it outlived the test, kept writing into a
        # `tmp_path` pytest had already deleted (#1695's retention policy),
        # and recreated `home/` there once the next parametrization was
        # handed the same directory name.
        process.run(
            [str(binary("clud")), "daemon", "stop"],
            cwd=str(tmp_path),
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
    assert result.returncode == 0, result.stderr
    shim_dir = home / ".clud" / "state" / "rm-shim"
    assert f"{shim_dir}|{shim_dir / 'safe-rm'}|" in result.stdout, result.stdout
    _assert_no_daemon_left(home)


def _assert_no_daemon_left(home: Path) -> None:
    """#1700: the daemon a test started under its `HOME` is gone."""
    info = home / ".clud" / "state" / "daemon.json"
    if not info.is_file():
        return
    pid = json.loads(info.read_text(encoding="utf-8"))["pid"]
    deadline = time.monotonic() + 10
    while psutil.pid_exists(pid) and time.monotonic() < deadline:
        time.sleep(0.1)
    assert not psutil.pid_exists(pid), f"daemon {pid} outlived the test"


@pytest.mark.parametrize("operand", ["/", "/.", "//", "/./", "/.."])
def test_root_is_refused_without_handoff(world, operand: str) -> None:
    shim, _, env, log = world
    result = run(shim, env, "-rf", operand)
    assert result.returncode == 2, result
    assert json.loads(result.stdout)["decision"] == "deny"
    assert not log.exists()


@pytest.mark.parametrize("operand", ["/usr", "/etc", "/home", "/tmp", "/var", "/nix", "/opt"])
def test_top_level_is_refused_without_handoff(world, operand: str) -> None:
    shim, _, env, log = world
    result = run(shim, env, "-rf", operand)
    assert result.returncode == 2, result
    assert "top-level" in json.loads(result.stdout)["reason"]
    assert not log.exists()


@pytest.mark.parametrize("operand", [".", "..", "./", "../", "child/.", "child/.."])
def test_current_and_parent_are_refused_without_handoff(world, operand: str) -> None:
    shim, _, env, log = world
    result = run(shim, env, "-rf", operand)
    assert result.returncode == 2, result
    assert "current or parent directory" in json.loads(result.stdout)["reason"]
    assert not log.exists()


def test_home_and_mixed_batch_are_refused_atomically(world) -> None:
    shim, home, env, log = world
    safe = home / "safe"
    safe.mkdir()
    operands = (str(home), str(home) + "/", str(home) + "/.")
    for operand in operands:
        result = run(shim, env, "-rf", str(safe), operand)
        assert result.returncode == 2, (operand, result)
        assert safe.is_dir()
        assert not log.exists()


def test_symlink_with_trailing_slash_and_no_preserve_root_are_refused(world) -> None:
    shim, home, env, log = world
    link = home.parent / "link"
    link.symlink_to("/", target_is_directory=True)
    for args in (("-rf", str(link) + "/"), ("-rf", "--no-preserve-root", "x")):
        result = run(shim, env, *args)
        assert result.returncode == 2, result
        assert not log.exists()


def test_handoff_is_the_first_real_executable_after_shim(world) -> None:
    shim, home, env, log = world
    env["PATH"] = os.pathsep.join((str(shim.parent), str(shim.parent), str(log.parent / "stub")))
    result = run(shim, env, "-rf", str(home / "missing"))
    assert result.returncode == 0, result
    args = json.loads(log.read_text(encoding="utf-8"))
    prefix = ["-x"] if sys.platform == "darwin" else ["--preserve-root=all", "--one-file-system"]
    assert args == [*prefix, "-rf", str(home / "missing")]


def test_handoff_ignores_a_recording_rm_before_the_shim(world, tmp_path: Path) -> None:
    shim, home, env, log = world
    before_dir = tmp_path / "before"
    before_dir.mkdir()
    before_log = tmp_path / "before.log"
    before = before_dir / ("r" + "m")
    before.write_text('#!/bin/sh\nprintf before > "$RM_BEFORE_LOG"\n', encoding="utf-8")
    before.chmod(0o755)
    env["RM_BEFORE_LOG"] = str(before_log)
    env["PATH"] = os.pathsep.join(
        (str(before_dir), str(shim.parent), str(tmp_path / "stub"), "/usr/bin")
    )
    result = run(shim, env, "-rf", str(home / "missing"))
    assert result.returncode == 0, result
    assert not before_log.exists(), "handoff must not select an earlier PATH entry"
    assert json.loads(log.read_text(encoding="utf-8"))[-1] == str(home / "missing")


def test_handoff_refuses_when_shim_directory_is_not_on_path(world, tmp_path: Path) -> None:
    shim, home, env, log = world
    env["PATH"] = os.pathsep.join((str(tmp_path / "stub"), "/usr/bin"))
    result = run(shim, env, "-rf", str(home / "missing"))
    assert result.returncode == 2, result
    assert "shim directory is missing" in json.loads(result.stdout)["reason"]
    assert not log.exists()


def test_handoff_injects_guard_flags_even_when_names_follow_double_dash(world) -> None:
    shim, _, env, log = world
    result = run(shim, env, "--", "--preserve-root=all", "--one-file-system")
    assert result.returncode == 0, result
    args = json.loads(log.read_text(encoding="utf-8"))
    prefix = ["-x"] if sys.platform == "darwin" else ["--preserve-root=all", "--one-file-system"]
    assert args == [
        *prefix,
        "--",
        "--preserve-root=all",
        "--one-file-system",
    ]


def test_busybox_handoff_does_not_receive_gnu_only_flags(world, tmp_path: Path) -> None:
    shim, home, env, log = world
    stub_dir = tmp_path / "stub"
    applet = stub_dir / "busybox"
    applet.write_text(
        # Debian's system interpreter is outside clud's shim. python-name-lint: allow-next-line
        "#!/usr/bin/python3\nimport json, os, sys\n"
        'open(os.environ["RM_STUB_LOG"], "w").write(json.dumps(sys.argv[1:]))\n',
        encoding="utf-8",
    )
    applet.chmod(0o755)
    (stub_dir / ("r" + "m")).unlink()
    (stub_dir / ("r" + "m")).symlink_to(applet)
    result = run(shim, env, "-rf", str(home / "missing"))
    assert result.returncode == 0, result
    assert json.loads(log.read_text(encoding="utf-8")) == ["-rf", str(home / "missing")]


def test_no_handoff_is_refused(world) -> None:
    shim, home, env, log = world
    env["PATH"] = str(shim.parent)
    result = run(shim, env, "-rf", str(home / "missing"))
    assert result.returncode == 2, result
    assert "handoff" in json.loads(result.stdout)["reason"]
    assert not log.exists()


def test_real_handoff_deletes_outside_session_roots_in_disposable_world(world) -> None:
    shim, home, env, _ = world
    env["PATH"] = os.pathsep.join((str(shim.parent), "/usr/bin", "/bin"))
    target = home / ".cache" / "probe"
    target.parent.mkdir()
    target.write_text("disposable", encoding="utf-8")
    result = run(shim, env, "-rf", str(target))
    assert result.returncode == 0, result
    assert not target.exists()


def test_missing_and_relative_parent_match_native_behavior(world) -> None:
    shim, home, env, _ = world
    env["PATH"] = os.pathsep.join((str(shim.parent), "/usr/bin", "/bin"))
    sub = home / "sub"
    sub.mkdir()
    dist = home / "dist"
    dist.mkdir()
    assert run(shim, env, "-rf", "../dist", cwd=sub).returncode == 0
    assert not dist.exists()
    assert run(shim, env, "-rf", "missing", cwd=sub).returncode == 0
    missing = run(shim, env, "missing", cwd=sub)
    assert missing.returncode == 1
    assert "No such file" in missing.stderr


def test_all_visible_home_children_are_refused_as_one_batch(world) -> None:
    shim, home, env, log = world
    children = [home / name for name in ("a", "b", "c")]
    for child in children:
        child.write_text("keep", encoding="utf-8")
    result = run(shim, env, "-rf", *(str(child) for child in children))
    assert result.returncode == 2, result
    assert all(child.exists() for child in children)
    assert not log.exists()


def test_partial_visible_home_children_are_allowed(world) -> None:
    shim, home, env, log = world
    children = [home / name for name in ("a", "b", "c")]
    for child in children:
        child.write_text("keep", encoding="utf-8")
    result = run(shim, env, "-rf", *(str(child) for child in children[:2]))
    assert result.returncode == 0, result
    handed_off = json.loads(log.read_text(encoding="utf-8"))
    assert handed_off[-2:] == [str(child) for child in children[:2]]


def test_each_call_writes_one_child_audit_record(world) -> None:
    shim, home, env, _ = world
    missing = str(home / "missing")
    allowed = run(shim, env, "-rf", missing)
    denied = run(shim, env, "-rf", "/")
    assert (allowed.returncode, denied.returncode) == (0, 2)
    audit_root = home / ".clud" / "state" / "logs" / ("r" + "m")
    files = list(audit_root.glob("*.jsonl"))
    records = [json.loads(line) for file in files for line in file.read_text().splitlines()]
    assert len(records) == 2, records
    assert all(record["role"] == "child" for record in records)
    assert records[0]["operands"] == ["-rf", missing]
    assert records[0]["exit"] == 0
    assert records[1]["exit"] == 2
    assert records[1]["handoff"] is None


def _without_session(env: dict[str, str]) -> dict[str, str]:
    from tests.shim_env import session_key_names

    names = session_key_names(binary("clud-shim"))
    return {key: value for key, value in env.items() if key not in names}


def test_outside_a_session_rm_passes_through_without_floor_or_audit(world) -> None:
    """#1546: with no valid session the alias is the next `rm`, unmodified.

    PATH holds only the shim and the recording stub, so the refused-in-session
    operand below can reach nothing but the recorder.
    """
    shim, home, env, log = world
    env = _without_session(env)
    env["PATH"] = os.pathsep.join((str(shim.parent), str(log.parent / "stub")))
    result = run(shim, env, "-rf", str(home))
    assert result.returncode == 0, result
    assert result.stdout == "", "no JSON decision outside a session"
    assert json.loads(log.read_text(encoding="utf-8")) == ["-rf", str(home)]
    assert not (home / ".clud" / "state" / "logs" / ("r" + "m")).exists()


def test_stale_session_alias_dir_passes_through(world, tmp_path: Path) -> None:
    shim, home, env, log = world
    env["PATH"] = os.pathsep.join((str(shim.parent), str(log.parent / "stub")))
    env["CLUD_RM_SHIM_DIR"] = str(tmp_path / "gone")
    result = run(shim, env, "-rf", str(home / "missing"))
    assert result.returncode == 0, result
    assert json.loads(log.read_text(encoding="utf-8")) == ["-rf", str(home / "missing")]


_OLD_CORPUS = json.loads(
    (Path(__file__).resolve().parents[1] / "ci/docker/rm_protection/stub_cases.json").read_text(
        encoding="utf-8"
    )
)


@pytest.mark.parametrize("case", _OLD_CORPUS, ids=[case["id"] for case in _OLD_CORPUS])
def test_old_stub_corpus_maps_to_new_floor_and_hook(world, case: dict) -> None:
    shim, home, env, log = world
    argv = [str(home) if arg == "$" + "HOME" else arg for arg in case["argv"]]
    result = run(shim, env, *argv)
    assert result.returncode == (2 if case["decision"] == "deny" else 0), (case, result)
    assert log.exists() == (case["decision"] == "allow"), case

    command = shlex.join(["r" + "m", *argv])
    payload = json.dumps(
        {"tool_name": "Bash", "tool_input": {"command": command}, "cwd": str(shim.parent)}
    )
    hook_env = dict(env)
    hook_env["CLUD_SKIP_RM_IDENTITY"] = "1"
    hook = process.run(
        [str(binary("clud-cmd-scan"))],
        input=payload,
        cwd=str(shim.parent),
        env=hook_env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert hook.returncode == 0, (case, hook)
    output = json.loads(hook.stdout)["hookSpecificOutput"]
    assert output["permissionDecision"] == "allow", (case, output)
    assert output["updatedInput"]["command"].startswith("safe-rm "), (case, output)
