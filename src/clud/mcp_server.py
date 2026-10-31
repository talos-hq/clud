"""MCP bridge exposing the `clud` agent harness to MCP clients (e.g. Hermes Agent).

Runs as a stdio MCP server. It exposes three families of tools:

* ``run`` / ``dry_run`` — launch ``clud -p <prompt>`` as a child process and
  stream its output back (the original blocking surface).
* ``serve_start`` / ``serve_status`` / ``serve_stop`` — expose a local
  Anthropic-compatible Claude/Codex API gateway for clients such as Hermes.
* ``session_*`` — drive clud's durable ``/v1/sessions`` daemon API as an
  explicit session handle: create + submit a turn and get a ``session_id``
  back immediately, then probe, collect, wait on, or kill that session.

Why the session surface exists: a client-imposed tool-call deadline (Hermes
abandons a tool call after ``timeouts.tools.sequential_call``, 420 s by
default) cuts the *wait*, not the work. With ``run`` the caller loses the
handle while clud keeps going. With ``session_start`` the daemon — not the MCP
call — owns the run, so the id is a durable handle: probe it later, and a
caller that wants a synchronous answer can still block via ``session_wait``,
which returns the moment the turn seals instead of erroring at a deadline.

Process execution uses :mod:`asyncio` subprocess primitives (never the blocking
``subprocess`` module) so clud's stdout/stderr stream back incrementally without
a blocking read — the same non-blocking-streaming rationale that clud's
``running-process`` rule encodes for its own process tree.
"""

from __future__ import annotations

import asyncio
import codecs
import json
import os
import shlex
import shutil
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from contextlib import asynccontextmanager

import anyio
from mcp.server.mcpserver import MCPServer
from mcp.server.mcpserver.context import Context

from clud.serve_mcp import GatewayManager

#: Upper bound on harness output returned to the client, in characters. Hermes'
#: own MCP client hard-caps a single result at 2,000,000 chars; stay well under
#: it. Log notifications are additionally capped so a chatty harness cannot
#: flood the client's log stream.
_RESULT_CHAR_CAP = 1_000_000
_LOG_LINE_CAP = 2_000

_gateway = GatewayManager()


@asynccontextmanager
async def _gateway_lifespan(app):
    del app
    try:
        yield {}
    finally:
        with anyio.CancelScope(shield=True):
            await _gateway.stop()


server = MCPServer(
    name="clud",
    title="clud — agent harness runner and local model gateway",
    lifespan=_gateway_lifespan,
    instructions=(
        "To delegate work to a Codex (OpenAI) sub-agent, call session_start with "
        'backend="codex" and a self-contained prompt and cwd, then session_wait or '
        "session_result for its result. Use run for short blocking jobs. Pass model "
        "to pick a specific Codex model. Check board_read periodically for messages "
        "from clud, Hermes or other agents, and answer with board_post."
    ),
    description=(
        "Run the clud agent harness (Claude Code / Codex in YOLO mode) on a "
        "prompt — either as a blocking child process, or as a durable session "
        "you can probe, wait on, and collect. Discover and manage local Claude/Codex "
        "Anthropic Messages endpoints with serve_start, serve_status, and serve_stop."
    ),
)


def _build_argv(prompt: str, backend: str, model: str, extra_flags: str) -> list[str]:
    """Assemble the clud argv for a prompt run."""
    argv = [os.environ.get("CLUD_BIN", "clud"), "-p", prompt]
    if backend == "codex":
        argv.append("--codex")
    elif backend == "claude":
        argv.append("--claude")
    if model:
        argv.extend(("--model", model))
    if extra_flags:
        argv.extend(shlex.split(extra_flags))
    return argv


def _resolve_cwd(raw: str, default: str) -> str:
    """Expand ``~``/env vars and make a cwd absolute (relative to the bridge's cwd).

    The daemon's ``/v1/sessions`` contract only accepts an absolute, existing
    cwd, so a bare ``~`` or relative path must be normalised before it is sent.
    """
    value = (raw or "").strip()
    if not value:
        return default
    value = os.path.expandvars(os.path.expanduser(value))
    if not os.path.isabs(value):
        value = os.path.join(os.getcwd(), value)
    # Normalise separators, not just make it absolute. `~/work` and `rel/dir`
    # expand to `C:\\Users\\x/work` on Windows -- a mixed-separator path that
    # most APIs tolerate and no reader or comparison does. The docstring above
    # already promised a normalised value.
    return os.path.normpath(value)


