#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# managed-by: clud
"""Count repeated Codex and Claude tool activity without exposing transcript content."""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import posixpath
import re
import secrets
import shlex
import sys
from collections import Counter

MAX_ROW_BYTES = 16 * 1024 * 1024
MAX_TRACKED = 100_000
MAX_NESTED_MENTIONS = 1000
FRICTION_MARKERS = {
    "hook_refusal": re.compile(
        r"^(?:Script error: )?Command blocked by PreToolUse hook:"
        r"|^[ \t]*PreToolUse[^\n]{0,160}\b(?:blocked\b|denied\b|Refused a )"
        r"|^[ \t]*(?:\[[a-z][a-z-]+\] )?Refused a ",
        re.I | re.M,
    ),
    "output_cap": re.compile(
        r"^(?:Warning: truncated output\b|Original token count:|\[TRUNCATED:)",
        re.I | re.M,
    ),
}
READ = re.compile(r"(?:^|[\s;'\"(])(?:cat|head|tail|sed|rg|grep|read_file)(?:[\s;'\")]|$)")
FAIL = re.compile(
    r"(?:\berror(?:\[[^]\n]{1,32}\])?:|##\[error\]|\bcould not compile\b|"
    r"(?:^|\n)[ \t]*(?:FAIL(?:ED)?\b|panic\b))",
    re.I,
)
WARNING = re.compile(
    r"(?:\bwarning(?:\[[^]\n]{1,32}\])?:|##\[warning\]|(?:^|\n)[ \t]*warn\b)", re.I
)
SUCCESS = re.compile(r"(?:^|\n)[ \t]*(?:test result: ok\b|\d+ passed\b|PASS\b|finished `)", re.I)
ANSI = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))")
PROGRESS = re.compile(
    r"(?:^|\n)[^\n]{0,160}(?:soldr\[cache\]|Compiling\s+\w|test\s+\S+\s+\.\.\.\s+ok\b|PASS\s+\[)",
    re.I,
)
TIMEOUT = re.compile(r"\b(?:timed out|timeout exceeded|deadline exceeded)\b", re.I)
CANCELLATION = re.compile(r"\b(?:cancelled|canceled)\b", re.I)
BINARY = re.compile(r"(?:data:[^\s;,]+;base64,|\"(?:image_url|data)\"\s*:\s*\"[A-Za-z0-9+/]{128})")


def fingerprint(salt, value):
    return hmac.new(salt, value.encode("utf-8", "replace"), hashlib.sha256).digest()


def category(name, arguments):
    if name in {"wait", "wait_agent", "sleep", "write_stdin"}:
        return "wait"
    if "tools.write_stdin(" in arguments or "tools.clock__" in arguments:
        return "wait"
    if READ.search(arguments):
        return "read"
    if re.search(r"\b(?:git status|gh run|bosn ci show)\b", arguments):
        return "state_probe"
    if re.search(r"\b(?:test|pytest|nextest|clippy|lint)\b", arguments):
        return "validation"
    return "other"


def simple_command_identity(name, arguments, salt):
    """Recognize only direct, simple argv; compound shell syntax remains unknown."""
    if name != "exec_command":
        return None, None
    try:
        obj = json.loads(arguments)
        command = obj.get("cmd") if isinstance(obj, dict) else None
        if not isinstance(command, str) or any(c in command for c in "|;&<>`$\n\r"):
            return None, None
        argv = shlex.split(command)
    except (ValueError, TypeError, RecursionError):
        return None, None
    if not argv or argv[0] not in {
        "cat",
        "head",
        "tail",
        "rg",
        "grep",
        "sed",
        "git",
        "gh",
        "bosn",
        "soldr",
    }:
        return None, None
    command_shape = fingerprint(salt, "program\0" + argv[0])
    target = None
    if argv[0] in {"cat", "head", "tail"} and len(argv) == 2 and not argv[-1].startswith("-"):
        value = argv[-1]
        workdir = obj.get("workdir")
        if value.startswith("/"):
            target = fingerprint(salt, "target\0" + posixpath.normpath(value))
        elif isinstance(workdir, str) and workdir.startswith("/"):
            target = fingerprint(
                salt, "target\0" + posixpath.normpath(posixpath.join(workdir, value))
            )
    return command_shape, target


