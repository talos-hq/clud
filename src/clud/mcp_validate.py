"""Up-front input validation for the clud MCP bridge (#2521).

Every check here is pure: it reads its arguments (and, for ``cwd``, the
filesystem) and never spawns a process or contacts the daemon. A tool runs
its checks first and returns the first failure, so a rejected call has no
side effect: no session, no worktree, no checkout claim.

A failure is a :class:`ToolError` whose payload is
``{ok: false, code, message, fix, candidates, error}``. ``fix`` is the
concrete next call; ``candidates`` lists valid values where they exist.
``error`` repeats ``message`` for readers written before this contract.
"""

from __future__ import annotations

import os
import re
import shlex
import shutil

#: Claude Code rejects a ``/goal`` condition longer than this (#2372, #2522).
GOAL_CONDITION_LIMIT = 4000

BACKENDS = ("claude", "codex")

#: Model families whose dotted or spaced spellings the harness rejects.
_FAMILY_TYPO = re.compile(
    r"^(?:claude[-_ ]?)?(opus|sonnet|haiku|fable)[-_ ]?(\d+)[. ](\d+)$", re.IGNORECASE
)
_MODEL_CHARS = re.compile(r"^[A-Za-z0-9._:/@\[\]-]+$")
_SESSION_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


class ToolError(Exception):
    """A correctable tool failure with its machine-readable payload."""

    def __init__(
        self,
        code: str,
        message: str,
        fix: str,
        candidates: list[str] | None = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.fix = fix
        self.candidates = list(candidates or [])

    def payload(self) -> dict:
        return {
            "ok": False,
            "code": self.code,
            "message": self.message,
            "fix": self.fix,
            "candidates": self.candidates,
            "error": self.message,
        }


def check_backend(backend: str, tool: str) -> ToolError | None:
    if backend in BACKENDS:
        return None
    return ToolError(
        "invalid_backend",
        f"unknown backend '{backend}' (expected 'claude' or 'codex')",
        f'Call {tool} again with backend="claude" or backend="codex".',
        list(BACKENDS),
    )


def check_prompt(prompt: str, tool: str) -> ToolError | None:
    if prompt and prompt.strip():
        return None
    return ToolError(
        "missing_argument",
        "prompt must not be empty: `prompt` is required",
        f'Pass the task text, e.g. {tool}(prompt="Summarize README.md", backend="claude").',
    )


def check_goal_length(prompt: str) -> ToolError | None:
    """A ``/goal`` prompt whose condition Claude Code would reject."""
    stripped = prompt.lstrip()
    if not stripped.startswith("/goal "):
        return None
    condition = stripped[len("/goal ") :].strip()
    if len(condition) <= GOAL_CONDITION_LIMIT:
        return None
    return ToolError(
        "goal_too_long",
        f"the /goal condition is {len(condition)} characters; Claude Code caps it at "
        f"{GOAL_CONDITION_LIMIT}",
        f"Shorten the condition to at most {GOAL_CONDITION_LIMIT} characters: write the "
        'full brief to a file and pass prompt="/goal Execute <path> exactly; constraints: '
        '<the hard constraints>", or use `clud do`, which compacts long goals.',
    )


def check_cwd(cwd: str, resolved: str) -> ToolError | None:
    if not cwd or os.path.isdir(resolved):
        return None
    here = os.getcwd()
    return ToolError(
        "cwd_not_found",
        f"cwd '{resolved}' is not a directory",
        f'Pass an existing absolute directory, e.g. cwd="{here}", or omit cwd to use it.',
        [here],
    )


def model_candidates(model: str) -> list[str]:
    match = _FAMILY_TYPO.match(model.strip())
    if not match:
        return []
    family, major, minor = match.group(1).lower(), match.group(2), match.group(3)
    return [f"claude-{family}-{major}-{minor}", family]


def check_model(model: str) -> ToolError | None:
    """Reject model spellings the harness is known to refuse.

    Ids that merely look unfamiliar pass: only the harness knows its full
    inventory, and a wrong guess here would block a valid new model.
    """
    if not model:
        return None
    candidates = model_candidates(model)
    if candidates:
        return ToolError(
            "unknown_model",
            f"model '{model}' is not a servable id (Claude Code rejects it as unrecognized_model)",
            f'Use the canonical id, e.g. model="{candidates[0]}", or the alias '
            f'model="{candidates[1]}".',
            candidates,
        )
    if not _MODEL_CHARS.match(model):
        return ToolError(
            "unknown_model",
            f"model '{model}' contains characters no model id uses",
            'Pass one model id without spaces, e.g. model="claude-opus-5-5" or model="opus".',
        )
    return None


def check_extra_flags(extra_flags: str) -> ToolError | None:
    if not extra_flags:
        return None
    try:
        tokens = shlex.split(extra_flags)
    except ValueError as exc:
        return ToolError(
            "invalid_extra_flags",
            f"extra_flags cannot be parsed: {exc}",
            "Balance every quote in extra_flags, e.g. extra_flags=\"--name 'my run'\".",
        )
    if tokens and not tokens[0].startswith("-"):
        return ToolError(
            "misplaced_argument",
            f"extra_flags starts with '{tokens[0]}', which is not a flag",
            "extra_flags carries global clud flags only, and global flags go before the "
            f"subcommand: remove '{tokens[0]}' and pass e.g. extra_flags=\"--model opus\".",
        )
    return None


def check_seconds(name: str, value: int, *, minimum: int, tool: str) -> ToolError | None:
    if isinstance(value, int) and value >= minimum:
        return None
    example = 0 if minimum == 0 else 300
    return ToolError(
        "invalid_argument",
        f"{name} must be an integer >= {minimum} (got {value!r})",
        f"Call {tool} again with {name}={example} or another value >= {minimum}.",
    )


def check_session_id(session_id: str, tool: str) -> ToolError | None:
    if not session_id:
        return ToolError(
            "missing_argument",
            "session_id is required",
            f'Pass the id session_start returned, e.g. {tool}(session_id="sess-...").',
        )
    if _SESSION_ID.match(session_id):
        return None
    return ToolError(
        "invalid_session_id",
        f"session_id '{session_id}' is not a session id",
        "Pass the `session_id` value session_start returned (it looks like sess-...).",
    )


def check_clud_bin(clud_bin: str) -> ToolError | None:
    if os.path.isabs(clud_bin) or shutil.which(clud_bin) is not None:
        return None
    return ToolError(
        "clud_not_found",
        f"clud binary '{clud_bin}' not found on PATH",
        "Install clud (`uv tool install clud`) or set CLUD_BIN to the absolute path of the "
        "clud executable in the MCP server's environment.",
    )


def daemon_unavailable(exc: BaseException) -> ToolError:
    return ToolError(
        "daemon_unavailable",
        f"could not discover clud daemon API: {exc}",
        "Run `clud daemon api-info --json` in a shell to see why the daemon is unreachable, "
        "fix that, then retry the same call.",
    )


def first_error(*checks: ToolError | None) -> ToolError | None:
    for check in checks:
        if check is not None:
            return check
    return None


def normalize(payload: dict) -> dict:
    """Give any tool payload the ``{ok, code, message, fix}`` shape."""
    if "ok" in payload:
        return payload
    if "error" not in payload:
        return {"ok": True, **payload}
    message = str(payload["error"])
    return {
        "ok": False,
        "code": payload.get("code") or "tool_failed",
        "message": message,
        "fix": payload.get("fix")
        or "Read `message`; if it names a session_id, probe it with session_status "
        "before retrying instead of relaunching the work.",
        "candidates": payload.get("candidates", []),
        **payload,
    }
