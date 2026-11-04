"""The native clud installer entry must gate writes and restore its terminal."""

from __future__ import annotations

import errno
import hashlib
import json
import os
import select
import shutil
import sys
import time
from pathlib import Path

import pytest
from running_process import PseudoTerminalProcess

from tests.process import run as run_process

if sys.platform != "win32":
    import pty
    import termios


def clud_binary() -> Path:
    candidate = os.environ.get("CLUD_TEST_BINARY")
    if not candidate:
        pytest.skip("compiled clud bundle is required")
    path = Path(candidate)
    if not path.is_file():
        pytest.skip("compiled clud bundle is unavailable")
    return path


def required_shell(name: str) -> str:
    shell = shutil.which(name)
    if shell is not None:
        return shell
    if os.environ.get("CLUD_TEST_REQUIRE_INSTALLER_SHELLS") == "1":
        pytest.fail(f"required installer acceptance shell is unavailable: {name}")
    pytest.skip(f"{name} is unavailable")


def assert_fresh_shell_lookup(shell: str, env: dict[str, str], installed: Path) -> None:
    version = run_process(
        [str(clud_binary()), "--version"],
        env=env,
        capture_output=True,
        timeout=15,
        check=True,
    ).stdout.strip()
    for mode in ("-lc", "-ic", "-lic"):
        fresh = run_process(
            [shell, mode, "command -v clud; clud --version"],
            env=env,
            capture_output=True,
            timeout=15,
            check=False,
        )
        assert fresh.returncode == 0, fresh.stderr.decode(errors="replace")
        assert fresh.stdout.splitlines() == [os.fsencode(installed), version]


def isolated_env(home: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["HOME"] = str(home)
    if sys.platform == "win32":
        env["USERPROFILE"] = str(home)
        env.pop("LOCALAPPDATA", None)
    return env


def installer_json(result, status: str, code: int, committed: bool) -> dict:
    assert result.returncode == code, result.stderr.decode(errors="replace")
    report = json.loads(result.stdout)
    assert len(result.stdout.splitlines()) == 1, result.stdout
    assert report["schema"] == "clud-installer-result/v1"
    assert report["status"] == status
    assert report["exit_code"] == code
    assert report["binary_committed"] is committed
    return report


@pytest.fixture
def installer_target(tmp_path: Path):  # noqa: C901
    if sys.platform != "win32":
        home = tmp_path / "home"
        home.mkdir(mode=0o700)
        env = isolated_env(home)
        env["SHELL"] = shutil.which("bash") or "/bin/bash"
        yield env, home / ".local" / "bin" / "clud"
        return

    if not (
        os.environ.get("GITHUB_ACTIONS") == "true"
        and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
    ):
        pytest.skip("Windows User environment test requires a GitHub-hosted runner")
    import winreg

    local = os.environ.get("LOCALAPPDATA")
    if not local:
        pytest.skip("Windows User environment lacks LOCALAPPDATA")
    destination = Path(local) / "Programs" / "clud" / "bin" / "clud.exe"
    install_tree = destination.parents[1]
    if install_tree.exists():
        pytest.skip("Windows User profile already has a clud install tree")
    key = winreg.OpenKey(
        winreg.HKEY_CURRENT_USER,
        "Environment",
        0,
        winreg.KEY_READ | winreg.KEY_WRITE,
    )
    try:
        prior = winreg.QueryValueEx(key, "Path")
    except FileNotFoundError:
        prior = None
    try:
        yield os.environ.copy(), destination
    finally:
        if prior is None:
            try:
                winreg.DeleteValue(key, "Path")
            except FileNotFoundError:
                pass
        else:
            winreg.SetValueEx(key, "Path", 0, prior[1], prior[0])
        key.Close()
        if install_tree.exists():
            quarantine = tmp_path / "installed-clud-tree"
            for attempt in range(20):
                try:
                    install_tree.replace(quarantine)
                    break
                except PermissionError:
                    if attempt == 19:
                        raise
                    time.sleep(0.25)


def assert_crlf(output: bytes) -> None:
    for index, byte in enumerate(output):
        if byte == 10:
            assert index > 0, "selector emitted a bare LF"
            assert output[index - 1] == 13, "selector emitted a bare LF"


@pytest.mark.parametrize(
    ("flags", "expected"),
    [
        (["--installer"], 2),
        (["--installer", "--install-current"], 2),
        (["--installer", "--install-current", "--no-modify-path"], 2),
        (["--installer", "--install-version", "2.8.14"], 2),
    ],
)
def test_no_tty_never_writes(tmp_path: Path, flags: list[str], expected: int) -> None:
    home = tmp_path / "home"
    result = run_process(
        [str(clud_binary()), *flags],
        env=isolated_env(home),
        input=b"",
        capture_output=True,
        timeout=15,
        check=False,
    )
    assert result.returncode == expected, result.stderr.decode(errors="replace")
    assert not home.exists(), "installer entry wrote to the user home"


def test_no_tty_refusal_preserves_existing_startup_file(tmp_path: Path) -> None:
    home = tmp_path / "home"
    home.mkdir()
    profile = home / ".bashrc"
    profile.write_text("# user settings\n")
    env = isolated_env(home)
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current"],
        env=env,
        capture_output=True,
        timeout=15,
        check=False,
    )
    assert result.returncode == 2, result.stderr.decode(errors="replace")
    assert profile.read_text() == "# user settings\n"
    assert not (home / ".local").exists()


