"""`clud sudo`: the masked prompt is the only route from the user to sudo.

A fake sudo models the behaviour measured on classic sudo 1.9 and sudo-rs 0.2
with no controlling terminal: a ticket is keyed on the parent process, `-n`
never prompts, and `-S` reads one password line from stdin. A fake lokr
answers prompts from a queue, so no dialog ever opens.
"""

import json
import os
import sys
from pathlib import Path

import pytest

from tests.test_hello import _run

PASSWORD = "pw-SENTINEL-4711"

pytestmark = pytest.mark.skipif(os.name == "nt", reason="sudo is a Unix tool")

FAKE_SUDO = r"""
import json, os, sys
state = os.environ["FAKE_SUDO_STATE"]
mode = open(os.path.join(state, "mode")).read().strip()
expected = open(os.path.join(state, "expected")).read()
args = sys.argv[1:]
with open(os.path.join(state, "calls.jsonl"), "a") as log:
    log.write(json.dumps({"argv": args, "password_in_env": any(
        expected in value for value in os.environ.values())}) + "\n")
flags, index, prompt = set(), 0, "Password: "
while index < len(args) and args[index].startswith("-"):
    flag = args[index]
    index += 1
    if flag == "--":
        break
    if flag == "-p":
        prompt = args[index]
        index += 1
        continue
    flags.update(flag[1:])
command = args[index:]
ticket = os.path.join(state, "ticket")
if "k" in flags:
    if os.path.exists(ticket):
        os.remove(ticket)
    if not command:
        sys.exit(0)
if mode == "denied":
    print("Sorry, user alice may not run sudo on host.", file=sys.stderr)
    sys.exit(1)


def read_line():
    line = b""
    while not line.endswith(b"\n"):
        byte = os.read(0, 1)
        if not byte:
            return None
        line += byte
    return line.decode().rstrip("\n")


fresh = os.path.exists(ticket) and open(ticket).read() == str(os.getppid())
if mode != "nopasswd" and not fresh:
    if "n" in flags:
        print("sudo: a password is required", file=sys.stderr)
        sys.exit(1)
    if "S" not in flags:
        print("sudo: a terminal is required to read the password", file=sys.stderr)
        sys.exit(1)
    for attempt in range(3):
        sys.stderr.write(prompt)
        sys.stderr.flush()
        line = read_line()
        with open(os.path.join(state, "reads.jsonl"), "a") as log:
            log.write(json.dumps(line is not None) + "\n")
        if line == expected:
            break
        if line is None:
            print("sudo: no password was provided", file=sys.stderr)
            sys.exit(1)
        print("Sorry, try again.", file=sys.stderr)
    else:
        print("sudo: 3 incorrect password attempts", file=sys.stderr)
        sys.exit(1)
    if mode != "no-ticket":
        open(ticket, "w").write(str(os.getppid()))
if "v" in flags or not command:
    sys.exit(0)
os.execvp(command[0], command)
"""

FAKE_LOKR = r"""
import json, os, pathlib, sys
state = pathlib.Path(os.environ["FAKE_SUDO_STATE"])
with open(state / "lokr.jsonl", "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
answers = (state / "answers").read_text().splitlines()
if not answers:
    sys.exit(1)
(state / "answers").write_text("".join(line + "\n" for line in answers[1:]))
if answers[0] == "CANCEL":
    sys.exit(3)
target = sys.argv[sys.argv.index("--to-file") + 1]
pathlib.Path(target).write_text(answers[0])
"""

COMMAND = ["--", "sh", "-c", "echo RAN-AS-ROOT; cat; echo STDIN-DONE"]


def _script(path: Path, body: str) -> Path:
    path.write_text(f"#!{sys.executable}\n{body}")
    path.chmod(0o700)
    return path