async def _stream_and_capture(proc: asyncio.subprocess.Process, ctx: Context) -> str:
    """Pump stdout+stderr to the client and accumulate the full transcript."""
    parts: list[str] = []
    total = 0
    log_count = 0

    async def _pump(stream: asyncio.StreamReader | None, label: str) -> None:
        nonlocal total, log_count
        if stream is None:
            return
        # One incremental decoder and line carry per stream, so a character or
        # line split across reads is not mangled (#1916).
        decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        carry = ""
        logging = True
        while True:
            chunk = await stream.read(8192)
            text = decoder.decode(chunk, final=not chunk)
            if total < _RESULT_CHAR_CAP:
                parts.append(text)
                total += len(text)
            carry += text
            lines = carry.split("\n")
            carry = "" if not chunk else lines.pop()
            for line in lines:
                # Stop forwarding logs at the cap or on a logging failure, but
                # keep reading until EOF so the child's pipe never fills and
                # blocks it (#1914).
                if not logging or log_count >= _LOG_LINE_CAP or not line.strip():
                    continue
                log_count += 1
                try:
                    await ctx.info(f"[{label}] {line.rstrip()}")
                except Exception:  # logging must never kill a run
                    logging = False
            if not chunk:
                break

    await asyncio.gather(_pump(proc.stdout, "stdout"), _pump(proc.stderr, "stderr"))
    return "".join(parts)[:_RESULT_CHAR_CAP]


@server.tool()
async def run(
    prompt: str,
    ctx: Context,
    backend: str = "claude",
    cwd: str = "",
    model: str = "",
    timeout: int = 1800,
    extra_flags: str = "",
) -> str:
    """Run `clud -p <prompt>` and return the harness's streamed output.

    Blocking, and the child is killed when `timeout` elapses, so prefer
    `session_start` / `session_wait` for anything that can run past a few
    minutes: those are owned by the clud daemon and survive a client-side
    tool-call deadline (a `session_id` keeps the work reachable even when the
    caller gives up waiting).

    Args:
        prompt: The prompt passed to `clud -p`. This is a full autonomous agent
            run: clud launches Claude Code (default) or Codex in YOLO mode and
            lets it work on the prompt (e.g. "write a 4-paragraph song").
        backend: Which harness clud drives: "claude" (default) or "codex".
        cwd: Working directory for the run (defaults to clud's current dir).
        model: Optional model override passed through as `--model <model>`.
        timeout: Maximum seconds to wait for the run before terminating it.
        extra_flags: Optional space-separated extra clud flags to pass through.
    """
    clud_bin = os.environ.get("CLUD_BIN", "clud")
    if not os.path.isabs(clud_bin) and shutil.which(clud_bin) is None:
        return f"error: clud binary '{clud_bin}' not found on PATH"

    if backend not in ("claude", "codex"):
        return f"error: unknown backend '{backend}' (expected 'claude' or 'codex')"

    argv = _build_argv(prompt, backend, model, extra_flags)
    workdir = cwd or None
    if workdir is not None:
        workdir = _resolve_cwd(workdir, "")
        if not os.path.isdir(workdir):
            return f"error: cwd '{workdir}' is not a directory"

    try:
        await ctx.info(f"running: {shlex.join(argv)}")
    except Exception:
        pass

    try:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            cwd=workdir,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
    except FileNotFoundError:
        return f"error: could not start '{clud_bin}'"
    except OSError as exc:
        return f"error: failed to launch clud: {exc}"

    try:
        output = await asyncio.wait_for(_stream_and_capture(proc, ctx), timeout=timeout)
    except asyncio.TimeoutError:
        try:
            proc.kill()
        except ProcessLookupError:
            pass
        await proc.wait()
        return f"error: clud run exceeded {timeout}s timeout; process terminated"

    rc = await proc.wait()
    output += f"\n\n[clud exited with code {rc}]"
    return output


# ---------------------------------------------------------------------------
# JSON translation layer — clud's machine-readable surfaces.
#
# clud natively normalises its backend JSON into a typed loopback HTTP API
# (`clud daemon api-info --json` → base_url + bearer token). Claude Code runs
# headless with `--output-format stream-json` (NDJSON); Codex runs `codex exec
# --json`. The daemon captures those lines and re-emits them as bounded,
# cursor-addressable `Event` records. The session tools below drive that API
# and hand the client a clean, structured handle instead of a raw stream.
# ---------------------------------------------------------------------------