def static_command_identities(name, arguments, salt):
    if name == "exec_command":
        yield simple_command_identity(name, arguments, salt)
    elif name == "exec":
        # Static mentions are not proof that a JS branch actually executed.
        pattern = r'tools\.exec_command\(\s*\{\s*cmd\s*:\s*("(?:\\.|[^"\\])*")'
        for index, match in enumerate(re.finditer(pattern, arguments)):
            if index >= MAX_NESTED_MENTIONS:
                break
            try:
                command = json.loads(match.group(1))
            except ValueError:
                continue
            yield simple_command_identity("exec_command", json.dumps({"cmd": command}), salt)


def observe_identity(identity, result, command_shapes, read_targets):
    command_shape, target = identity
    if command_shape is not None:
        result["repeated_simple_command_shapes"] += int(command_shape in command_shapes)
        if len(command_shapes) < MAX_TRACKED:
            command_shapes.add(command_shape)
        elif command_shape not in command_shapes:
            result["tracking_truncated"] = True
    if target is not None:
        result["repeated_simple_read_targets"] += int(target in read_targets)
        if len(read_targets) < MAX_TRACKED:
            read_targets.add(target)
        elif target not in read_targets:
            result["tracking_truncated"] = True


def wait_observation(text):
    # These markers describe only what this tool result says, not whether
    # polling was necessary or a process tree has fully exited.
    terminal = bool(
        re.search(r'["\']?exit_code["\']?\s*[:=]\s*-?\d+|Process exited with code', text)
    )
    live = bool(re.search(r'["\']?session_id["\']?\s*[:=]\s*\d+|Script running with cell ID', text))
    missing = bool(re.search(r"(?:session|cell) (?:was )?not found", text, re.I))
    present = [
        name
        for name, found in (("terminal", terminal), ("live", live), ("missing", missing))
        if found
    ]
    return present[0] if len(present) == 1 else "unknown"


def text_output(value):
    """Return text and whether non-text/binary content was excluded."""
    if isinstance(value, str):
        return ("", True) if BINARY.search(value) else (value, False)
    if not isinstance(value, list):
        return "", True
    parts = []
    unknown = False
    for item in value:
        if isinstance(item, str):
            text = item
        elif isinstance(item, dict) and item.get("type") in {"input_text", "output_text", "text"}:
            text = item.get("text")
        else:
            unknown = True
            continue
        if not isinstance(text, str) or BINARY.search(text):
            unknown = True
        else:
            parts.append(text)
    return "".join(parts), unknown


def events(row):
    """Yield bounded event kinds; retain no transcript content in the final report."""
    if not isinstance(row, dict):
        yield ("malformed",)
        return
    kind = row.get("type")
    if kind == "turn_context":
        yield ("turn",)
        return
    if kind == "compacted" or (kind == "system" and row.get("subtype") == "compact_boundary"):
        yield ("compaction",)
        return
    if kind == "response_item":
        item = row.get("payload")
        if not isinstance(item, dict):
            yield ("malformed",)
            return
        item_type = item.get("type")
        if item_type in {"custom_tool_call", "function_call"}:
            yield (
                "call",
                item.get("call_id"),
                item.get("name"),
                item.get("input", item.get("arguments")),
            )
        elif item_type in {"custom_tool_call_output", "function_call_output"}:
            yield ("result", item.get("call_id"), item.get("output"), item.get("id"))
        return
    yield from claude_events(row)


def claude_events(row):
    if row.get("type") not in {"assistant", "user"}:
        return
    message = row.get("message")
    if not isinstance(message, dict) or not isinstance(message.get("content"), list):
        return
    for index, item in enumerate(message["content"]):
        if not isinstance(item, dict):
            yield ("malformed",)
        elif item.get("type") == "tool_use":
            yield ("call", item.get("id"), item.get("name"), item.get("input"))
        elif item.get("type") == "tool_result":
            row_id = row.get("uuid")
            result_id = f"{row_id}:{index}" if isinstance(row_id, str) else None
            yield ("result", item.get("tool_use_id"), item.get("content"), result_id)


