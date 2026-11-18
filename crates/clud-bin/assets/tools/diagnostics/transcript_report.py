#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# managed-by: clud
"""transcript_report.py — privacy-preserving report on a Claude session transcript.

Usage:
  transcript_report.py <transcript.jsonl> [--json] [--burst-threshold N]
                       [--jump-threshold TOKENS] [--context-floor TOKENS]
                       [--state-dir DIR]

Reads one Claude Code session transcript (JSONL) and reports what drove its
context usage (issue #1276):

* responses: rows collapsed by assistant ``message.id``. Claude Code repeats
  a response's ``usage`` on every row it splits that response into, so the
  output tokens are counted once per message id, not once per row.
* repeated tool-call bursts: identical tool calls (same tool name and the
  same input) inside one response, with the total copy count and the
  longest consecutive streak.
* compaction boundaries (``compact_boundary`` system rows), context jumps
  between consecutive responses, and terminal context errors.
* the history premium: how many responses held a context above
  ``--context-floor`` (default 100k) and how many tokens those responses
  re-sent above that floor. A tool result is paid for once, but the history
  behind it is re-sent on every later request; on 2026-11-07 five long-lived
  manager sessions held half of the fleet's 1.33B cache-read tokens while
  their tool output was under 1 MB each. The premium is derived from usage
  integers only and is the number to look at before blaming a tool.
* tool-result byte totals and the largest single result.
* the effective max-context setting, which a transcript does not record. It
  comes from clud's per-session launch-context record (#1675), joined by a
  hashed session id (see ``launch_context_key``) from
  ``<state>/launch-context/<key>.json``; ``<state>`` is ``--state-dir``, else
  ``$CLUD_DAEMON_STATE_DIR``, else ``~/.clud/state``. Without a record the
  field is reported as unavailable instead of being guessed. Per-response
  token and compaction numbers always come from the transcript.

Privacy: the report never contains prompts, commands, tool inputs, tool
output or the session id. Tool calls are identified by tool name plus a
fingerprint keyed with a random per-invocation salt, so a fingerprint can
only be compared within one report. The tool writes no files.

Run through clud's `tool run` subcommand:
  "$CLUD_EXE" tool run diagnostics/transcript_report.py ~/.claude/projects/<p>/<id>.jsonl

Exit codes:
  0  report printed
  1  usage error, or the transcript could not be read
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import os
import re
import secrets
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# Bounded stdout (#1676, docs/architecture/bounded-output.md): a result over
# STDOUT_CAP_BYTES prints its head, a truncation notice, and the path of an
# artifact under the clud tmp dir that holds the full text.
STDOUT_CAP_BYTES = 32 * 1024


def _tool_output_dir() -> Path | None:
    override = os.environ.get("CLUD_TOOL_OUTPUT_DIR")
    if override:
        return Path(override)
    home = os.environ.get("USERPROFILE") or os.environ.get("HOME")
    return Path(home) / ".clud" / "tmp" / "tool-output" if home else None


def emit_bounded(text: str, name: str, cap: int = STDOUT_CAP_BYTES) -> None:
    data = text.encode("utf-8")
    if len(data) <= cap:
        print(text)
        return
    try:
        out_dir = _tool_output_dir()
        if out_dir is None:
            raise OSError("no home directory")
        out_dir.mkdir(parents=True, exist_ok=True)
        stamp = time.strftime("%Y%m%dT%H%M%S")
        artifact = out_dir / f"{name}-{stamp}-{os.getpid()}.txt"
        artifact.write_bytes(data)
        where = f"full output: {artifact}"
    except OSError as exc:
        where = f"full output could not be saved ({exc})"
    print(data[:cap].decode("utf-8", "ignore"))
    print(f"[TRUNCATED: showed {cap} of {len(data)} bytes; {where}]")


DEFAULT_BURST_THRESHOLD = 20
DEFAULT_JUMP_THRESHOLD = 50_000
#: Context at or above which a response is charged for its history. 100k is
#: well under every window the fleet launches with (200k..786k), so a session
#: that stays below it is not paying a measurable premium, and one that lives
#: above it is.
DEFAULT_CONTEXT_FLOOR = 100_000
#: Premium at which the text report names the compaction lever. Below this the
#: summary line is information; above it the session is the problem.
PREMIUM_HINT_TOKENS = 5_000_000

# Must match `launch_context::HASH_DOMAIN` in crates/clud-bin/src/launch_context.rs.
LAUNCH_CONTEXT_HASH_DOMAIN = "clud-launch-context-v1\0"
UNAVAILABLE = "unavailable: a transcript does not record the child environment"
_SOURCES = {"ambient", "catalog", "served", "unset", "unknown"}
_ROUTES = {"direct", "unified_gateway", "codex_bridge", "native"}
_WORD = re.compile(r"[a-z0-9_-]{1,32}")
_MODEL = re.compile(r"[A-Za-z0-9._/:@\[\]~+-]{1,128}")
_VERSION = re.compile(r"[A-Za-z0-9._+-]{1,40}")

# Lower-cased markers of a terminal context failure. Only the marker name is
# reported, never the surrounding text.
CONTEXT_ERROR_MARKERS = (
    ("autocompact is thrashing", "autocompact_thrashing"),
    ("context limit reached", "context_limit_reached"),
    ("prompt is too long", "prompt_too_long"),
    ("context window exceeded", "context_window_exceeded"),
)


@dataclass
class Response:
    message_id_fp: str
    first_ts: str | None
    last_ts: str | None
    output_tokens: int = 0
    input_tokens: int = 0
    cache_read_tokens: int = 0
    cache_creation_tokens: int = 0
    stop_reason: str | None = None
    calls: list[tuple[str, str]] = field(default_factory=list)

    @property
    def context_tokens(self) -> int:
        return self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens


def _fingerprint(salt: bytes, value: str) -> str:
    return hmac.new(salt, value.encode("utf-8"), hashlib.sha256).hexdigest()[:12]


def launch_context_key(session_id: str) -> str:
    """The join key clud's writer files the record under.

    First 16 hex chars of ``sha256("clud-launch-context-v1\\0" + session_id)``,
    the same bytes as ``launch_context::session_hash`` in Rust.
    """
    data = (LAUNCH_CONTEXT_HASH_DOMAIN + session_id).encode("utf-8")
    return hashlib.sha256(data).hexdigest()[:16]


def _setting(v: Any) -> dict[str, Any] | None:
    if not isinstance(v, dict) or v.get("source") not in _SOURCES:
        return None
    value = v.get("value")
    if value is not None and (not isinstance(value, int) or isinstance(value, bool)):
        return None
    return {"value": value, "source": v["source"]}


def _match(pattern: re.Pattern[str], v: Any) -> str | None:
    return v if isinstance(v, str) and pattern.fullmatch(v) else None


def load_launch_context(state_dir: Path, session_id: str) -> dict[str, Any] | None:
    """Read and allowlist the record for ``session_id``; None when absent or bad."""
    key = launch_context_key(session_id)
    path = state_dir / "launch-context" / (key + ".json")
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(raw, dict) or raw.get("session") != key:
        return None
    max_ctx = _setting(raw.get("max_context_tokens"))
    if max_ctx is None or raw.get("route") not in _ROUTES:
        return None
    pct = raw.get("autocompact_pct_override")
    return {
        "clud_version": _match(_VERSION, raw.get("clud_version")),
        "harness": _match(_WORD, raw.get("harness")),
        "harness_version": _match(_VERSION, raw.get("harness_version")),
        "route": raw["route"],
        "provider": _match(_WORD, raw.get("provider")),
        "wire_model": _match(_MODEL, raw.get("wire_model")),
        "max_context_tokens": max_ctx,
        "auto_compact_window": _setting(raw.get("auto_compact_window")),
        "autocompact_pct_override": pct
        if isinstance(pct, int) and not isinstance(pct, bool)
        else None,
    }


def default_state_dir() -> Path | None:
    """Mirror ``daemon::default_state_dir``: env override, else ~/.clud/state."""
    override = os.environ.get("CLUD_DAEMON_STATE_DIR")
    if override:
        return Path(override)
    home = os.environ.get("USERPROFILE") or os.environ.get("HOME")
    return Path(home) / ".clud" / "state" if home else None


def _int(v: Any) -> int:
    return v if isinstance(v, int) and not isinstance(v, bool) else 0


def _text_of(obj: Any) -> str:
    """Flatten the text in a row's message content (for marker matching only)."""
    if isinstance(obj, str):
        return obj
    if isinstance(obj, list):
        return " ".join(_text_of(x) for x in obj)
    if isinstance(obj, dict):
        parts = []
        for key in ("text", "content", "message"):
            if key in obj:
                parts.append(_text_of(obj[key]))
        return " ".join(parts)
    return ""


