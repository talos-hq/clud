"""#1708: credentials go to a sink without entering agent-visible output."""

from pathlib import Path

from tests.test_hello import _run


def test_secret_prompt_exposes_explicit_sinks() -> None:
    result = _run("secret", "--help")
    assert result.returncode == 0, result.stderr
    for option in [
        "--title",
        "--label",
        "--message",
        "--hint",
        "--dry-run",
        "--to-file",
        "--to-command",
        "--vault",
        "--multiline",
        "--pattern",
    ]:
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
        "--title",
        "GitHub API key for fixture.example",
        "--message",
        "Authenticate the fixture; find the key in GitHub developer settings",
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
        "--title",
        "GitHub API key for fixture.example",
        "--message",
        "Authenticate the fixture; find the key in GitHub developer settings",
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
            "--title",
            "GitHub API key for fixture.example",
            "--message",
            "Authenticate the fixture; find the key in GitHub developer settings",
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
        "--title",
        "GitHub API key for fixture.example",
        "--message",
        "Authenticate the fixture; find the key in GitHub developer settings",
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
        result = _run(
            "secret",
            "--title",
            "GitHub API key for fixture.example",
            "--message",
            "Authenticate the fixture; find the key in GitHub developer settings",
            *arguments,
        )
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


def test_secret_fixture_cannot_enable_real_gui(tmp_path: Path) -> None:
    source = tmp_path / "fixture"
    source.write_text("FAKE_TEST_KEY\nsecond line\n")
    destination = tmp_path / "received"
    result = _run(
        "secret",
        "--title",
        "SSH identity for fixture.example",
        "--message",
        "Load the test identity from the fixture secret manager",
        "--multiline",
        "--to-file",
        str(destination),
        env_overrides={
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
            "CLUD_TEST_SECRET_PROMPT_GUI": "1",
        },
    )
    assert result.returncode == 0, result.stderr
    assert destination.read_text() == source.read_text()
    assert "FAKE_TEST_KEY" not in result.stdout + result.stderr


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


def test_secret_test_mode_without_fixture_refuses_ui(tmp_path: Path) -> None:
    destination = tmp_path / "unused"
    result = _run(
        "secret",
        "--title",
        "GitHub OAuth for fixture.example",
        "--message",
        "Enable login; find Client ID in GitHub developer settings",
        "--to-file",
        str(destination),
        env_overrides={"CLUD_INTEGRATION_TESTS": "1", "CLUD_TEST_SECRET_PROMPT_GUI": "1"},
    )
    assert result.returncode == 1
    assert result.stdout.strip() == "failed"
    assert not destination.exists()


def test_secret_fixture_authenticates_real_glab(tmp_path: Path) -> None:
    """A fake prompt feeds the real consumer without opening any window."""
    import os

    import pytest

    from tests import process
    from tests.secret_glab_fixture import HOST, TOKEN, environment, gitlab_api
    from tests.test_hello import CLUD

    required = os.environ.get("CLUD_TEST_REQUIRE_SECRET_CONSUMER") == "1"
    glab = os.environ.get("CLUD_TEST_GLAB_BINARY")
    if not glab or not Path(glab).is_file():
        assert not required, "native secret acceptance requires the pinned real GitLab CLI"
        pytest.skip("pinned GitLab CLI is required for the native credential consumer probe")
    source = tmp_path / "fixture-token"
    source.write_text(TOKEN)
    env = environment(tmp_path, source)
    with gitlab_api() as (api_host, accepted):
        argv = [
            CLUD,
            "secret",
            "--title",
            "GitLab fixture login",
            "--label",
            f"Authenticate glab for {HOST}; create a token in GitLab user settings > Access Tokens",
            "--to-command",
            glab,
            "auth",
            "login",
            "--stdin",
            "--hostname",
            HOST,
            "--api-host",
            api_host,
            "--api-protocol",
            "http",
            "--git-protocol",
            "https",
            "--insecure-storage",
        ]
        result = process.run(argv, capture_output=True, text=True, timeout=40, env=env)
        assert TOKEN not in result.stdout + result.stderr
        assert result.returncode == 0, result.stderr
        assert result.stdout.strip() == "stored"
        login_requests = len(accepted)
        assert login_requests >= 1, "glab login never authenticated against the fixture"
        status = process.run(
            [glab, "auth", "status", "--hostname", HOST],
            capture_output=True,
            text=True,
            timeout=15,
            env=env,
            cwd=str(tmp_path),
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
        "--title",
        "GitHub API key for fixture.example",
        "--message",
        "Authenticate the fixture; find the key in GitHub developer settings",
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
    result = _run(
        "secret",
        "--title",
        "GitHub API key for fixture.example",
        "--message",
        "Authenticate the fixture; find the key in GitHub developer settings",
        "--pattern",
        "[",
        "--to-file",
        str(destination),
    )
    status_ok = result.returncode == 2
    assert status_ok, "unexpected argument status"
    assert not destination.exists()


def test_secret_pattern_accepts_full_multiline_value(tmp_path: Path) -> None:
    source = tmp_path / "fixture"
    source.write_text("synthetic\nfixture\n")
    destination = tmp_path / "credential"
    result = _run(
        "secret",
        "--title",
        "GitHub API key for fixture.example",
        "--message",
        "Authenticate the fixture; find the key in GitHub developer settings",
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


def test_secret_request_context_is_required(tmp_path: Path) -> None:
    for context in [
        (),
        ("--title", "GitHub OAuth for example.com"),
        ("--message", "Find the key in GitHub settings to enable login"),
        ("--title", "password", "--message", "credential"),
    ]:
        result = _run("secret", "--dry-run", *context, "--to-file", str(tmp_path / "unused"))
        assert result.returncode == 2
        assert not (tmp_path / "unused").exists()


def test_secret_dry_run_shows_context_without_acquiring_or_writing(tmp_path: Path) -> None:
    result = _run(
        "secret",
        "--dry-run",
        "--title",
        "GitHub OAuth for fixture.example",
        "--message",
        "Enable login; find Client ID in GitHub developer settings",
        "--hint",
        "Iv1. prefix",
        "--to-file",
        str(tmp_path / "unused"),
    )
    assert result.returncode == 0, result.stderr
    assert "GitHub OAuth for fixture.example" in result.stdout
    assert "Enable login; find Client ID in GitHub developer settings" in result.stdout
    assert "Expected format: Iv1. prefix" in result.stdout
    assert "This is NOT your computer or login password." in result.stdout
    assert not (tmp_path / "unused").exists()
