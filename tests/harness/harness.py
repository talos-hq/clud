"""Real Claude Code on a scripted mock agent backend (#1323).

A :class:`Harness` builds an isolated world in a temp directory and runs the
installed Claude Code against ``mock-agent serve``:

* **home** — ``HOME`` and ``CLAUDE_CONFIG_DIR`` (``home/.claude``), populated
  by ``clud install-assets`` so the test runs the skills, agents and workflow
  users get.
* **repo** — a git checkout on ``main`` with a local bare ``origin``.
* **bin** — first on PATH: a fake ``gh`` (``fake_gh.py``), ``rm``, and
  ``safe-rm``, copies of the built ``clud-shim`` as a clud session installs
  them. The env stamps a clud session with ``bin`` as its alias directory, so
  ``rm`` enforces its in-session floor (#1546). ``CLUD_RM_ROOTS`` is the repo
  and ``home/.clud/tmp``, as clud sets it for a session (#1340).
* **logs** — the backend's request log, the hook recorder's log and, with
  ``run(answers=...)``, the answered-questions log.

AskUserQuestion: `claude -p` only offers the tool when a permission prompt
tool is configured, so ``run(answers={...})`` adds ``--mcp-config`` +
``--permission-prompt-tool`` for the stub server ``answer_mcp.py`` and
registers the PreToolUse hook ``answer_hook.py``, which answers each call via
``updatedInput.answers``. A test that scripts AskUserQuestion must pass
``answers`` (``{"*": label}`` for a default); see :meth:`Harness.answer_args`.

PATH never contains clud's own shim directories (``~/.clud/state/shims``,
``~/.clud/state/rm-shim``): a developer's installed aliases would otherwise
stand in for the build under test. Scripts and hooks here run under the
test's own interpreter.

Opt-in: these tests need an installed Claude Code and run only with
``CLUD_REAL_CLAUDE_TESTS=1``. See ``docs/architecture/testing-tiers.md``.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import time
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from tests import process
from tests.metered_consent import consent_for
from tests.shim_env import session_env

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
EXE = ".exe" if os.name == "nt" else ""
ENABLED = os.environ.get("CLUD_REAL_CLAUDE_TESTS") == "1"
# The AskUserQuestion answerer's MCP server and its permission-prompt tool
# (``answer_mcp.py``; the names must match its SERVER and TOOL).
ANSWER_SERVER = "clud_harness"
ANSWER_TOOL = f"mcp__{ANSWER_SERVER}__answer"


def _built(name: str, env_var: str) -> Path:
    override = os.environ.get(env_var)
    if override:
        return Path(override)
    return ROOT / "target" / "debug" / f"{name}{EXE}"


def claude_executable() -> str | None:
    override = os.environ.get("CLUD_REAL_CLAUDE")
    if override:
        return override
    return shutil.which("claude")


@dataclass
class RunResult:
    returncode: int
    stdout: str
    requests: list[dict[str, Any]]
    hooks: list[dict[str, Any]]
    stderr: str = ""
    events: list[dict[str, Any]] = field(default_factory=list)
    # One record per AskUserQuestion question the harness answered:
    # ``{"t": ms, "agent_type", "question", "header", "answer"}``.
    questions: list[dict[str, Any]] = field(default_factory=list)

    def first_request(self, role: str = "main") -> dict[str, Any]:
        for request in self.requests:
            if request.get("role") == role:
                return request
        raise AssertionError(
            f"no request from role {role!r}; roles seen: {[r.get('role') for r in self.requests]}"
        )

    def prompt_text(self, role: str = "main") -> str:
        """Everything the model saw from the user in the role's first request."""
        return json.dumps(self.first_request(role).get("messages"))

    def questions_before(self, role: str = "grind-prework") -> list[dict[str, Any]]:
        """See :func:`questions_before`."""
        return questions_before(self, role)

    def questions_after(self, role: str = "grind-prework") -> list[dict[str, Any]]:
        """See :func:`questions_after`."""
        return questions_after(self, role)