def analyze(rows, *, salt=None):
    salt = salt if salt is not None else secrets.token_bytes(32)
    result = {
        "schema_version": 1,
        "tool_calls": 0,
        "tool_results": 0,
        "duplicate_calls": 0,
        "conflicting_duplicate_calls": 0,
        "duplicate_results": 0,
        "result_identity_unknown": 0,
        "conflicting_or_unknown_duplicate_results": 0,
        "unmatched_results": 0,
        "repeated_calls": 0,
        "repeated_call_pairs": 0,
        "repeated_pairs_by_category": {
            name: 0 for name in ("wait", "read", "state_probe", "validation", "other")
        },
        "pair_interpretation": "consecutive accepted call shapes; necessity unverified",
        "repeated_by_category": {
            name: 0 for name in ("wait", "read", "state_probe", "validation", "other")
        },
        "repeated_simple_command_shapes": 0,
        "repeated_simple_read_targets": 0,
        "nested_tool_mentions": {"exec_command": 0, "write_stdin": 0, "other": 0},
        "nested_interpretation": "static mentions and literal shapes; not necessarily executed",
        "output_bytes": 0,
        "largest_output_bytes": 0,
        "friction_results": dict.fromkeys((*FRICTION_MARKERS, "over_32_kib"), 0),
        "incomplete_results": 0,
        "malformed_rows": 0,
        "turns": 0,
        "compactions": 0,
        "categories": {name: 0 for name in ("wait", "read", "state_probe", "validation", "other")},
        "output_classes": {
            name: 0
            for name in (
                "failure",
                "warning",
                "success",
                "progress",
                "timeout",
                "cancellation",
                "unknown",
            )
        },
        "repeated_read_observations": {
            "same_observed_text": 0,
            "changed_observed_text": 0,
            "unknown": 0,
        },
        "read_completeness": "unknown; equal observed text does not establish unchanged input",
        "target_interpretation": "simple absolute or explicit-cwd reads only; identity unverified",
        "wait_observations": {name: 0 for name in ("live", "terminal", "missing", "unknown")},
        "wait_interpretation": "reported markers only; necessity and tree state unverified",
        "category_interpretation": "heuristic classifications; not a judgment of necessity",
        "output_class_interpretation": "heuristic markers per result; classes can overlap",
        "command_shape_interpretation": "recognized program name only; arguments may differ",
        "byte_scope": "included text bytes; binary-containing blocks excluded; not model tokens",
        "tracking_truncated": False,
        "nested_mentions_truncated": False,
    }
    calls = {}
    seen_shapes = set()
    seen_results = {}
    read_states = {}
    command_shapes = set()
    read_targets = set()
    uncertain_calls = set()
    seen_pairs = set()
    previous_shape = None
    for row in rows:
        for event in events(row):
            kind = event[0]
            if kind in {"turn", "compaction", "malformed"}:
                result[
                    {"turn": "turns", "compaction": "compactions", "malformed": "malformed_rows"}[
                        kind
                    ]
                ] += 1
            elif kind == "call":
                before = result["tool_calls"]
                record_call(
                    event,
                    salt,
                    result,
                    calls,
                    seen_shapes,
                    command_shapes,
                    read_targets,
                    uncertain_calls,
                )
                if result["tool_calls"] > before:
                    shape, call_category = calls[fingerprint(salt, event[1])]
                    observe_pair(previous_shape, shape, call_category, seen_pairs, result)
                    previous_shape = shape
                elif result["tracking_truncated"]:
                    previous_shape = None
            elif kind == "result":
                record_result(
                    event, salt, result, calls, seen_results, read_states, uncertain_calls
                )
    finalize_reads(calls, read_states, result, uncertain_calls)
    return result