@pytest.mark.skipif(sys.platform != "win32", reason="requires Windows User registry")
def test_no_tty_refusal_preserves_windows_user_path() -> None:
    import winreg

    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment") as key:
        try:
            before = winreg.QueryValueEx(key, "Path")
        except FileNotFoundError:
            before = None
        result = run_process(
            [str(clud_binary()), "--installer", "--install-current"],
            env=os.environ.copy(),
            capture_output=True,
            timeout=15,
            check=False,
        )
        assert result.returncode == 2, result.stderr.decode(errors="replace")
        try:
            after = winreg.QueryValueEx(key, "Path")
        except FileNotFoundError:
            after = None
        assert after == before


@pytest.mark.skipif(sys.platform == "win32", reason="requires POSIX launcher symlinks")
def test_upgrade_replaces_uv_launcher_and_preserves_package_target(tmp_path: Path) -> None:
    home = tmp_path / "home"
    target = home / ".local/share/uv/tools/clud/bin/clud"
    target.parent.mkdir(parents=True, mode=0o700)
    shutil.copy2(clud_binary(), target)
    # uv/debug installs may exceed the published-asset limit due to symbols.
    with target.open("r+b") as executable:
        executable.truncate(max(target.stat().st_size, 256 * 1024 * 1024 + 1))
    original = hashlib.sha256(target.read_bytes()).digest()
    cached = target.with_name("cached-clud")
    os.link(target, cached)
    destination = home / ".local/bin/clud"
    destination.parent.mkdir(parents=True, mode=0o700)
    destination.symlink_to(target)
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes", "--no-modify-path"],
        env=isolated_env(home), capture_output=True, timeout=30, check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    assert not destination.is_symlink()
    assert hashlib.sha256(destination.read_bytes()).digest() == hashlib.sha256(
        clud_binary().read_bytes()
    ).digest()
    assert hashlib.sha256(target.read_bytes()).digest() == original
    assert hashlib.sha256(cached.read_bytes()).digest() == original
    assert not destination.with_suffix(".clud-backup").exists()


@pytest.mark.skipif(sys.platform == "win32", reason="requires POSIX startup files")
def test_install_only_preserves_profiles_and_reports_activation(tmp_path: Path) -> None:
    home = tmp_path / "home"
    home.mkdir(mode=0o700)
    profiles = {
        home / ".bash_profile": b"# preserve login configuration\n",
        home / ".bashrc": b"# preserve non-UTF8 configuration: \xff\n",
    }
    for path, content in profiles.items():
        path.write_bytes(content)
        path.chmod(0o400)
    env = isolated_env(home)
    env["SHELL"] = required_shell("bash")
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes", "--no-modify-path"],
        env=env, capture_output=True, timeout=30, check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    destination = home / ".local" / "bin" / "clud"
    assert destination.is_file()
    assert hashlib.sha256(destination.read_bytes()).digest() == hashlib.sha256(
        clud_binary().read_bytes()
    ).digest()
    for path, content in profiles.items():
        assert path.read_bytes() == content
        assert path.stat().st_mode & 0o777 == 0o400
    assert b"activation required" in result.stderr.lower()
    assert b"verified fresh name-based lookup" not in result.stderr