def _result_bytes(block: dict[str, Any]) -> int:
    content = block.get("content")
    if isinstance(content, str):
        return len(content.encode("utf-8"))
    if isinstance(content, list):
        return sum(len(_text_of(c).encode("utf-8")) for c in content)
    return 0


def _percentile(values: list[int], pct: int) -> int:
    """Nearest-rank percentile of ``values``; ``0`` for an empty list.

    Integer arithmetic only: the report must not depend on float formatting.
    """
    ordered = sorted(values)
    if not ordered:
        return 0
    rank = (pct * len(ordered) + 99) // 100
    return ordered[min(max(rank, 1), len(ordered)) - 1]


def sustained_context(contexts: list[int], floor: int) -> dict[str, Any]:
    """The tokens one session re-sent above ``floor`` (#2233).

    ``premium_tokens`` is ``sum(context - floor)`` over every response that
    held at least ``floor`` tokens. It is a lower bound on what the session
    cost: the same history is sent again on every request, and cache reads
    are billed, not free. Only usage integers go in, so nothing is leaked.
    """
    held = [c for c in contexts if c > 0]
    over = [c for c in held if c >= floor]
    return {
        "floor_tokens": floor,
        "responses": len(held),
        "responses_over_floor": len(over),
        "median_context_tokens": _percentile(held, 50),
        "p90_context_tokens": _percentile(held, 90),
        "peak_context_tokens": max(held, default=0),
        "premium_tokens": sum(c - floor for c in over),
    }