def _sudo(tmp_path: Path, mode: str, answers: list[str], *args: str):
    state = tmp_path / "state"
    state.mkdir(exist_ok=True)
    (state / "mode").write_text(mode)
    (state / "expected").write_text(PASSWORD)
    (state / "answers").write_text("".join(answer + "\n" for answer in answers))
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir(exist_ok=True)
    _script(bin_dir / "sudo", FAKE_SUDO)
    lokr = _script(tmp_path / "lokr", FAKE_LOKR)
    result = _run(
        "sudo",
        *(args or COMMAND),
        input_data="AGENT-STDIN\n",
        env_overrides={
            "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
            "FAKE_SUDO_STATE": str(state),
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_LOKR_BINARY": str(lokr),
            "LOKR_TEST_MODE": "1",
        },
    )
    calls = []
    if (state / "calls.jsonl").exists():
        calls = [json.loads(line) for line in (state / "calls.jsonl").read_text().splitlines()]
    prompts = []
    if (state / "lokr.jsonl").exists():
        prompts = [json.loads(line) for line in (state / "lokr.jsonl").read_text().splitlines()]
    return result, calls, prompts


def _assert_secret_never_leaked(result, calls, prompts) -> None:
    assert PASSWORD not in result.stdout + result.stderr
    for call in calls:
        assert PASSWORD not in " ".join(call["argv"]), call
        assert not call["password_in_env"], call
    for prompt in prompts:
        assert PASSWORD not in " ".join(prompt), prompt


def test_password_from_masked_prompt_runs_the_command_once(tmp_path: Path) -> None:
    result, calls, prompts = _sudo(tmp_path, "password", [PASSWORD])
    assert result.returncode == 0, result.stderr
    assert "RAN-AS-ROOT" in result.stdout
    # The command keeps the caller's stdin; the password never reaches it.
    assert "AGENT-STDIN" in result.stdout
    assert calls[0]["argv"] == ["-n", "-v"]
    assert calls[1]["argv"] == ["-S", "-p", "clud-sudo-password-request\n", "-v"]
    assert calls[-2]["argv"][:2] == ["-n", "sh"]
    assert calls[-1]["argv"] == ["-k"]
    assert len(prompts) == 1
    title = prompts[0][prompts[0].index("--title") + 1]
    assert title.startswith("sudo password for ")
    message = prompts[0][prompts[0].index("--message") + 1]
    assert message.startswith("Run as root: sh -c")
    _assert_secret_never_leaked(result, calls, prompts)


def test_wrong_passwords_reprompt_then_succeed(tmp_path: Path) -> None:
    result, calls, prompts = _sudo(tmp_path, "password", ["wrong-1", "wrong-2", PASSWORD])
    assert result.returncode == 0, result.stderr
    assert "RAN-AS-ROOT" in result.stdout
    assert len(prompts) == 3
    second = prompts[1][prompts[1].index("--message") + 1]
    assert second.startswith("sudo rejected the previous password (attempt 2 of 3).")
    _assert_secret_never_leaked(result, calls, prompts)


def test_three_wrong_passwords_run_nothing(tmp_path: Path) -> None:
    result, calls, prompts = _sudo(tmp_path, "password", ["wrong-1", "wrong-2", "wrong-3"])
    assert result.returncode == 1
    assert "RAN-AS-ROOT" not in result.stdout
    assert "rejected the password 3 times" in result.stderr
    assert len(prompts) == 3
    assert not any(call["argv"][:2] == ["-n", "sh"] for call in calls)
    # One sudo session read exactly one line per prompt: no extra empty-password
    # attempt for pam_faillock to count.
    reads = (tmp_path / "state" / "reads.jsonl").read_text().split()
    assert reads == ["true", "true", "true"]
    assert sum(call["argv"][:1] == ["-S"] for call in calls) == 1


def test_cancel_runs_nothing(tmp_path: Path) -> None:
    result, calls, prompts = _sudo(tmp_path, "password", ["CANCEL"])
    assert result.returncode == 3
    assert "cancelled; nothing was run" in result.stderr
    assert "RAN-AS-ROOT" not in result.stdout
    assert [call["argv"][:2] for call in calls] == [["-n", "-v"], ["-S", "-p"]]
    assert len(prompts) == 1