@pytest.mark.skipif(sys.platform != "win32", reason="requires Windows User registry")
def test_install_only_preserves_windows_user_path(installer_target) -> None:
    import winreg

    env, destination = installer_target
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment") as key:
        try:
            before = winreg.QueryValueEx(key, "Path")
        except FileNotFoundError:
            before = None
        result = run_process(
            [str(clud_binary()), "--installer", "--install-current", "--yes", "--no-modify-path"],
            env=env, capture_output=True, timeout=30, check=False,
        )
        assert result.returncode == 0, result.stderr.decode(errors="replace")
        assert destination.is_file()
        try:
            after = winreg.QueryValueEx(key, "Path")
        except FileNotFoundError:
            after = None
        assert after == before
    assert hashlib.sha256(destination.read_bytes()).digest() == hashlib.sha256(
        clud_binary().read_bytes()
    ).digest()
    assert b"activation required" in result.stderr.lower()
    assert b"verified fresh name-based lookup" not in result.stderr


@pytest.mark.parametrize(("flags", "status", "verified"), [
    ([], "complete", True),
    (["--no-modify-path"], "activation_required", False),
])
def test_installer_json_reports_committed_state(installer_target, flags, status, verified) -> None:
    env, destination = installer_target
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes", "--installer-json",
         *flags],
        env=env, capture_output=True, timeout=30, check=False,
    )
    report = installer_json(result, status, 0, True)
    assert report["activation_verified"] is verified
    assert Path(report["destination"]).resolve() == destination.resolve()
    assert hashlib.sha256(destination.read_bytes()).digest() == hashlib.sha256(
        clud_binary().read_bytes()
    ).digest()
    version = run_process([str(clud_binary()), "--version"], capture_output=True, check=True)
    assert report["version"] == version.stdout.decode().strip().split()[-1]


@pytest.mark.parametrize(("flags", "status"), [
    (["--install-current"], "refused"),
    (["--install-version", "not-a-version", "--yes"], "failed"),
])
def test_installer_json_distinguishes_refusal_and_failure(tmp_path, flags, status) -> None:
    home = tmp_path / "home"
    result = run_process(
        [str(clud_binary()), "--installer", "--installer-json", *flags],
        env=isolated_env(home), input=b"", capture_output=True, timeout=15, check=False,
    )
    report = installer_json(result, status, 2, False)
    assert report["activation_verified"] is False
    assert not home.exists(), "structured reporting must not add pre-consent writes"


def test_explicit_current_installs_verified_copy_offline(installer_target) -> None:
    env, destination = installer_target
    env["HTTPS_PROXY"] = "http://127.0.0.1:1"
    env["HTTP_PROXY"] = "http://127.0.0.1:1"
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes"],
        env=env,
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    assert destination.is_file(), result.stderr.decode(errors="replace")
    assert hashlib.sha256(destination.read_bytes()).digest() == hashlib.sha256(
        clud_binary().read_bytes()
    ).digest()
    repeat = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes"],
        env=env,
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert repeat.returncode == 0, repeat.stderr.decode(errors="replace")
    assert destination.read_bytes() == clud_binary().read_bytes()
    assert not list(destination.parent.glob(".clud-install-*"))
    assert not destination.with_suffix(".clud-backup").exists()
    if sys.platform != "win32":
        for profile in (
            destination.parents[2] / ".bash_profile",
            destination.parents[2] / ".bashrc",
        ):
            assert profile.read_text().count("# >>> clud installer PATH >>>") == 1


def test_interrupted_commit_restores_backup_before_reinstall(installer_target) -> None:
    env, destination = installer_target
    command = [str(clud_binary()), "--installer", "--install-current", "--yes"]
    first = run_process(command, env=env, capture_output=True, timeout=30, check=False)
    assert first.returncode == 0, first.stderr.decode(errors="replace")
    backup = destination.with_suffix(".clud-backup")
    backup.write_bytes(destination.read_bytes())
    backup.chmod(destination.stat().st_mode)
    destination.write_bytes(b"interrupted commit")
    recovered = run_process(command, env=env, capture_output=True, timeout=30, check=False)
    assert recovered.returncode == 0, recovered.stderr.decode(errors="replace")
    assert destination.read_bytes() == clud_binary().read_bytes()
    assert not backup.exists()


@pytest.mark.skipif(sys.platform == "win32", reason="requires POSIX bash startup files")
def test_bash_fresh_login_and_interactive_lookup(tmp_path: Path) -> None:
    shell = required_shell("bash")
    home = tmp_path / "home with spaces and 'quotes' $dollars"
    home.mkdir(mode=0o700)
    env = isolated_env(home)
    env["SHELL"] = shell
    env["PATH"] = "/usr/bin:/bin"
    installed = home / ".local" / "bin" / "clud"
    source_version = run_process(
        [str(clud_binary()), "--version"],
        env=env,
        capture_output=True,
        timeout=15,
        check=True,
    ).stdout.strip()
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes"],
        env=env,
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    for mode in ("-lc", "-ic", "-lic"):
        fresh = run_process(
            [shell, mode, "command -v clud; clud --version"],
            env=env,
            capture_output=True,
            timeout=15,
            check=False,
        )
        assert fresh.returncode == 0, fresh.stderr.decode(errors="replace")
        assert fresh.stdout.splitlines() == [
            os.fsencode(installed),
            source_version,
        ]