def analyze(  # noqa: C901
    rows: list[dict[str, Any]],
    *,
    burst_threshold: int = DEFAULT_BURST_THRESHOLD,
    jump_threshold: int = DEFAULT_JUMP_THRESHOLD,
    context_floor: int = DEFAULT_CONTEXT_FLOOR,
    salt: bytes | None = None,
    state_dir: Path | None = None,
) -> dict[str, Any]:
    salt = salt if salt is not None else secrets.token_bytes(16)
    session_id: str | None = None
    responses: dict[str, Response] = {}
    order: list[str] = []
    compactions: list[dict[str, Any]] = []
    errors: list[dict[str, Any]] = []
    result_count = 0
    result_total = 0
    result_max = 0
    malformed = 0

    for row in rows:
        if not isinstance(row, dict):
            malformed += 1
            continue
        if session_id is None and isinstance(row.get("sessionId"), str) and row["sessionId"]:
            session_id = row["sessionId"]
        ts = row.get("timestamp") if isinstance(row.get("timestamp"), str) else None
        rtype = row.get("type")
        msg = row.get("message") if isinstance(row.get("message"), dict) else {}

        if rtype == "system" and row.get("subtype") == "compact_boundary":
            meta = row.get("compactMetadata")
            meta = meta if isinstance(meta, dict) else {}
            trigger = meta.get("trigger")
            compactions.append(
                {
                    "timestamp": ts,
                    "trigger": trigger if isinstance(trigger, str) else None,
                    "pre_tokens": _int(meta.get("preTokens")) or None,
                }
            )
            continue

        haystack = (_text_of(row.get("content")) + " " + _text_of(msg.get("content"))).lower()
        for needle, kind in CONTEXT_ERROR_MARKERS:
            if needle in haystack and rtype != "user":
                errors.append({"timestamp": ts, "kind": kind})

        if rtype == "assistant":
            mid = msg.get("id")
            if not isinstance(mid, str) or not mid:
                malformed += 1
                continue
            resp = responses.get(mid)
            if resp is None:
                resp = Response(_fingerprint(salt, "msg:" + mid), ts, ts)
                responses[mid] = resp
                order.append(mid)
            resp.last_ts = ts or resp.last_ts
            usage = msg.get("usage") if isinstance(msg.get("usage"), dict) else {}
            # Usage repeats on every row of one response: keep the maximum,
            # never the sum.
            resp.output_tokens = max(resp.output_tokens, _int(usage.get("output_tokens")))
            resp.input_tokens = max(resp.input_tokens, _int(usage.get("input_tokens")))
            resp.cache_read_tokens = max(
                resp.cache_read_tokens, _int(usage.get("cache_read_input_tokens"))
            )
            resp.cache_creation_tokens = max(
                resp.cache_creation_tokens, _int(usage.get("cache_creation_input_tokens"))
            )
            if isinstance(msg.get("stop_reason"), str):
                resp.stop_reason = msg["stop_reason"]
            content = msg.get("content")
            for block in content if isinstance(content, list) else []:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    name = block.get("name") if isinstance(block.get("name"), str) else "?"
                    canon = json.dumps(block.get("input"), sort_keys=True, default=str)
                    resp.calls.append((name, _fingerprint(salt, name + "\0" + canon)))
        elif rtype == "user":
            content = msg.get("content")
            for block in content if isinstance(content, list) else []:
                if isinstance(block, dict) and block.get("type") == "tool_result":
                    n = _result_bytes(block)
                    result_count += 1
                    result_total += n
                    result_max = max(result_max, n)

    resp_out = []
    bursts = []
    jumps = []
    prev_ctx: int | None = None
    for mid in order:
        r = responses[mid]
        counts: dict[tuple[str, str], int] = {}
        longest: dict[tuple[str, str], int] = {}
        run_key, run_len = None, 0
        for call in r.calls:
            counts[call] = counts.get(call, 0) + 1
            run_len = run_len + 1 if call == run_key else 1
            run_key = call
            longest[call] = max(longest.get(call, 0), run_len)
        for (name, fp), n in sorted(counts.items(), key=lambda kv: -kv[1]):
            if n >= burst_threshold:
                bursts.append(
                    {
                        "response": r.message_id_fp,
                        "tool": name,
                        "call_fingerprint": fp,
                        "copies": n,
                        "longest_streak": longest[(name, fp)],
                        "output_tokens": r.output_tokens,
                        "first_timestamp": r.first_ts,
                        "last_timestamp": r.last_ts,
                    }
                )
        ctx = r.context_tokens
        if prev_ctx is not None and ctx - prev_ctx >= jump_threshold:
            jumps.append(
                {"response": r.message_id_fp, "timestamp": r.first_ts, "from": prev_ctx, "to": ctx}
            )
        if ctx:
            prev_ctx = ctx
        resp_out.append(
            {
                "response": r.message_id_fp,
                "first_timestamp": r.first_ts,
                "output_tokens": r.output_tokens,
                "context_tokens": ctx,
                "stop_reason": r.stop_reason,
                "tool_calls": len(r.calls),
            }
        )

    launch = (
        load_launch_context(state_dir, session_id)
        if state_dir is not None and session_id is not None
        else None
    )
    return {
        "rows": len(rows),
        "malformed_rows": malformed,
        "responses": len(order),
        "output_tokens_total": sum(r.output_tokens for r in responses.values()),
        "max_output_tokens_single_response": max(
            (r.output_tokens for r in responses.values()), default=0
        ),
        "tool_calls_total": sum(len(r.calls) for r in responses.values()),
        "repeated_call_bursts": bursts,
        "compactions": compactions,
        "context_jumps": jumps,
        "context_errors": errors,
        "sustained_context": sustained_context(
            [row["context_tokens"] for row in resp_out], max(0, context_floor)
        ),
        "tool_results": {
            "count": result_count,
            "total_bytes": result_total,
            "max_bytes": result_max,
        },
        "max_context_tokens": launch["max_context_tokens"]
        if launch
        else {"value": None, "source": UNAVAILABLE},
        "launch_context": launch,
        "per_response": resp_out,
    }