def test_passwordless_sudo_never_prompts(tmp_path: Path) -> None:
    result, calls, prompts = _sudo(tmp_path, "nopasswd", [])
    assert result.returncode == 0, result.stderr
    assert "RAN-AS-ROOT" in result.stdout
    assert prompts == []
    assert all("-S" not in call["argv"] for call in calls)


def test_refused_account_is_reported_without_a_prompt(tmp_path: Path) -> None:
    result, _calls, prompts = _sudo(tmp_path, "denied", [PASSWORD])
    assert result.returncode == 1
    assert "may not run sudo" in result.stderr
    assert "no prompt was opened" in result.stderr
    assert prompts == []


def test_no_ticket_policy_pipes_the_password_to_the_command_sudo(tmp_path: Path) -> None:
    result, calls, prompts = _sudo(tmp_path, "no-ticket", [PASSWORD])
    assert result.returncode == 0, result.stderr
    assert "RAN-AS-ROOT" in result.stdout
    # sudo consumed the only stdin line; the command reads end of input.
    assert "STDIN-DONE" in result.stdout
    assert "AGENT-STDIN" not in result.stdout
    assert calls[-2]["argv"][:4] == ["-S", "-p", "", "sh"]
    _assert_secret_never_leaked(result, calls, prompts)


def test_clud_owned_sudo_options_are_refused(tmp_path: Path) -> None:
    for argv in (["--", "-A", "id"], ["--", "-S", "id"], ["--", "-v"], ["--", "-u", "root"]):
        result, calls, prompts = _sudo(tmp_path, "password", [PASSWORD], *argv)
        assert result.returncode == 2, (argv, result.stderr)
        assert calls == [], argv
        assert prompts == []


def test_dry_run_shows_the_plan_without_prompting(tmp_path: Path) -> None:
    result, calls, prompts = _sudo(
        tmp_path,
        "password",
        [PASSWORD],
        "--dry-run",
        "--reason",
        "Install foo",
        "--",
        "apt-get",
        "install",
        "-y",
        "foo",
    )
    assert result.returncode == 0, result.stderr
    assert "sudo password for " in result.stdout
    assert "Install foo." in result.stdout
    assert "sudo -n apt-get install -y foo" in result.stdout
    assert calls == []
    assert prompts == []


def _hook(tmp_path: Path, command: str):
    import shutil

    from tests import process

    name = "clud-cmd-scan.exe" if sys.platform == "win32" else "clud-cmd-scan"
    binary = None
    if os.environ.get("CLUD_TEST_BINARY"):
        sibling = Path(os.environ["CLUD_TEST_BINARY"]).with_name(name)
        binary = sibling if sibling.is_file() else None
    binary = binary or (Path(shutil.which(name)) if shutil.which(name) else None)
    assert binary is not None, "clud-cmd-scan test binary not found"
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    env = {key: value for key, value in os.environ.items() if key != "CLUD_BAD_CMD_OVERRIDE"}
    env.update(
        HOME=str(home),
        USERPROFILE=str(home),
        CLUD_DAEMON_STATE_DIR=str(tmp_path / "hook-state"),
        CLUD_SKIP_RM_IDENTITY="1",
    )
    payload = json.dumps(
        {
            "session_id": "sudo-agent",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "cwd": str(tmp_path),
            "tool_input": {"command": command},
        }
    )
    return process.run(
        [str(binary)],
        input=payload,
        cwd=tmp_path,
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
    )


def test_agent_hook_refuses_raw_sudo_with_the_blessed_recipe(tmp_path: Path) -> None:
    result = _hook(tmp_path, "sudo apt-get install -y foo")
    assert result.returncode == 2, result.stdout + result.stderr
    decision = json.loads(result.stdout.strip().splitlines()[-1])
    reason = decision["hookSpecificOutput"]["permissionDecisionReason"]
    assert decision["hookSpecificOutput"]["permissionDecision"] == "deny"
    assert "`clud sudo -- apt-get install -y foo`" in reason
    assert "masked password prompt" in reason
    allowed = _hook(tmp_path, "clud sudo -- apt-get install -y foo")
    assert allowed.returncode == 0, allowed.stdout + allowed.stderr