@pytest.mark.skipif(sys.platform == "win32", reason="requires POSIX bash startup files")
def test_existing_user_bin_and_first_bash_login_file(tmp_path: Path) -> None:
    shell = required_shell("bash")
    home = tmp_path / "home"
    home.mkdir(mode=0o700)
    bin_dir = home / "bin"
    bin_dir.mkdir(mode=0o700)
    login = home / ".bash_login"
    login.write_text("# existing login settings\n")
    login.chmod(0o600)
    env = isolated_env(home)
    env["SHELL"] = shell
    env["PATH"] = f"{bin_dir}:/usr/bin:/bin"
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes"],
        env=env,
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    assert (bin_dir / "clud").is_file()
    assert login.read_text().startswith("# existing login settings\n")
    assert login.read_text().count("# >>> clud installer PATH >>>") == 1
    assert not (home / ".bash_profile").exists()


@pytest.mark.skipif(sys.platform == "win32", reason="requires POSIX fish startup files")
def test_fish_custom_xdg_uses_nonuniversal_snippet(tmp_path: Path) -> None:
    shell = required_shell("fish")
    home = tmp_path / "home"
    home.mkdir(mode=0o700)
    config = home / "custom config"
    env = isolated_env(home)
    env["SHELL"] = shell
    env["XDG_CONFIG_HOME"] = str(config)
    env["PATH"] = "/usr/bin:/bin"
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes"],
        env=env,
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    snippet = config / "fish" / "conf.d" / "clud-path.fish"
    assert snippet.is_file()
    assert "fish_add_path --path --move --" in snippet.read_text()
    assert not (config / "fish" / "fish_variables").exists()
    assert_fresh_shell_lookup(shell, env, home / ".local" / "bin" / "clud")


@pytest.mark.skipif(sys.platform == "win32", reason="requires POSIX zsh startup files")
def test_zsh_custom_zdotdir_activation(tmp_path: Path) -> None:
    shell = required_shell("zsh")
    home = tmp_path / "home"
    home.mkdir(mode=0o700)
    dotdir = home / "custom zsh"
    env = isolated_env(home)
    env["SHELL"] = shell
    env["ZDOTDIR"] = str(dotdir)
    env["PATH"] = "/usr/bin:/bin"
    result = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes"],
        env=env,
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    assert (dotdir / ".zprofile").is_file()
    assert (dotdir / ".zshrc").is_file()
    assert_fresh_shell_lookup(shell, env, home / ".local" / "bin" / "clud")