def _max_context_line(report: dict[str, Any]) -> str:
    mc = report["max_context_tokens"]
    if report.get("launch_context") is None:
        return f"max context: {mc['source']}"
    return f"max context: {mc['value']} (source: {mc['source']})"


def sustained_context_lines(s: dict[str, Any]) -> list[str]:
    """The history-premium summary, plus the lever when the premium is large."""
    lines = [
        f"sustained context: {s['responses_over_floor']}/{s['responses']} responses at or"
        f" above {s['floor_tokens'] // 1000}k, median {s['median_context_tokens'] // 1000}k,"
        f" p90 {s['p90_context_tokens'] // 1000}k, peak {s['peak_context_tokens'] // 1000}k,"
        f" premium {s['premium_tokens'] / 1e6:.1f}M tokens"
    ]
    if s["premium_tokens"] >= PREMIUM_HINT_TOKENS:
        lines.append(
            "  that premium is history re-sent on every request, not tool output: a"
            " long-lived session is cheaper when it compacts earlier"
            " (CLAUDE_AUTOCOMPACT_PCT_OVERRIDE, e.g. 45), not when it compacts later"
        )
    return lines


def render_text(report: dict[str, Any]) -> str:
    lines = [
        f"rows: {report['rows']} (malformed: {report['malformed_rows']})",
        f"responses (by message id): {report['responses']}",
        f"output tokens (once per response): {report['output_tokens_total']}"
        f" (max single response: {report['max_output_tokens_single_response']})",
        f"tool calls: {report['tool_calls_total']}",
        f"tool results: {report['tool_results']['count']} totaling"
        f" {report['tool_results']['total_bytes']} bytes"
        f" (largest {report['tool_results']['max_bytes']})",
        _max_context_line(report),
        *sustained_context_lines(report["sustained_context"]),
        f"repeated tool-call bursts: {len(report['repeated_call_bursts'])}",
    ]
    for b in report["repeated_call_bursts"]:
        lines.append(
            f"  {b['copies']} x {b['tool']} [{b['call_fingerprint']}] in response"
            f" {b['response']} (streak {b['longest_streak']}, {b['output_tokens']} output"
            f" tokens, {b['first_timestamp']} .. {b['last_timestamp']})"
        )
    launch = report.get("launch_context")
    if launch:
        compact = launch.get("auto_compact_window") or {}
        lines.append(
            f"launch: route: {launch['route']} provider: {launch['provider']}"
            f" model: {launch['wire_model']} clud: {launch['clud_version']}"
            f" auto-compact window: {compact.get('value')} (source: {compact.get('source')})"
        )
    lines.append(f"compaction boundaries: {len(report['compactions'])}")
    for c in report["compactions"]:
        lines.append(f"  {c['timestamp']} trigger={c['trigger']} pre_tokens={c['pre_tokens']}")
    lines.append(f"context jumps: {len(report['context_jumps'])}")
    for j in report["context_jumps"]:
        lines.append(f"  {j['timestamp']} {j['from']} -> {j['to']}")
    lines.append(f"context errors: {len(report['context_errors'])}")
    for e in report["context_errors"]:
        lines.append(f"  {e['timestamp']} {e['kind']}")
    return "\n".join(lines)