_HTTP_TIMEOUT = 20  # seconds per HTTP round-trip
#: The daemon refuses `limit` above this (`MAX_EVENTS_PAGE`) — page accordingly.
_EVENT_PAGE_LIMIT = 128
#: Bound on events accumulated by one result/collect call, so a long run cannot
#: pull an unbounded transcript into a single tool result.
_RESULT_EVENT_CAP = 2048
#: Poll cadence for `session_wait`. One second matches the daemon's own tick and
#: keeps a wait cheap (a local loopback GET) while still returning promptly.
_WAIT_TICK_SECONDS = 1.0
#: Session states in which no turn can still be running.
_TERMINAL_SESSION_STATES = frozenset({"idle", "failed", "terminated"})
#: Turn states that mean "this turn is over, for good or ill".
_TERMINAL_TURN_STATES = frozenset({"completed", "failed", "interrupted", "killed"})


class SessionError(RuntimeError):
    """A session lifecycle call failed, carrying the handle when one exists."""

    def __init__(self, message: str, session_id: str = "") -> None:
        super().__init__(message)
        self.session_id = session_id

    def payload(self) -> dict:
        out: dict = {"error": str(self)}
        if self.session_id:
            out["session_id"] = self.session_id
        return out


def _dump(payload: dict) -> str:
    """Serialize a tool result. Never raises on odd payloads."""
    try:
        return json.dumps(payload, indent=2, default=str)
    except (TypeError, ValueError):
        return json.dumps({"error": "result was not JSON-serializable"})


def _clud_bin() -> str:
    return os.environ.get("CLUD_BIN", "clud")