def observe_pair(previous, shape, call_category, seen_pairs, result):
    if previous is None:
        return
    pair = (previous, shape)
    if pair in seen_pairs:
        result["repeated_call_pairs"] += 1
        result["repeated_pairs_by_category"][call_category] += 1
    elif len(seen_pairs) < MAX_TRACKED:
        seen_pairs.add(pair)
    else:
        result["tracking_truncated"] = True


def record_call(
    event, salt, result, calls, seen_shapes, command_shapes, read_targets, uncertain_calls
):
    _, call_id, name, arguments = event
    if not isinstance(call_id, str) or not isinstance(name, str):
        result["malformed_rows"] += 1
        return
    identity = fingerprint(salt, call_id)
    if not isinstance(arguments, str):
        try:
            arguments = json.dumps(
                arguments, sort_keys=True, ensure_ascii=True, separators=(",", ":")
            )
        except (TypeError, ValueError, RecursionError):
            result["malformed_rows"] += 1
            return
    shape = fingerprint(salt, name + "\0" + arguments)
    if identity in calls:
        result["duplicate_calls"] += 1
        if calls[identity][0] != shape:
            result["conflicting_duplicate_calls"] += 1
            uncertain_calls.add(identity)
        return
    if len(calls) >= MAX_TRACKED:
        result["tracking_truncated"] = True
        return
    call_category = category(name, arguments)
    for command_identity in static_command_identities(name, arguments, salt):
        observe_identity(command_identity, result, command_shapes, read_targets)
    if name == "exec":
        count_nested_mentions(arguments, result)
    calls[identity] = (shape, call_category)
    result["tool_calls"] += 1
    result["categories"][call_category] += 1
    if shape in seen_shapes:
        result["repeated_calls"] += 1
        result["repeated_by_category"][call_category] += 1
    else:
        seen_shapes.add(shape)


def count_nested_mentions(arguments, result):
    for index, match in enumerate(
        re.finditer(r"\btools\.([a-zA-Z_][a-zA-Z_0-9]*)\s*\(", arguments)
    ):
        if index >= MAX_NESTED_MENTIONS:
            result["nested_mentions_truncated"] = True
            break
        nested = match.group(1)
        bucket = nested if nested in {"exec_command", "write_stdin"} else "other"
        result["nested_tool_mentions"][bucket] += 1


def is_replayed_result(
    result_id, call_identity, digest, incomplete, salt, result, seen_results, uncertain_calls, calls
):
    if isinstance(result_id, str) and result_id:
        result_identity = fingerprint(salt, result_id)
        if result_identity in seen_results:
            result["duplicate_results"] += 1
            old_call, old_digest, old_incomplete = seen_results[result_identity]
            conflict = (
                call_identity != old_call or incomplete or old_incomplete or old_digest != digest
            )
            result["conflicting_or_unknown_duplicate_results"] += int(conflict)
            if conflict:
                uncertain_calls.update(
                    identity for identity in (old_call, call_identity) if identity in calls
                )
            return True
        if len(seen_results) < MAX_TRACKED:
            seen_results[result_identity] = (call_identity, digest, incomplete)
        else:
            result["tracking_truncated"] = True
    else:
        # Without an event identity, count every observed block and mark the
        # uncertainty rather than assuming a second chunk is a replay.
        result["result_identity_unknown"] += 1
    return False


def record_result(event, salt, result, calls, seen_results, read_states, uncertain_calls):
    _, call_id, raw, result_id = event
    if not isinstance(call_id, str):
        result["malformed_rows"] += 1
        return
    call_identity = fingerprint(salt, call_id)
    text, incomplete = text_output(raw)
    digest = fingerprint(salt, text)
    conflicts_before = result["conflicting_or_unknown_duplicate_results"]
    if is_replayed_result(
        result_id,
        call_identity,
        digest,
        incomplete,
        salt,
        result,
        seen_results,
        uncertain_calls,
        calls,
    ):
        if result["conflicting_or_unknown_duplicate_results"] > conflicts_before:
            state = read_states.get(call_identity)
            if state is not None:
                state[1] = True
        return
    size = len(text.encode("utf-8", "replace"))
    result["tool_results"] += 1
    result["output_bytes"] += size
    result["largest_output_bytes"] = max(result["largest_output_bytes"], size)
    result["incomplete_results"] += int(incomplete)
    record_output_classes(text, incomplete, result)
    friction = result["friction_results"]
    friction["over_32_kib"] += int(size > 32 * 1024)
    for name, pattern in FRICTION_MARKERS.items():
        friction[name] += int(bool(pattern.search(text)))
    call = calls.get(call_identity)
    if call is None:
        result["unmatched_results"] += 1
    if call and call[1] == "wait":
        result["wait_observations"]["unknown" if incomplete else wait_observation(text)] += 1
    if call and call[1] == "read":
        record_read(call_identity, text, incomplete or not result_id, salt, read_states)