def _split_at_role(
    result: RunResult, role: str
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """AskUserQuestion hook records before / from `role`'s first hook event."""
    before: list[dict[str, Any]] = []
    after: list[dict[str, Any]] = []
    seen = False
    for record in result.hooks:
        seen = seen or record.get("agent_type") == role
        if record.get("tool_name") == "AskUserQuestion":
            (after if seen else before).append(record)
    return before, after


def questions_before(result: RunResult, role: str = "grind-prework") -> list[dict[str, Any]]:
    """The `AskUserQuestion` hook records made before `role` first acted.

    ``hooks.jsonl`` is appended in call order, so this walks it and stops at
    the first record whose ``agent_type`` is `role` (for the up-front rule,
    ``grind-prework``); if `role` never appears, every AskUserQuestion record
    is returned. Pair it with :func:`questions_after` to assert the whole
    question round happened before prework and none came after (#1392 U10).
    """
    return _split_at_role(result, role)[0]


def questions_after(result: RunResult, role: str = "grind-prework") -> list[dict[str, Any]]:
    """The `AskUserQuestion` hook records from `role`'s first event on, made
    by any agent including the main session; empty if `role` never ran."""
    return _split_at_role(result, role)[1]


class Harness:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.home = root / "home"
        self.config = self.home / ".claude"
        self.repo = root / "repo"
        self.origin = root / "origin.git"
        self.bin = root / "bin"
        self.logs = root / "logs"
        for directory in (self.config, self.bin, self.logs):
            directory.mkdir(parents=True, exist_ok=True)
        self.clud = _built("clud", "CLUD_HARNESS_CLUD")
        self.mock_agent = _built("mock-agent", "CLUD_HARNESS_MOCK_AGENT")
        claude = claude_executable()
        if claude is None:
            raise RuntimeError("Claude Code is not installed; set CLUD_REAL_CLAUDE")
        self.claude = claude
        for binary in (self.clud, self.mock_agent):
            if not binary.is_file():
                raise RuntimeError(
                    f"{binary} is missing; build it first (`bash build` or soldr cargo build)"
                )
        self.hide_clud = False
        # Each `run` is one Claude Code session, launched with `--session-id`
        # so a test knows the id up front: the `/grind` run facts live at
        # `~/.clud/tmp/grind/<session_id>.json` (#1337). Claude Code refuses
        # to reuse an id, so a second run needs `new_session()` first.
        self.session_id = str(uuid.uuid4())
        self._session_used = False
        self.gh_state = root / "gh-state.json"
        self.write_gh_state({"repo": "o/r", "issues": {}, "prs": []})
        self._install_bin()

    # ---- world -----------------------------------------------------------------

    def _install_bin(self) -> None:
        gh = self.bin / "gh"
        gh.write_text(
            f'#!/bin/sh\nexec "{sys.executable}" "{HERE / "fake_gh.py"}" "$@"\n', encoding="utf-8"
        )
        gh.chmod(0o755)
        shim = self.clud.parent / f"clud-shim{EXE}"
        for name in ("rm", "safe-rm"):
            shutil.copyfile(shim, self.bin / f"{name}{EXE}")
            (self.bin / f"{name}{EXE}").chmod(0o755)

    def write_gh_state(self, state: dict[str, Any]) -> None:
        self.gh_state.write_text(json.dumps(state, indent=1), encoding="utf-8")

    def read_gh_state(self) -> dict[str, Any]:
        return json.loads(self.gh_state.read_text(encoding="utf-8"))

    def git(self, *args: str, cwd: Path | None = None) -> str:
        result = process.run(
            ["git", *args],
            cwd=str(cwd or self.repo),
            capture_output=True,
            text=True,
            env=self.env(),
            timeout=60,
        )
        if result.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)} failed: {result.stdout}{result.stderr}")
        return (result.stdout or "").strip()

    def make_repo(self) -> None:
        """`repo` on `main` with one commit, pushed to a bare `origin`."""
        self.repo.mkdir(parents=True, exist_ok=True)
        self.git("init", "-q", "--bare", "-b", "main", str(self.origin), cwd=self.root)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "harness@example.invalid")
        self.git("config", "user.name", "clud harness")
        (self.repo / "README.md").write_text("# harness repo\n", encoding="utf-8")
        self.git("add", "README.md")
        self.git("commit", "-q", "-m", "initial")
        self.git("remote", "add", "origin", str(self.origin))
        self.git("push", "-q", "-u", "origin", "main")
        self.git("remote", "set-head", "origin", "main")

    def install_assets(self) -> None:
        result = process.run(
            [str(self.clud), "install-assets", "--home", str(self.home)],
            capture_output=True,
            text=True,
            env=self.env(),
            timeout=60,
        )
        if result.returncode != 0:
            raise RuntimeError(f"clud install-assets failed: {result.stdout}{result.stderr}")

    # ---- /grind run facts ------------------------------------------------------

    def new_session(self) -> str:
        """Start a fresh session id for the next `run`; returns it."""
        self.session_id = str(uuid.uuid4())
        self._session_used = False
        return self.session_id

    def run_facts_path(self) -> Path:
        """The `/grind` run-facts file of the current session (#1337)."""
        return self.home / ".clud" / "tmp" / "grind" / f"{self.session_id}.json"

    def write_run_facts(self, facts: dict[str, Any]) -> Path:
        """Record run facts the way the router does, for the current session."""
        path = self.run_facts_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(facts, indent=1), encoding="utf-8")
        return path

    # ---- environment -----------------------------------------------------------

    def path(self) -> str:
        """PATH for the harness: ours first, clud's session shims removed."""
        keep = [
            str(self.bin),
            str(self.clud.parent),
            str(Path(self.claude).parent),
            str(Path(sys.executable).parent),
        ]
        for entry in os.environ.get("PATH", "").split(os.pathsep):
            if (
                not entry
                or ".clud" + os.sep + "state" in entry
                or "uv" + os.sep + "archive-v0" in entry
            ):
                continue
            if entry not in keep:
                keep.append(entry)
        if self.hide_clud:
            # Simulate "clud is not installed": drop every directory that
            # holds a `clud`, including the build dir. Hooks and fixture
            # scripts use absolute paths, so they keep working.
            keep = [d for d in keep if not (Path(d) / f"clud{EXE}").exists()]
        return os.pathsep.join(keep)

    def env(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        env = {
            name: os.environ[name]
            for name in ("LANG", "LC_ALL", "TERM", "SYSTEMROOT", "TEMP", "TMP")
            if name in os.environ
        }
        env.update(
            {
                "PATH": self.path(),
                "HOME": str(self.home),
                "USERPROFILE": str(self.home),
                "CLAUDE_CONFIG_DIR": str(self.config),
                "ANTHROPIC_API_KEY": "fixture-api-key-canary",
                "NO_PROXY": "127.0.0.1,localhost",
                "DISABLE_TELEMETRY": "1",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                "FAKE_GH_STATE": str(self.gh_state),
                "CLUD_EXE": str(self.clud),
                "CLUD_RM_ROOTS": os.pathsep.join(
                    [str(self.repo), str(self.home / ".clud" / "tmp")]
                ),
                "GIT_TERMINAL_PROMPT": "0",
                **session_env(self.clud.parent / f"clud-shim{EXE}", self.bin),
            }
        )
        env.update(extra or {})
        return env

    def settings(
        self,
        *,
        cmd_scan: bool = True,
        answers: dict[str, Any] | None = None,
        record_scheduler: bool = False,
    ) -> Path:
        """A `--settings` file: the recorder, then clud's real command guard.

        The recorder and command guard are injected consistently. Project
        settings hooks may also load under the isolated CLAUDE_CONFIG_DIR;
        source-loading probes must establish their own positive control.
        With `answers`, an `AskUserQuestion` hook (``answer_hook.py``) picks
        each question's option from that table and logs it with its time to
        ``logs/questions.jsonl`` (#1402); see :meth:`answer_args`.
        """
        record = f'"{sys.executable}" "{HERE / "record_hook.py"}" "{self.logs / "hooks.jsonl"}"'
        pre = [{"type": "command", "command": record}]
        if cmd_scan:
            pre.append(
                {"type": "command", "command": f'"{self.clud.parent / ("clud-cmd-scan" + EXE)}"'}
            )
        pre_entries: list[dict[str, Any]] = []
        if answers is not None:
            answers_path = self.root / "answers.json"
            answers_path.write_text(json.dumps(answers, indent=1), encoding="utf-8")
            answer = (
                f'"{sys.executable}" "{HERE / "answer_hook.py"}" '
                f'"{answers_path}" "{self.logs / "questions.jsonl"}"'
            )
            pre_entries.append(
                {"matcher": "AskUserQuestion", "hooks": [{"type": "command", "command": answer}]}
            )
        pre_entries.append({"matcher": "*", "hooks": pre})
        settings = {
            "hooks": {
                "PreToolUse": pre_entries,
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": record}]}],
            }
        }
        if record_scheduler:
            settings["hooks"]["PostToolUse"] = [
                {
                    "matcher": "CronCreate|CronDelete|ScheduleWakeup",
                    "hooks": [{"type": "command", "command": record}],
                }
            ]
        path = self.root / "settings.json"
        path.write_text(json.dumps(settings, indent=1), encoding="utf-8")
        return path

    def answer_args(self) -> list[str]:
        """Claude Code flags that make `AskUserQuestion` answerable in `-p`.

        Print mode drops AskUserQuestion from the model's tool list unless a
        permission prompt tool is configured (Claude Code 2.1.x: the tool's
        ``isEnabled`` is false in a non-interactive session without one), so
        the answer hook alone never fires. This registers ``answer_mcp.py`` as
        an MCP server and names its tool with ``--permission-prompt-tool``;
        the hook then answers through ``updatedInput``, and the MCP tool
        answers from the same table if Claude Code routes the call to it.
        Call :meth:`settings` with the answers first (it writes the table).
        """
        answers_path = self.root / "answers.json"
        config = {
            "mcpServers": {
                ANSWER_SERVER: {
                    "type": "stdio",
                    "command": sys.executable,
                    "args": [
                        str(HERE / "answer_mcp.py"),
                        str(answers_path),
                        str(self.logs / "questions.jsonl"),
                    ],
                }
            }
        }
        path = self.root / "answer-mcp.json"
        path.write_text(json.dumps(config, indent=1), encoding="utf-8")
        # `--mcp-config` is variadic: keep an option right after its value.
        return ["--mcp-config", str(path), "--permission-prompt-tool", ANSWER_TOOL]

    # ---- running ---------------------------------------------------------------

    @contextmanager
    def backend(self, script: dict[str, Any]) -> Iterator[str]:
        script_path = self.root / "script.json"
        script_path.write_text(json.dumps(script, indent=1), encoding="utf-8")
        port_file = self.root / "backend.port"
        port_file.unlink(missing_ok=True)
        log = self.logs / "requests.jsonl"
        log.write_text("", encoding="utf-8")
        child = process.Popen(
            [
                str(self.mock_agent),
                "serve",
                "--script",
                str(script_path),
                "--port",
                "0",
                "--log",
                str(log),
                "--port-file",
                str(port_file),
            ],
            stdout=process.PIPE,
            stderr=process.PIPE,
            text=True,
        )
        try:
            deadline = time.monotonic() + 10
            while not port_file.is_file() or not port_file.read_text().strip():
                if child.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("mock-agent serve did not start")
                time.sleep(0.05)
            yield f"http://127.0.0.1:{port_file.read_text().strip()}"
        finally:
            child.kill()
            child.wait(timeout=10)

    def run(
        self,
        prompt: str,
        script: dict[str, Any],
        *,
        skip_permissions: bool = True,
        cmd_scan: bool = True,
        extra_args: list[str] | None = None,
        extra_env: dict[str, str] | None = None,
        timeout: float = 120,
        answers: dict[str, Any] | None = None,
        cwd: Path | None = None,
    ) -> RunResult:
        if self._session_used:
            raise RuntimeError(
                f"session {self.session_id} already ran; call new_session() before another run"
            )
        self._session_used = True
        (self.logs / "hooks.jsonl").write_text("", encoding="utf-8")
        (self.logs / "questions.jsonl").write_text("", encoding="utf-8")
        with self.backend(script) as base_url:
            argv = [
                self.claude,
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-sonnet-5",
                "--settings",
                str(self.settings(cmd_scan=cmd_scan, answers=answers)),
                "--session-id",
                self.session_id,
            ]
            if answers is not None:
                argv += self.answer_args()
            if skip_permissions:
                argv.append("--dangerously-skip-permissions")
            argv += list(extra_args or [])
            argv.append(prompt)
            env = self.env({"ANTHROPIC_BASE_URL": base_url, **(extra_env or {})})
            # Launches that pick a metered provider consent to it explicitly.
            env = consent_for(env, argv)
            cwd = cwd if cwd is not None else self.repo if self.repo.is_dir() else self.root
            result = process.run(
                argv, cwd=str(cwd), capture_output=True, text=True, env=env, timeout=timeout
            )
        return RunResult(
            returncode=result.returncode,
            stdout=result.stdout or "",
            stderr=result.stderr or "",
            requests=_jsonl(self.logs / "requests.jsonl"),
            hooks=_jsonl(self.logs / "hooks.jsonl"),
            questions=_jsonl(self.logs / "questions.jsonl"),
            events=[
                e for e in (_json_or_none(line) for line in (result.stdout or "").splitlines()) if e
            ],
        )

    # ---- running through clud ----------------------------------------------------

    def clud_env(self, base_url: str, extra: dict[str, str] | None = None) -> dict[str, str]:
        """Env for a real `clud --openrouter` launch against the mock (#1984).

        The OpenRouter key comes from a test vault, the direct route's
        upstream is the mock (`CLUD_TEST_DIRECT_UPSTREAM_URL`, debug builds
        only), the key preflight asks the mock (404 = continue offline), and
        the daemon is a disposable one in this world's state dir.
        """
        vault = self.root / "vault"
        vault.mkdir(exist_ok=True)
        (vault / "clud_openrouter--api_key_v1.secret").write_text(
            "sk-or-v1-harness-fake-key-0000", encoding="utf-8"
        )
        registry = self.root / "registry"
        registry.mkdir(exist_ok=True)
        env = self.env(
            {
                "CLUD_INTEGRATION_TESTS": "1",
                "CLUD_TEST_SECRET_STORE_DIR": str(vault),
                "CLUD_TEST_DIRECT_UPSTREAM_URL": base_url,
                "CLUD_TEST_CREDENTIAL_PROBE_URL": f"{base_url}/v1/key",
                "CLUD_SERVER_SETTINGS": "0",
                "CLUD_DAEMON_TEST_MODE": "1",
                "CLUD_DAEMON_STATE_DIR": str(self.root / "daemon-state"),
                "CLUD_SESSION_DB": str(registry / "sessions.redb"),
                "CLUD_SESSION_LOCK": str(registry / "sessions.lock"),
                "CLUD_NO_UNLOCK": "1",
                "SOLDR_BROKER_AUTOSPAWN": "0",
                "XDG_STATE_HOME": str(self.home / ".local" / "state"),
                "XDG_CACHE_HOME": str(self.home / ".cache"),
                "CLUD_HOOK_HOME": str(self.home),
                **(extra or {}),
            }
        )
        for name in ("ANTHROPIC_API_KEY", "CLUD_ALLOW_WORKTREES", "CLUD_UNSAFE_MODE"):
            env.pop(name, None)
        if extra:
            env.update(extra)
        return env

    def _install_cmd_scan_alias(self) -> None:
        # clud's own `--settings` fragment runs the bare `clud-cmd-scan`, so
        # the build under test must answer to that name first on PATH.
        alias = self.bin / f"clud-cmd-scan{EXE}"
        shutil.copyfile(self.clud, alias)
        alias.chmod(0o755)

    def stop_daemon(self, env: dict[str, str]) -> None:
        process.run(
            [str(self.clud), "daemon", "stop"],
            capture_output=True,
            text=True,
            env=env,
            timeout=30,
        )

    def run_clud(
        self,
        prompt: str,
        script: dict[str, Any],
        *,
        provider: str = "--openrouter",
        model: str | None = None,
        record_scheduler: bool = False,
        extra_args: list[str] | None = None,
        extra_env: dict[str, str] | None = None,
        timeout: float = 120,
    ) -> RunResult:
        """One headless `clud <provider> -p` session against the scripted mock.

        Unlike :meth:`run`, Claude Code is launched by clud itself, so clud's
        launch-scoped `--settings` (its PreToolUse and UserPromptSubmit
        hooks) and its route env are what the test exercises. The harness
        recorder is merged in through `--settings` so `hooks.jsonl` still
        sees every call. The process is killed on timeout, and the daemon it
        started is stopped either way.
        """
        if self._session_used:
            raise RuntimeError(
                f"session {self.session_id} already ran; call new_session() before another run"
            )
        self._session_used = True
        (self.logs / "hooks.jsonl").write_text("", encoding="utf-8")
        self._install_cmd_scan_alias()
        with self.backend(script) as base_url:
            env = self.clud_env(base_url, extra_env)
            if provider == "--unified":
                # Every upstream and the gateway's native client credential
                # belong to this disposable test world.
                env.update(
                    {
                        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "0",
                        "ANTHROPIC_API_KEY": "fixture-api-key-canary",
                        "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": base_url,
                        "CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL": base_url,
                        "CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL": base_url,
                        "CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL": base_url,
                    }
                )
            argv = [
                str(self.clud),
                provider,
                *(["--model", model] if model else []),
                "-p",
                prompt,
                "--",
                "--output-format",
                "stream-json",
                "--verbose",
                "--settings",
                str(self.settings(cmd_scan=False, record_scheduler=record_scheduler)),
                "--session-id",
                self.session_id,
                *(extra_args or []),
            ]
            cwd = self.repo if self.repo.is_dir() else self.root
            try:
                result = process.run(
                    argv,
                    cwd=str(cwd),
                    capture_output=True,
                    text=True,
                    env=consent_for(env, argv),
                    timeout=timeout,
                )
            finally:
                self.stop_daemon(env)
        return RunResult(
            returncode=result.returncode,
            stdout=result.stdout or "",
            stderr=result.stderr or "",
            requests=_jsonl(self.logs / "requests.jsonl"),
            hooks=_jsonl(self.logs / "hooks.jsonl"),
            events=[
                e for e in (_json_or_none(line) for line in (result.stdout or "").splitlines()) if e
            ],
        )

    def run_clud_interactive(
        self,
        prompt: str,
        script: dict[str, Any],
        *,
        model: str,
        finished: str,
        timeout: float = 90,
        confirm_background_exit: bool = True,
    ) -> RunResult:
        """Run the real interactive Claude surface in an owned PTY (#1844).

        Print mode omits EnterPlanMode, so it cannot validate its denial.
        Only disposable fixture configs accept onboarding/trust dialogs.
        """
        from running_process import PseudoTerminalProcess

        if self._session_used:
            raise RuntimeError("call new_session() before another run")
        self._session_used = True
        (self.logs / "hooks.jsonl").write_text("", encoding="utf-8")
        self._install_cmd_scan_alias()
        preferences = {
            "hasCompletedOnboarding": True,
            "theme": "dark",
            "bypassPermissionsModeAccepted": True,
            "customApiKeyResponses": {
                "approved": ["fixture-api-key-canary", "fixture-api-key-canary"[-20:]],
                "rejected": [],
            },
            "projects": {str(self.repo): {"hasTrustDialogAccepted": True}},
        }
        for path in (self.home / ".claude.json", self.config / ".claude.json"):
            path.write_text(json.dumps(preferences), encoding="utf-8")
        settings = self.settings(cmd_scan=False, record_scheduler=True)
        document = json.loads(settings.read_text(encoding="utf-8"))
        document["skipDangerousModePermissionPrompt"] = True
        settings.write_text(json.dumps(document), encoding="utf-8")
        with self.backend(script) as base_url:
            # Free models enable the unified redaction gateway. Its client
            # auth still needs a fixture Claude key, while upstream auth
            # comes from the isolated OpenRouter vault. Every possible
            # upstream stays on the scripted loopback backend.
            env = self.clud_env(
                base_url,
                {
                    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "0",
                    "ANTHROPIC_API_KEY": "fixture-api-key-canary",
                    "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": base_url,
                    "CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL": base_url,
                    "CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL": base_url,
                    "CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL": base_url,
                },
            )
            argv = [
                str(self.clud),
                "--openrouter",
                "--model",
                model,
                "--",
                prompt,
                "--settings",
                str(settings),
                "--session-id",
                self.session_id,
            ]
            child = None
            try:
                child = PseudoTerminalProcess(
                    argv, cwd=self.repo, env=consent_for(env, argv), cols=160, rows=40
                )
                child.expect("Launch setup scope", timeout=30)
                child.write("\r")  # The default highlighted choice is session-only.
                child.expect(finished, timeout=timeout)
                child.write("/exit\r")
                if confirm_background_exit:
                    # Claude Code asks only while a background task is still
                    # live; a session whose tasks already finished exits at once.
                    try:
                        child.expect("Background", timeout=15)
                        child.write("\r")  # Confirm Exit and stop tasks, the default.
                    except EOFError:
                        pass
                code = child.wait(timeout=15)
                output = child.output_text
            except Exception as error:
                output = child.output_text if child is not None else ""
                raise AssertionError(
                    f"interactive harness failed: {error}\n{output[-6000:]}"
                ) from error
            finally:
                if child is not None:
                    if child.poll() is None:
                        child.kill()
                    child.close()
                self.stop_daemon(env)
        return RunResult(
            returncode=code,
            stdout=output,
            stderr="",
            requests=_jsonl(self.logs / "requests.jsonl"),
            hooks=_jsonl(self.logs / "hooks.jsonl"),
            events=[],
        )


def tool_results(result: RunResult) -> list[dict[str, Any]]:
    """Every `tool_result` block the session's JSONL stream reported."""
    found: list[dict[str, Any]] = []
    for event in result.events:
        if event.get("type") != "user":
            continue
        content = (event.get("message") or {}).get("content")
        if isinstance(content, list):
            found.extend(
                block
                for block in content
                if isinstance(block, dict) and block.get("type") == "tool_result"
            )
    return found


def tool_result_text(block: dict[str, Any]) -> str:
    content = block.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(str(part.get("text", "")) for part in content if isinstance(part, dict))
    return json.dumps(content)


def _json_or_none(line: str) -> dict[str, Any] | None:
    try:
        value = json.loads(line)
    except ValueError:
        return None
    return value if isinstance(value, dict) else None


def _jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.is_file():
        return []
    return [
        v
        for v in (_json_or_none(line) for line in path.read_text(encoding="utf-8").splitlines())
        if v
    ]