async def _discover_api() -> tuple[str, str]:
    """Return the daemon's (base_url, bearer_token) from `clud daemon api-info`."""
    proc = await asyncio.create_subprocess_exec(
        _clud_bin(),
        "daemon",
        "api-info",
        "--json",
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    out, err = await proc.communicate()
    if proc.returncode != 0:
        raise RuntimeError(f"clud daemon api-info failed: {err.decode().strip()}")
    try:
        doc = json.loads(out.decode())
        return doc["base_url"], doc["token"]
    except (KeyError, json.JSONDecodeError) as exc:
        raise RuntimeError(f"unexpected api-info payload: {exc}") from exc


def _http_json(
    base: str,
    token: str,
    method: str,
    path: str,
    body=None,
    headers: dict | None = None,
):
    """Blocking JSON HTTP call against the loopback daemon API."""
    req = urllib.request.Request(base + path, method=method)
    req.add_header("Authorization", "Bearer " + token)
    for name, value in (headers or {}).items():
        req.add_header(name, value)
    data = None
    if body is not None:
        req.add_header("Content-Type", "application/json")
        data = json.dumps(body).encode()
    try:
        with urllib.request.urlopen(req, data=data, timeout=_HTTP_TIMEOUT) as resp:
            raw = resp.read().decode()
            return resp.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode()
        try:
            return exc.code, json.loads(raw)
        except json.JSONDecodeError:
            return exc.code, {"error": raw}


async def _api(
    base: str, token: str, method: str, path: str, body=None, headers=None
) -> tuple[int, object]:
    """Off-loop wrapper around :func:`_http_json`."""
    return await asyncio.to_thread(_http_json, base, token, method, path, body, headers)


def _extract_text(events: list) -> str:  # noqa: C901
    """Best-effort plain-text answer assembled from normalised backend JSONL."""
    parts: list[str] = []
    for ev in events:
        if ev.get("kind") != "raw_jsonl":
            continue
        data = ev.get("data")
        if not isinstance(data, dict):
            continue
        line = data.get("line")
        if not isinstance(line, str):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = obj.get("type")
        if kind == "result":
            res = obj.get("result")
            if isinstance(res, str):
                parts.append(res)
            elif isinstance(res, dict):
                parts.append(res.get("result") or res.get("text") or "")
        elif kind == "assistant":
            for item in (obj.get("message") or {}).get("content", []) or []:
                if isinstance(item, dict) and item.get("type") == "text":
                    parts.append(item.get("text", ""))
    # A headless run's final `result` line echoes the last assistant message, so
    # the raw concatenation duplicates the answer. Collapse adjacent identical
    # blocks (whitespace-insensitive) without touching genuinely repeated prose
    # that is separated by other text.
    deduped: list[str] = []
    for part in parts:
        if not part:
            continue
        if deduped and deduped[-1].strip() == part.strip():
            continue
        deduped.append(part)
    return "\n".join(deduped).strip()


def _turn_entry(record: dict, turn_id: str = "") -> dict:
    """The requested turn record, else the session's most recent one."""
    turns = record.get("turns")
    if not isinstance(turns, list) or not turns:
        return {}
    if turn_id:
        for entry in turns:
            if isinstance(entry, dict) and entry.get("id") == turn_id:
                return entry
        return {}
    last = turns[-1]
    return last if isinstance(last, dict) else {}


def _turn_state(record: dict, turn_id: str = "") -> str:
    """State of the requested (or latest) turn, or "" when not recorded yet."""
    value = _turn_entry(record, turn_id).get("state")
    return value if isinstance(value, str) else ""


def _turn_done(record: dict, turn_id: str = "") -> bool:
    """Whether the requested turn has sealed.

    A turn that has not been recorded yet is NOT treated as done while the
    session is still live: `session_start` submits the turn and returns, so a
    caller may probe in the window before the daemon registers the turn. Only a
    dead session (failed/terminated) seals an unrecorded turn.
    """
    state = _turn_state(record, turn_id)
    if state in _TERMINAL_TURN_STATES:
        return True
    if state:
        return False
    return record.get("state") in ("failed", "terminated")


def _turn_summary(record: dict, turn_id: str = "") -> dict:
    """Small, stable view of a turn record (no transcript embedded)."""
    entry = _turn_entry(record, turn_id)
    return {
        key: entry.get(key)
        for key in ("id", "state", "disposition", "generation", "completed_at_ms")
        if key in entry
    }


def _elapsed_s(record: dict) -> float:
    """Seconds since the session was created (0.0 when unreadable)."""
    created = record.get("created_at_ms")
    if not isinstance(created, (int, float)):
        return 0.0
    return max(0.0, time.time() - float(created) / 1000.0)


async def _session_record(base: str, token: str, session_id: str) -> dict:
    """GET one session record, or raise :class:`SessionError`."""
    status, record = await _api(base, token, "GET", f"/v1/sessions/{session_id}")
    if status != 200 or not isinstance(record, dict):
        raise SessionError(f"session lookup failed ({status}): {record}", session_id=session_id)
    return record


async def _collect_events(
    base: str, token: str, session_id: str, after: int = 0, turn_id: str = ""
) -> dict:
    """Page the cursor-addressable event log and assemble text for a turn.

    Pages with `after=<cursor>` so a caller can continue from `next_cursor`
    without re-reading (and without losing output to the daemon's bounded event
    retention, which surfaces as `retention_gap`).
    """
    cursor = max(0, int(after or 0))
    events: list = []
    retention_gap = False
    while len(events) < _RESULT_EVENT_CAP:
        status, page = await _api(
            base,
            token,
            "GET",
            f"/v1/sessions/{session_id}/events?after={cursor}&limit={_EVENT_PAGE_LIMIT}",
        )
        if status != 200 or not isinstance(page, dict):
            raise SessionError(f"event fetch failed ({status}): {page}", session_id=session_id)
        page_events = page.get("events")
        if not isinstance(page_events, list) or not page_events:
            break
        retention_gap = retention_gap or bool(page.get("retention_gap"))
        events.extend(page_events)
        next_cursor = page.get("next_cursor")
        cursor = next_cursor if isinstance(next_cursor, int) else cursor + len(page_events)
        if len(page_events) < _EVENT_PAGE_LIMIT:
            break
    selected = [ev for ev in events if not turn_id or ev.get("turn_id") == turn_id]
    return {
        "events": selected,
        "text": _extract_text(selected),
        "next_cursor": cursor,
        "retention_gap": retention_gap,
    }


async def _start_turn(
    base: str,
    token: str,
    *,
    prompt: str,
    backend: str,
    cwd: str,
    model: str,
    safe: bool,
    name: str,
    resume_session_id: str,
) -> tuple[str, str]:
    """Create (or resume) a session and submit one turn; return its ids.

    The turn POST is idempotent (`Idempotency-Key`), so a client retry after a
    dropped response replays the same turn instead of starting a second run.
    """
    workdir = _resolve_cwd(cwd, os.getcwd())
    if not os.path.isdir(workdir):
        raise SessionError(f"cwd '{workdir}' is not a directory")

    session_id = resume_session_id
    if not session_id:
        body: dict = {"backend": backend, "cwd": workdir, "safe": bool(safe)}
        if name:
            body["name"] = name
        if model:
            body["model"] = model
        status, created = await _api(base, token, "POST", "/v1/sessions", body)
        if status != 201 or not isinstance(created, dict) or not created.get("id"):
            raise SessionError(f"create session failed ({status}): {created}")
        session_id = str(created["id"])

    status, turn = await _api(
        base,
        token,
        "POST",
        f"/v1/sessions/{session_id}/turns",
        {"message": prompt, "interrupt_running": False},
        {"Idempotency-Key": uuid.uuid4().hex},
    )
    if status not in (200, 202) or not isinstance(turn, dict) or not turn.get("turn_id"):
        raise SessionError(f"submit turn failed ({status}): {turn}", session_id=session_id)
    return session_id, str(turn["turn_id"])


async def _wait_for_turn(
    base: str, token: str, session_id: str, turn_id: str, wait_seconds: float
) -> tuple[dict, float]:
    """Poll a turn until it seals or the wait budget runs out.

    Returns ``(record, waited_s)``. It NEVER raises for "not finished yet" and
    never interrupts the turn: running out of budget is a normal outcome, and
    the caller keeps the session handle.
    """
    started = time.monotonic()
    deadline = started + max(0.0, float(wait_seconds or 0.0))
    while True:
        record = await _session_record(base, token, session_id)
        if _turn_done(record, turn_id):
            return record, time.monotonic() - started
        if time.monotonic() >= deadline:
            return record, time.monotonic() - started
        await asyncio.sleep(_WAIT_TICK_SECONDS)


async def _snapshot(base: str, token: str, session_id: str, turn_id: str = "") -> dict:
    """State probe payload shared by status/result/wait."""
    record = await _session_record(base, token, session_id)
    return {
        "session_id": session_id,
        "state": record.get("state"),
        "current_turn_id": record.get("current_turn_id"),
        "done": _turn_done(record, turn_id),
        "elapsed_s": round(_elapsed_s(record), 1),
        "turn": _turn_summary(record, turn_id),
        "last_error": record.get("last_error"),
        "_record": record,
    }


async def _result_payload(
    base: str,
    token: str,
    session_id: str,
    *,
    turn_id: str = "",
    after: int = 0,
    include_events: bool = False,
    waited_s: float | None = None,
) -> dict:
    """Assemble the structured result for a session (safe to call mid-run)."""
    snapshot = await _snapshot(base, token, session_id, turn_id)
    collected = await _collect_events(base, token, session_id, after=after, turn_id=turn_id)
    payload = {
        "session_id": session_id,
        "turn_id": turn_id or snapshot["turn"].get("id") or "",
        "state": snapshot["state"],
        "done": snapshot["done"],
        "elapsed_s": snapshot["elapsed_s"],
        "turn": snapshot["turn"],
        "text": collected["text"],
        "next_cursor": collected["next_cursor"],
        "retention_gap": collected["retention_gap"],
        "last_error": snapshot["last_error"],
    }
    if waited_s is not None:
        payload["waited_s"] = round(waited_s, 1)
    if include_events:
        payload["events"] = collected["events"]
    if not snapshot["done"]:
        payload["note"] = (
            "STILL RUNNING - the turn has not sealed. Keep the session_id and "
            "probe again (session_result) or wait (session_wait); do not relaunch "
            "the work."
        )
    return payload


async def _resolve_and_start(
    prompt: str,
    backend: str,
    cwd: str,
    model: str,
    safe: bool,
    name: str,
    resume_session_id: str,
) -> tuple[str, str, str, str]:
    """Validate + discover + submit. Returns (base, token, session_id, turn_id)."""
    if backend not in ("claude", "codex"):
        raise SessionError(f"unknown backend '{backend}' (expected 'claude' or 'codex')")
    if not prompt.strip():
        raise SessionError("prompt must not be empty")
    try:
        base, token = await _discover_api()
    except Exception as exc:  # discovery must never mask a usable handle
        raise SessionError(f"could not discover clud daemon API: {exc}") from exc
    session_id, turn_id = await _start_turn(
        base,
        token,
        prompt=prompt,
        backend=backend,
        cwd=cwd,
        model=model,
        safe=safe,
        name=name,
        resume_session_id=resume_session_id,
    )
    return base, token, session_id, turn_id


@server.tool()
async def session_start(
    prompt: str,
    ctx: Context,
    backend: str = "claude",
    cwd: str = "",
    model: str = "",
    safe: bool = False,
    name: str = "",
    resume_session_id: str = "",
    wait_seconds: int = 0,
) -> str:
    """Start a clud run as a DURABLE SESSION and return its handle immediately.

    Use this instead of `run` for anything that may outlast a tool-call
    deadline: the clud daemon owns the run, so the returned `session_id` keeps
    the work reachable (probe with `session_status`, collect with
    `session_result`, block with `session_wait`) even if this call's client
    stops waiting. Nothing here kills the run.

    `wait_seconds` is the optional synchronous shortcut: 0 (default) returns the
    handle the instant the turn is submitted; a positive value blocks — with an
    early return the moment the turn seals — up to that many seconds, then
    returns the partial result with `done: false`. Keep a positive value below
    the client's tool-call deadline (Hermes: `timeouts.tools.sequential_call`,
    420 s by default) or the client will abandon the wait while the turn keeps
    running. For longer work, return immediately and wait out-of-band.

    Args:
        prompt: The task prompt. This is a full autonomous agent run under
            `clud` (Claude Code by default, Codex with backend="codex") in YOLO
            mode unless `safe` is set; write it self-contained.
        backend: "claude" (default) or "codex".
        cwd: Absolute path (or `~`/relative, normalised) to run in. Defaults to
            the bridge's working directory.
        model: Optional model override (`--model`), e.g. "claude-opus-5" or,
            with backend="codex", "gpt-6-astra".
        safe: Create the session in non-YOLO ("safe") mode.
        name: Optional human label for the session.
        resume_session_id: Continue an existing session instead of creating a
            new one (multi-turn). The stored settings/cwd are reused.
        wait_seconds: Seconds to block waiting for the turn before returning
            the handle (0 = return immediately, always async).
    """
    try:
        base, token, session_id, turn_id = await _resolve_and_start(
            prompt, backend, cwd, model, safe, name, resume_session_id
        )
    except SessionError as exc:
        return _dump(exc.payload())

    if not wait_seconds:
        return _dump(
            {
                "session_id": session_id,
                "turn_id": turn_id,
                "state": "submitted",
                "done": False,
                "waited_s": 0.0,
                "note": (
                    "Turn submitted; the daemon owns it. Probe with "
                    "session_status/session_result, or wait with session_wait."
                ),
            }
        )

    try:
        record, waited_s = await _wait_for_turn(base, token, session_id, turn_id, wait_seconds)
        payload = await _result_payload(base, token, session_id, turn_id=turn_id, waited_s=waited_s)
    except SessionError as exc:
        return _dump(exc.payload())
    del record
    return _dump(payload)


@server.tool()
async def session_status(session_id: str, ctx: Context) -> str:
    """Probe a clud session: state, active turn, and whether it has sealed.

    Cheap, read-only, and safe to call as often as you like — but do not build
    a model-side sleep/poll loop with it: use `session_wait` to block with an
    early return, or `session_start` + an out-of-band waiter to be told when the
    run is done.

    Args:
        session_id: The id returned by `session_start` (`sess-...`).
    """
    try:
        base, token = await _discover_api()
        snapshot = await _snapshot(base, token, session_id)
    except SessionError as exc:
        return _dump(exc.payload())
    except Exception as exc:
        return _dump({"error": f"could not discover clud daemon API: {exc}"})
    snapshot.pop("_record", None)
    return _dump(snapshot)


@server.tool()
async def session_result(
    session_id: str,
    ctx: Context,
    turn_id: str = "",
    after: int = 0,
    include_events: bool = False,
) -> str:
    """Collect a session's answer so far — safe to call while it is running.

    `text` is the assembled plain-text answer (backend `result` / `assistant`
    text), not the raw stream. When `done` is false the text is partial: keep
    the handle and probe again rather than relaunching the work.

    Args:
        session_id: The id returned by `session_start`.
        turn_id: Restrict text/events to one turn (e.g. the `turn_id` from
            `session_start`); empty means the whole session.
        after: Event cursor to continue from (use the `next_cursor` of a
            previous call). Events are retained on a bounded ring, so poll
            with cursors rather than expecting the whole log at the end; when
            `retention_gap` is true some early events have already rolled off.
        include_events: Include the normalised event records in the result
            (large; off by default).
    """
    try:
        base, token = await _discover_api()
        payload = await _result_payload(
            base,
            token,
            session_id,
            turn_id=turn_id,
            after=after,
            include_events=include_events,
        )
    except SessionError as exc:
        return _dump(exc.payload())
    except Exception as exc:
        return _dump({"error": f"could not discover clud daemon API: {exc}"})
    return _dump(payload)


@server.tool()
async def session_wait(
    session_id: str,
    ctx: Context,
    wait_seconds: int = 300,
    turn_id: str = "",
    after: int = 0,
    include_events: bool = False,
) -> str:
    """Block until a session's turn seals, or the wait budget expires.

    This is the synchronous-but-cheap path: one call, no model-side polling, and
    it returns THE MOMENT the turn finishes rather than burning the whole
    budget. When the budget runs out it returns `done: false` with the partial
    text — never an error, and the run keeps going on the daemon.

    Keep `wait_seconds` comfortably below the client's tool-call deadline
    (Hermes: `timeouts.tools.sequential_call`, 420 s by default) so the result
    comes back as a result rather than a client-side abandonment.

    Args:
        session_id: The id returned by `session_start`.
        wait_seconds: Maximum seconds to block (default 300).
        turn_id: Turn to wait for; empty means the session's latest turn.
        after: Event cursor to start collecting text from.
        include_events: Include the normalised event records in the result.
    """
    try:
        base, token = await _discover_api()
        record, waited_s = await _wait_for_turn(base, token, session_id, turn_id, wait_seconds)
        del record
        payload = await _result_payload(
            base,
            token,
            session_id,
            turn_id=turn_id,
            after=after,
            include_events=include_events,
            waited_s=waited_s,
        )
    except SessionError as exc:
        return _dump(exc.payload())
    except Exception as exc:
        return _dump({"error": f"could not discover clud daemon API: {exc}"})
    return _dump(payload)


@server.tool()
async def session_kill(session_id: str, ctx: Context) -> str:
    """Terminate a clud session (and any turn it is running).

    Args:
        session_id: The id returned by `session_start`.
    """
    try:
        base, token = await _discover_api()
    except Exception as exc:
        return _dump({"error": f"could not discover clud daemon API: {exc}"})
    status, payload = await _api(base, token, "DELETE", f"/v1/sessions/{session_id}")
    if status != 200:
        return _dump({"error": f"kill failed ({status}): {payload}", "session_id": session_id})
    return _dump({"session_id": session_id, "status": "terminated"})


#: Where this agent's next ``board_read`` resumes; per bridge process.
_board_cursor = 0


def _board_identity() -> str:
    """This agent's bulletin-board address: ``CLUD_BOARD_ID`` or the session id."""
    return os.environ.get("CLUD_BOARD_ID") or os.environ.get("CLUD_SESSION_ID", "")


@server.tool()
async def board_whoami() -> str:
    """Return this agent's bulletin-board address, which others post to."""
    return _dump({"id": _board_identity()})


@server.tool()
async def board_post(to: str, body: str) -> str:
    """Post a message on clud's agent bulletin board.

    Args:
        to: The recipient's board id (see `board_whoami`), or "all".
        body: The message text.
    """
    try:
        base, token = await _discover_api()
    except Exception as exc:
        return _dump({"error": f"could not discover clud daemon API: {exc}"})
    message = {"to": to, "from": _board_identity(), "body": body}
    status, payload = await _api(base, token, "POST", "/v1/board/messages", message)
    if status != 201:
        return _dump({"error": f"post failed ({status}): {payload}"})
    return _dump(payload)


@server.tool()
async def board_read(after: int = -1) -> str:
    """Read new bulletin-board messages addressed to this agent or to "all".

    Each call returns only what arrived since the previous call. Act on any
    instructions from clud, Hermes or the user, and reply with `board_post`.

    Args:
        after: Cursor to read from; -1 (default) continues from the last read.
    """
    global _board_cursor
    try:
        base, token = await _discover_api()
    except Exception as exc:
        return _dump({"error": f"could not discover clud daemon API: {exc}"})
    cursor = _board_cursor if after < 0 else after
    query = urllib.parse.urlencode({"to": _board_identity(), "after": cursor})
    status, payload = await _api(base, token, "GET", f"/v1/board/messages?{query}")
    if status != 200 or not isinstance(payload, dict):
        return _dump({"error": f"read failed ({status}): {payload}"})
    _board_cursor = int(payload.get("next_cursor", cursor))
    return _dump(payload)


@server.tool()
async def dry_run(
    prompt: str,
    ctx: Context,
    backend: str = "claude",
    model: str = "",
    extra_flags: str = "",
) -> str:
    """Resolve `clud -p <prompt>` to its LaunchPlan JSON without executing it.

    Args:
        prompt: The prompt that would be passed to `clud -p`.
        backend: "claude" (default) or "codex".
        model: Optional `--model` override.
        extra_flags: Optional space-separated extra clud flags.
    """
    clud_bin = _clud_bin()
    if not os.path.isabs(clud_bin) and shutil.which(clud_bin) is None:
        return f"error: clud binary '{clud_bin}' not found on PATH"
    if backend not in ("claude", "codex"):
        return f"error: unknown backend '{backend}' (expected 'claude' or 'codex')"
    argv = [clud_bin, "--dry-run", "-p", prompt]
    if backend == "codex":
        argv.append("--codex")
    if model:
        argv.extend(("--model", model))
    if extra_flags:
        argv.extend(shlex.split(extra_flags))
    proc = await asyncio.create_subprocess_exec(
        *argv,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    out, err = await proc.communicate()
    raw = out.decode("utf-8", "replace")
    start, end = raw.find("{"), raw.rfind("}")
    if start != -1 and end > start:
        try:
            plan = json.loads(raw[start : end + 1])
            return json.dumps(plan, indent=2)
        except json.JSONDecodeError:
            pass
    return raw + (("\n" + err.decode("utf-8", "replace")) if err else "")


@server.tool()
async def run_json(
    prompt: str,
    ctx: Context,
    backend: str = "claude",
    cwd: str = "",
    model: str = "",
    safe: bool = False,
    timeout: int = 1800,
    resume_session_id: str = "",
) -> str:
    """Run a headless clud turn and return the translated, structured JSON.

    Blocking convenience wrapper: `session_start` + `session_wait` + the
    collected result, with the same JSON shape it always returned. Prefer the
    `session_*` tools when the run may outlive the caller's tool-call deadline,
    since this one can only report what it managed to wait for.

    Args:
        prompt: The task message for the turn.
        backend: "claude" (default) or "codex".
        cwd: Working directory for the run (defaults to the bridge's cwd).
        model: Optional model override persisted on the session.
        safe: If true, create the session in `--safe` (non-YOLO) mode.
        timeout: Maximum seconds to wait for the turn to finish.
        resume_session_id: Reuse an existing logical session instead of
            creating a new one (enables multi-turn conversations).
    """
    try:
        base, token, session_id, turn_id = await _resolve_and_start(
            prompt, backend, cwd, model, safe, "", resume_session_id
        )
        record, waited_s = await _wait_for_turn(base, token, session_id, turn_id, timeout)
    except SessionError as exc:
        return _dump(exc.payload())

    if not _turn_done(record, turn_id):
        return (
            f"error: turn did not finish within {timeout}s "
            f"(last state '{record.get('state')}'); session_id={session_id}, "
            f"turn_id={turn_id}. The daemon still owns the run - probe it with "
            "session_result instead of relaunching."
        )

    collected = await _collect_events(base, token, session_id, turn_id=turn_id)
    result = {
        "session_id": session_id,
        "turn_id": turn_id,
        "state": record.get("state"),
        "text": collected["text"],
        "events": collected["events"],
    }
    del waited_s
    return json.dumps(result, indent=2)


@server.tool()
async def serve_start(backend: str = "codex", openrouter: bool = False, port: int = 0) -> str:
    """Start a local Claude/Codex model API and return connection details for Hermes.

    Returns base_url, models_url, messages_url, auth_header and client_token.
    This exposes the Anthropic Messages protocol, including streaming and tools;
    it does not expose OpenAI Chat Completions or Responses endpoints. The token
    authenticates local clients; keep it private. Upstream credentials are never
    returned. An identical repeat reuses the running gateway. This MCP server
    owns the gateway and stops it on disconnect; use serve_stop to stop earlier.

    Args:
        backend: "codex" (default, clud auth login codex or OPENAI_API_KEY) or
            "claude" (ANTHROPIC_API_KEY).
        openrouter: Add the route configured by clud auth login openrouter.
        port: Loopback port; 0 chooses an available port.
    """
    return _dump(await _gateway.start(_clud_bin(), backend, openrouter, port))


@server.tool()
async def serve_status() -> str:
    """Discover the MCP-owned gateway's URL, protocol and client authentication.

    Returns running=false when no managed gateway is alive. Use serve_start to
    create one. No connection to an upstream model is required for this probe.
    """
    return _dump(_gateway.status())


@server.tool()
async def serve_stop() -> str:
    """Stop and reap the MCP-owned model gateway; safe to repeat."""
    return _dump(await _gateway.stop())


def main() -> None:
    """Run the bridge over stdio until the client disconnects."""
    server.run("stdio")


if __name__ == "__main__":
    main()