def record_output_classes(text, incomplete, result):
    matched = False
    clean = ANSI.sub("", text)
    for name, pattern in (
        ("failure", FAIL),
        ("warning", WARNING),
        ("success", SUCCESS),
        ("progress", PROGRESS),
        ("timeout", TIMEOUT),
        ("cancellation", CANCELLATION),
    ):
        if pattern.search(clean):
            result["output_classes"][name] += 1
            matched = True
    if not matched or incomplete:
        result["output_classes"]["unknown"] += 1


def record_read(call_identity, text, incomplete, salt, read_states):
    if call_identity not in read_states:
        read_states[call_identity] = [hmac.new(salt, digestmod=hashlib.sha256), False]
    state = read_states[call_identity]
    state[0].update(text.encode("utf-8", "replace"))
    state[1] |= bool(incomplete)


def finalize_reads(calls, read_states, result, uncertain_calls):
    previous = {}
    for call_identity, (shape, call_category) in calls.items():
        if call_category != "read":
            continue
        state = read_states.get(call_identity)
        digest = (
            state[0].digest()
            if state is not None and not state[1] and call_identity not in uncertain_calls
            else None
        )
        if shape in previous:
            old = previous[shape]
            observation = (
                "unknown"
                if old is None or digest is None
                else "same_observed_text"
                if old == digest
                else "changed_observed_text"
            )
            result["repeated_read_observations"][observation] += 1
        previous[shape] = digest


def read_rows(path, stats):
    with open(path, "rb") as stream:
        while True:
            line = stream.readline(MAX_ROW_BYTES + 1)
            if not line:
                break
            if len(line) > MAX_ROW_BYTES:
                while line and not line.endswith(b"\n"):
                    line = stream.readline(MAX_ROW_BYTES + 1)
                stats["oversized_rows"] += 1
                continue
            try:
                yield json.loads(line)
            except (ValueError, UnicodeDecodeError, RecursionError):
                stats["malformed_json_rows"] += 1


class SafeParser(argparse.ArgumentParser):
    def error(self, _message):
        raise ValueError("invalid arguments")


def merge_report(total, report):
    """Combine counts from independent transcripts using the fixed audit schema."""
    for key, value in report.items():
        if isinstance(value, dict):
            merge_report(total[key], value)
        elif isinstance(value, bool):
            total[key] |= value
        elif key == "largest_output_bytes":
            total[key] = max(total[key], value)
        elif isinstance(value, int) and key != "schema_version":
            total[key] += value


def audit_transcripts(paths):
    total = None
    prevalence = dict.fromkeys((*FRICTION_MARKERS, "over_32_kib"), 0)
    for path in paths:
        stats = Counter(oversized_rows=0, malformed_json_rows=0)
        report = analyze(read_rows(path, stats))
        report.update(stats)
        for marker, count in report["friction_results"].items():
            prevalence[marker] += int(count > 0)
        if total is None:
            total = report
        else:
            merge_report(total, report)
    total["transcripts"] = len(paths)
    total["transcripts_with_friction"] = prevalence
    return total


def main(argv=None):
    parser = SafeParser(description=__doc__)
    parser.add_argument("transcript", nargs="+")
    try:
        args = parser.parse_args(argv)
    except ValueError:
        print("codex_audit: invalid arguments", file=sys.stderr)
        return 2
    try:
        result = audit_transcripts(args.transcript)
    except OSError:
        print("codex_audit: could not read transcript", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
