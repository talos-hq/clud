"""#1708: credentials go to a sink without entering agent-visible output."""

from pathlib import Path

from tests.test_hello import _run


def test_secret_prompt_exposes_explicit_sinks() -> None:
    result = _run("secret", "--help")
    assert result.returncode == 0, result.stderr
    for option in ["--to-file", "--to-command", "--vault", "--multiline", "--pattern"]:
        assert option in result.stdout


def test_secret_prompt_file_sink_keeps_multiline_value_private(tmp_path: Path) -> None:
    import os
    import stat

    source = tmp_path / "fixture"
    sentinel = "-----BEGIN PRIVATE KEY-----\nFAKE_TEST_KEY\n-----END PRIVATE KEY-----\n"
    source.write_text(sentinel)
    destination = tmp_path / "credential"
    result = _run(
        "secret",
        "--multiline",
        "--to-file",
        str(destination),
        env_overrides={
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
        },
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "stored"
    assert "FAKE_TEST_KEY" not in result.stdout + result.stderr
    assert destination.read_text() == sentinel
    if os.name != "nt":
        assert stat.S_IMODE(destination.stat().st_mode) == 0o600


def test_secret_prompt_cancellation_is_distinct(tmp_path: Path) -> None:
    destination = tmp_path / "cancelled"
    result = _run(
        "secret",
        "--to-file",
        str(destination),
        env_overrides={"CLUD_INTEGRATION_TESTS": "1", "CLUD_TEST_SECRET_PROMPT_CANCEL": "1"},
    )
    assert result.returncode == 3, result.stderr
    assert result.stdout.strip() == "cancelled"
    assert not destination.exists()


def test_secret_command_sink_suppresses_echoed_credentials(tmp_path: Path) -> None:
    import sys

    secret = "fake-credential-sentinel"
    source = tmp_path / "fixture"
    source.write_text(secret)
    received = tmp_path / "received"
    script = (
        "import pathlib,sys; value=sys.stdin.read(); "
        "pathlib.Path(sys.argv[1]).write_text(value); "
        "print(value); print(value,file=sys.stderr); sys.exit(int(sys.argv[2]))"
    )
    for exit_code, status in [(0, "stored"), (7, "failed")]:
        result = _run(
            "secret",
            "--to-command",
            sys.executable,
            "-c",
            script,
            str(received),
            str(exit_code),
            env_overrides={
                "CLUD_INTEGRATION_TESTS": "1",
                "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
            },
        )
        assert result.returncode == (0 if exit_code == 0 else 1), result.stderr
        assert result.stdout.strip() == status
        assert secret not in result.stdout + result.stderr
        assert received.read_text() == secret


def test_secret_command_timeout_covers_blocked_stdin(tmp_path: Path) -> None:
    import sys
    import time

    source = tmp_path / "fixture"
    source.write_text("x" * (512 * 1024))
    started = time.monotonic()
    result = _run(
        "secret",
        "--to-command",
        sys.executable,
        "-c",
        "import time; time.sleep(10)",
        env_overrides={
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
            "CLUD_TEST_SECRET_PROMPT_TIMEOUT_MS": "100",
        },
    )
    assert result.returncode == 1, result.stderr
    assert result.stdout.strip() == "failed"
    assert time.monotonic() - started < 5


def test_secret_prompt_refuses_missing_or_conflicting_sinks() -> None:
    for arguments in [(), ("--to-file", "x", "--vault", "kimi")]:
        result = _run("secret", *arguments)
        assert result.returncode == 2


def test_askpass_refuses_direct_agent_tool_invocation(tmp_path: Path) -> None:
    import os

    from tests import process

    links = _run("__link-aliases", str(tmp_path))
    assert links.returncode == 0, links.stderr
    alias = tmp_path / ("clud-askpass.exe" if os.name == "nt" else "clud-askpass")
    result = process.run([str(alias), "Password"], capture_output=True, text=True, timeout=10)
    assert result.returncode == 2
    assert result.stdout == ""
    assert "must be invoked by git, ssh or sudo" in result.stderr


def test_git_askpass_receives_secret_through_its_protocol(tmp_path: Path) -> None:
    import os
    import shutil

    import pytest

    from tests import process

    git = shutil.which("git")
    if git is None:
        pytest.skip("git is required for the real askpass protocol probe")
    links = _run("__link-aliases", str(tmp_path))
    assert links.returncode == 0, links.stderr
    source = tmp_path / "fixture"
    source.write_text("fake-askpass-token")
    alias = tmp_path / ("clud-askpass.exe" if os.name == "nt" else "clud-askpass")
    env = {
        **os.environ,
        "GIT_ASKPASS": str(alias),
        "GIT_TERMINAL_PROMPT": "0",
        "CLUD_INTEGRATION_TESTS": "1",
        "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
    }
    result = process.run(
        [git, "-c", "credential.helper=", "credential", "fill"],
        input="protocol=https\nhost=example.invalid\nusername=test\n\n",
        capture_output=True,
        text=True,
        timeout=10,
        env=env,
    )
    assert result.returncode == 0, result.stderr
    # Only this fake fixture test deliberately reads the askpass protocol.
    assert "password=fake-askpass-token" in result.stdout


def test_native_secret_window_can_render_and_deliver(tmp_path: Path) -> None:
    """Exercise the real window when the native runner has a graphical desktop."""
    import os
    import shutil
    import sys

    import pytest

    from tests import process
    from tests.test_hello import CLUD

    source = tmp_path / "fixture"
    source.write_text("FAKE_NATIVE_PRIVATE_KEY\nsecond line\n")
    destination = tmp_path / "received"
    env = {
        **os.environ,
        "CLUD_INTEGRATION_TESTS": "1",
        "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
        "CLUD_TEST_SECRET_PROMPT_GUI": "1",
    }
    argv = [CLUD, "secret", "--multiline", "--to-file", str(destination)]
    required = env.get("CLUD_TEST_REQUIRE_SECRET_GUI") == "1"
    if sys.platform == "linux" and not (env.get("DISPLAY") or env.get("WAYLAND_DISPLAY")):
        xvfb = shutil.which("xvfb-run")
        if xvfb is None:
            assert not required, "native GUI coverage requires xvfb-run on a headless Linux runner"
            pytest.skip("a desktop or xvfb-run is required for the native window probe")
        argv = [xvfb, "-a", *argv]
    result = process.run(argv, capture_output=True, text=True, timeout=30, env=env)
    diagnostics = result.stdout + result.stderr
    if result.returncode == 1 and "No graphical secret prompt is available" in diagnostics:
        assert not required, diagnostics
        pytest.skip("native runner has no supported graphics context")
    assert result.returncode == 0, diagnostics
    assert result.stdout.strip() == "stored"
    assert "FAKE_NATIVE_PRIVATE_KEY" not in diagnostics
    assert destination.read_text() == source.read_text()


def test_ssh_askpass_unlocks_a_fixture_key(tmp_path: Path) -> None:
    import os
    import shutil

    import pytest

    from tests import process

    keygen = shutil.which("ssh-keygen")
    if keygen is None:
        pytest.skip("OpenSSH is required for the real askpass protocol probe")
    source = tmp_path / "fixture"
    source.write_text("fake-askpass-token")
    key = tmp_path / "test-key"
    created = process.run(
        [keygen, "-q", "-t", "ed25519", "-N", "fake-askpass-token", "-f", str(key)],
        capture_output=True,
        text=True,
        timeout=15,
    )
    assert created.returncode == 0, created.stderr
    linked = _run("__link-aliases", str(tmp_path))
    assert linked.returncode == 0, linked.stderr
    alias = tmp_path / ("clud-askpass.exe" if os.name == "nt" else "clud-askpass")
    env = {
        **os.environ,
        "SSH_ASKPASS": str(alias),
        "SSH_ASKPASS_REQUIRE": "force",
        "DISPLAY": os.environ.get("DISPLAY", ":clud-fixture"),
        "CLUD_INTEGRATION_TESTS": "1",
        "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
    }
    result = process.run(
        [keygen, "-y", "-f", str(key)], capture_output=True, text=True, timeout=15, env=env
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.startswith("ssh-ed25519 ")
    assert "fake-askpass-token" not in result.stdout + result.stderr


def _wait_for_wayland_socket(compositor, socket_path: Path) -> None:
    """Consume compositor startup output with a deadline, without sleep polling."""
    import time

    deadline = time.monotonic() + 20
    while not socket_path.is_socket():
        remaining = deadline - time.monotonic()
        assert remaining > 0, "Wayland compositor did not create its socket"
        assert compositor.poll() is None, "Wayland compositor exited during startup"
        compositor.next_stderr(timeout=remaining)


def test_native_secret_window_on_wayland(tmp_path: Path) -> None:
    """Force a real Wayland window, with X11 unavailable as a fallback."""
    import os
    import shutil
    import sys

    import pytest

    from tests import process
    from tests.test_hello import CLUD

    if sys.platform != "linux":
        pytest.skip("Wayland is a Linux desktop backend")
    weston = shutil.which("weston")
    if weston is None:
        assert os.environ.get("CLUD_TEST_REQUIRE_SECRET_GUI") != "1", (
            "Wayland coverage requires Weston"
        )
        pytest.skip("Weston is required for the Wayland window probe")
    runtime = tmp_path / "runtime"
    runtime.mkdir(mode=0o700)
    source = tmp_path / "fixture"
    source.write_text("FAKE_WAYLAND_PRIVATE_KEY\nsecond line\n")
    destination = tmp_path / "received"
    env = {key: value for key, value in os.environ.items() if key not in (
        "DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET",
    )}
    env.update({
        "XDG_RUNTIME_DIR": str(runtime),
        "WAYLAND_DISPLAY": "clud-secret-test",
        "LIBGL_ALWAYS_SOFTWARE": "1",
        "CLUD_INTEGRATION_TESTS": "1",
        "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
        "CLUD_TEST_SECRET_PROMPT_GUI": "1",
    })
    help_result = process.run([weston, "--help"], capture_output=True, text=True, timeout=10)
    renderer = "--renderer=gl" if "--renderer" in help_result.stdout else "--use-gl"
    compositor = process.Popen(
        [weston, "--backend=headless-backend.so", renderer, "--no-config",
         "--socket=clud-secret-test", "--idle-time=0"],
        env=env, stdout=process.PIPE, stderr=process.PIPE, text=True,
    )
    try:
        _wait_for_wayland_socket(compositor, runtime / "clud-secret-test")
        result = process.run(
            [CLUD, "secret", "--multiline", "--to-file", str(destination)],
            capture_output=True, text=True, timeout=30, env=env,
        )
        diagnostics = result.stdout + result.stderr
        assert result.returncode == 0, diagnostics
        assert result.stdout.strip() == "stored"
        assert "FAKE_WAYLAND_PRIVATE_KEY" not in diagnostics
        assert destination.read_text() == source.read_text()
    finally:
        compositor.terminate()
        compositor.wait(timeout=10)


def test_required_gui_probes_cannot_skip_after_session_scrub(tmp_path: Path, monkeypatch) -> None:
    import os
    import shutil
    import sys

    import pytest

    from ci.env import scrub_clud_session_env

    env = {**os.environ, "CLUD_TEST_REQUIRE_SECRET_GUI": "1"}
    scrub_clud_session_env(env)
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    monkeypatch.setattr(os, "environ", env)
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setattr(shutil, "which", lambda _name: None)

    def invoke(probe) -> None:
        try:
            probe(tmp_path)
        except pytest.skip.Exception as error:
            pytest.fail(f"mandatory GUI probe skipped after session scrub: {error}")

    for probe, message in (
        (test_native_secret_window_can_render_and_deliver, "native GUI coverage requires xvfb-run"),
        (test_native_secret_window_on_wayland, "Wayland coverage requires Weston"),
    ):
        with pytest.raises(AssertionError, match=message):
            invoke(probe)


def test_native_secret_window_authenticates_real_glab(tmp_path: Path) -> None:
    """The actual consumer logs in and verifies its saved token on every native host."""
    import os
    import shutil
    import sys

    import pytest

    from tests import process
    from tests.secret_glab_fixture import HOST, TOKEN, environment, gitlab_api
    from tests.test_hello import CLUD

    required = os.environ.get("CLUD_TEST_REQUIRE_SECRET_GUI") == "1"
    glab = os.environ.get("CLUD_TEST_GLAB_BINARY")
    if not glab or not Path(glab).is_file():
        assert not required, "native secret acceptance requires the pinned real GitLab CLI"
        pytest.skip("pinned GitLab CLI is required for the native credential consumer probe")
    source = tmp_path / "fixture-token"
    source.write_text(TOKEN)
    env = environment(tmp_path, source)
    with gitlab_api() as (api_host, accepted):
        argv = [
            CLUD, "secret", "--title", "GitLab fixture login",
            "--label", f"Token for {HOST}", "--to-command", glab,
            "auth", "login", "--stdin", "--hostname", HOST,
            "--api-host", api_host, "--api-protocol", "http", "--git-protocol", "https",
            "--insecure-storage",
        ]
        if sys.platform == "linux" and not (env.get("DISPLAY") or env.get("WAYLAND_DISPLAY")):
            xvfb = shutil.which("xvfb-run")
            if xvfb is None:
                assert not required, "native GitLab consumer coverage requires xvfb-run"
                pytest.skip("a desktop or xvfb-run is required for the native GitLab consumer")
            argv = [xvfb, "-a", *argv]
        result = process.run(argv, capture_output=True, text=True, timeout=40, env=env)
        assert TOKEN not in result.stdout + result.stderr
        assert result.returncode == 0, result.stderr
        assert result.stdout.strip() == "stored"
        login_requests = len(accepted)
        assert login_requests >= 1, "glab login never authenticated against the fixture"
        status = process.run(
            [glab, "auth", "status", "--hostname", HOST],
            capture_output=True, text=True, timeout=15, env=env, cwd=str(tmp_path),
        )
        assert TOKEN not in status.stdout + status.stderr
        assert status.returncode == 0, status.stderr
        assert len(accepted) > login_requests, "glab status did not verify the saved token"


def test_secret_pattern_rejects_without_writing_or_exposing_input(tmp_path: Path) -> None:
    source = tmp_path / "fixture"
    source.write_text("synthetic-mismatch")
    destination = tmp_path / "credential"
    result = _run(
        "secret",
        "--pattern",
        "[0-9]+",
        "--to-file",
        str(destination),
        env_overrides={
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
        },
    )
    status_ok = result.returncode == 1
    assert status_ok, "unexpected prompt status"
    output_ok = result.stdout.strip() == "failed"
    assert output_ok, "unexpected prompt output"
    assert not destination.exists()
    private_output = source.read_text() not in result.stdout + result.stderr
    assert private_output, "credential exposed"


def test_secret_pattern_invalid_regex_is_an_argument_error(tmp_path: Path) -> None:
    destination = tmp_path / "credential"
    result = _run("secret", "--pattern", "[", "--to-file", str(destination))
    status_ok = result.returncode == 2
    assert status_ok, "unexpected argument status"
    assert not destination.exists()


def test_secret_pattern_accepts_full_multiline_value(tmp_path: Path) -> None:
    source = tmp_path / "fixture"
    source.write_text("synthetic\nfixture\n")
    destination = tmp_path / "credential"
    result = _run(
        "secret",
        "--multiline",
        "--pattern",
        "(?s)synthetic.*",
        "--to-file",
        str(destination),
        env_overrides={
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
        },
    )
    status_ok = result.returncode == 0
    assert status_ok, "unexpected prompt status"
    output_ok = result.stdout.strip() == "stored"
    assert output_ok, "unexpected prompt output"
    contents_ok = destination.read_bytes() == source.read_bytes()
    assert contents_ok, "stored content differs"
    private_output = source.read_text() not in result.stdout + result.stderr
    assert private_output, "credential exposed"