def test_exact_older_release_downloads_selected_executable(installer_target) -> None:
    env, destination = installer_target
    prior_version = os.environ.get("CLUD_TEST_PRIOR_VERSION", "2.8.13")
    result = run_process(
        [str(clud_binary()), "--installer", "--install-version", prior_version, "--yes"],
        env=env,
        capture_output=True,
        timeout=120,
        check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    assert destination.is_file(), result.stderr.decode(errors="replace")
    selected = run_process(
        [str(destination), "--version"],
        env=env,
        capture_output=True,
        timeout=15,
        check=False,
    )
    assert selected.returncode == 0, selected.stderr.decode(errors="replace")
    assert selected.stdout.strip() == f"clud {prior_version}".encode()


@pytest.mark.skipif(sys.platform != "win32", reason="requires a running Windows executable")
def test_running_windows_target_remains_valid(installer_target) -> None:
    env, destination = installer_target
    first = run_process(
        [str(clud_binary()), "--installer", "--install-current", "--yes"],
        env=env,
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert destination.is_file(), first.stderr.decode(errors="replace")
    before = hashlib.sha256(destination.read_bytes()).digest()
    running = run_process(
        [str(destination), "--installer", "--install-current", "--yes"],
        env=env,
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert running.returncode == 1, running.stderr.decode(errors="replace")
    assert destination.is_file()
    assert hashlib.sha256(destination.read_bytes()).digest() == before
    assert not destination.with_suffix(".clud-backup").exists()


@pytest.mark.skipif(sys.platform == "win32", reason="requires a POSIX PTY")
def test_explicit_menu_escape_restores_posix_terminal(tmp_path: Path) -> None:
    home = tmp_path / "home"
    pid, master = pty.fork()
    if pid == 0:
        binary = str(clud_binary())
        os.execve(binary, [binary, "--installer"], isolated_env(home))
    original = termios.tcgetattr(master)
    output = bytearray()
    sent = False
    status = None
    deadline = time.monotonic() + 15
    try:
        while time.monotonic() < deadline:
            readable, _, _ = select.select([master], [], [], 0.05)
            if readable:
                try:
                    output.extend(os.read(master, 4096))
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
            if b"Install clud" in output and not sent:
                os.write(master, b"\x1b")
                sent = True
            waited, child_status = os.waitpid(pid, os.WNOHANG)
            if waited:
                status = child_status
                break
        assert sent, output.decode(errors="replace")
        assert status is not None, output.decode(errors="replace")
        assert os.waitstatus_to_exitcode(status) == 130, output.decode(errors="replace")
        assert termios.tcgetattr(master) == original
        assert_crlf(output)
        assert not home.exists()
    finally:
        if status is None:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
        os.close(master)


@pytest.mark.skipif(sys.platform != "win32", reason="requires Windows ConPTY")
def test_explicit_menu_escape_through_conpty(tmp_path: Path) -> None:
    from winpty import PtyProcess

    home = tmp_path / "home"
    process = PtyProcess.spawn(
        [str(clud_binary()), "--installer"],
        env=isolated_env(home),
        dimensions=(24, 80),
    )
    output = ""
    sent = False
    deadline = time.monotonic() + 15
    try:
        while time.monotonic() < deadline:
            readable, _, _ = select.select([process.fileobj], [], [], 0.05)
            if readable:
                try:
                    output += process.read(4096)
                except EOFError:
                    break
            if "Install clud" in output and not sent:
                process.write("\x1b")
                sent = True
            if not process.isalive():
                break
        assert sent, output
        assert process.exitstatus == 130, output
        assert_crlf(output.encode())
        assert not home.exists()
    finally:
        process.close(force=True)


@pytest.mark.parametrize("cancel", ["\x1b", "\x03"])
def test_first_run_offer_defaults_to_not_now_and_cancels_without_writes(
    tmp_path: Path, cancel: str,
) -> None:
    home = tmp_path / "untouched-home"
    env = isolated_env(home)
    env["PATH"] = ""
    before = browser_user_path_snapshot()
    process = PseudoTerminalProcess([str(clud_binary())], env=env, capture=True)
    try:
        process.expect("[x] Not now", timeout=30)
        assert "[ ] Install this version" in process.output_text
        process.write(cancel)
        # running-process reports a child's interrupt exit as an exception.
        with pytest.raises(KeyboardInterrupt):
            process.wait(timeout=30)
        assert process.poll() == 130, process.output_text
        assert not home.exists(), process.output_text
        assert browser_user_path_snapshot() == before
    finally:
        if process.poll() is None:
            process.kill()
        process.close()


@pytest.mark.parametrize("opener_fails", [False, True])
def test_browser_menu_uses_canonical_url_without_installing(
    tmp_path: Path, opener_fails: bool,
) -> None:
    if not os.environ.get("CLUD_INSTALLER_CI_FIXTURE_DIR"):
        pytest.skip("requires the candidate's compile-time browser fixture")
    home = tmp_path / "untouched-home"
    log = tmp_path / "opened-url"
    if opener_fails:
        log = tmp_path / "missing-parent" / "opened-url"
    env = isolated_env(home)
    env["CLUD_INSTALLER_CI_BROWSER_LOG"] = str(log)
    before = browser_user_path_snapshot()
    process = PseudoTerminalProcess(
        [str(clud_binary()), "--installer"], env=env, capture=True,
    )
    page = "https://talos-hq.github.io/clud/install/index.html"
    try:
        process.expect("Install clud", timeout=30)
        process.write("\x1b[B\x1b[B\r")
        assert process.wait(timeout=30) == int(opener_fails), process.output_text
        assert page in process.output_text
        if opener_fails:
            assert "could not open browser" in process.output_text
            assert not log.exists()
        else:
            assert log.read_text(encoding="utf-8") == page
        assert not home.exists(), process.output_text
        assert browser_user_path_snapshot() == before
    finally:
        if process.poll() is None:
            process.kill()
        process.close()


def browser_user_path_snapshot():
    """Windows HOME overrides do not isolate the real per-user registry."""
    if sys.platform != "win32":
        return None
    import winreg

    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment") as key:
        try:
            return winreg.QueryValueEx(key, "Path")
        except FileNotFoundError:
            return None