def load_rows(path: Path) -> list[Any]:
    rows: list[Any] = []
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                rows.append(None)
    return rows


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("transcript", type=Path)
    parser.add_argument("--json", action="store_true", help="print the report as JSON")
    parser.add_argument("--burst-threshold", type=int, default=DEFAULT_BURST_THRESHOLD)
    parser.add_argument("--jump-threshold", type=int, default=DEFAULT_JUMP_THRESHOLD)
    parser.add_argument(
        "--context-floor",
        type=int,
        default=DEFAULT_CONTEXT_FLOOR,
        help="context size at or above which a response is charged for its history",
    )
    parser.add_argument(
        "--state-dir", type=Path, default=None, help="clud state dir holding launch-context/"
    )
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return 0 if exc.code == 0 else 1
    try:
        rows = load_rows(args.transcript)
    except OSError as exc:
        print(f"cannot read transcript: {exc.strerror}", file=sys.stderr)
        return 1
    report = analyze(
        rows,
        burst_threshold=max(2, args.burst_threshold),
        jump_threshold=args.jump_threshold,
        context_floor=max(0, args.context_floor),
        state_dir=args.state_dir or default_state_dir(),
    )
    emit_bounded(
        json.dumps(report, indent=2) if args.json else render_text(report), "transcript_report"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
