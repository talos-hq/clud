#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["running-process==4.10.19"]
# ///
# managed-by: clud
"""Run a local test command and inspect bounded failure-first output (#2230)."""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import signal
import stat
import sys
from pathlib import Path

from running_process import RunningProcess, TimeoutExpired


def safe_display_path(path: Path) -> None:
    text = str(path)
    if len(os.fsencode(text)) > 300 or not all(c.isprintable() for c in text):
        raise ValueError("artifact path is too long or contains control characters")


def quote_path(path: Path, windows: bool | None = None) -> str:
    value = str(path)
    safe_display_path(path)
    if windows is None:
        windows = os.name == "nt"
    if windows:
        if any(c in value for c in '%!"&|<>^$`'):
            raise ValueError("artifact path contains unsupported Windows shell characters")
        return f'"{value}"'
    return shlex.quote(value)


def check_ancestor(info) -> None:
    if not hasattr(os, "getuid"):
        return
    if info.st_uid not in (0, os.getuid()):
        raise ValueError("artifact directory has an untrusted ancestor owner")
    if info.st_mode & 0o022 and not info.st_mode & stat.S_ISVTX:
        raise ValueError("artifact directory has a writable non-sticky ancestor")


def artifact_root() -> Path:
    if sys.platform == "darwin":
        raise ValueError("macOS artifact ACL verification is not available yet")
    if os.name == "nt":
        raise ValueError("Windows artifact privacy verification is not available yet")
    root = Path(os.environ.get("CLUD_TOOL_OUTPUT_DIR") or Path.home() / ".clud/tmp/tool-output")
    safe_display_path(root)
    if len(os.fsencode(root)) > 200:
        raise ValueError("artifact directory path is too long")
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    base = root.resolve(strict=True)
    for directory in (base, *base.parents):
        check_ancestor(directory.stat())
    private = base / "local-test-private"
    private.mkdir(mode=0o700, exist_ok=True)
    if private.is_symlink():
        raise ValueError("artifact directory must not be a symlink")
    resolved = private.resolve(strict=True)
    safe_display_path(resolved)
    info = resolved.stat()
    if hasattr(os, "getuid") and (info.st_uid != os.getuid() or info.st_mode & 0o077):
        raise ValueError("artifact directory must be private and owned by the current user")
    return resolved


MAX_OUTPUT = 8192
MAX_LINE = 2048
MAX_DIAGNOSTICS = 16
PASS = re.compile(r"^(?:test\s+\S+\s+\.\.\.\s+ok\b|PASS\s+\[|\S+\s+PASSED\b)")
SKIP = re.compile(r"^(?:test\s+\S+\s+\.\.\.\s+(?:ignored|skipped)\b|SKIP(?:PED)?\b)")
UNITTEST_FAIL = re.compile(r"^(?:FAIL|ERROR):\s+(.+?)(?:\s+\([^()]*=[^()]*\))?$")
RUST_FAIL = re.compile(r"^test\s+(\S+)\s+\.\.\.\s+FAILED\b")
NEXTEST_FAIL = re.compile(r"^FAIL\s+\[[^]]+\]\s+(.+)$")
PYTEST_FAIL = re.compile(r"^FAILED\s+(\S+)(?:\s+-\s+.*)?$")
PYTEST_VERBOSE_FAIL = re.compile(r"^(\S+::\S+)\s+FAILED(?:\s|$)")
GO_FAIL = re.compile(r"^--- FAIL:\s+(\S+)")
PANIC = re.compile(r"^thread .* panicked at")
# The last alternative is pytest's source-prefixed warnings-summary entry,
# `path.py:9: UserWarning: text`, for any `*Warning` category (#2297).
WARNING = re.compile(
    r"^(?:warning(?:\[.*?\])?:|WARN(?:ING)?\b|.*DeprecationWarning:|\s*\S.*?:\d+:\s+\w*Warning:)",
    re.I,
)
ERROR = re.compile(
    r"^(?:FAIL(?:ED)?\b|error(?:\[[^]]+\])?:|##\[error\]|thread .* panicked at|AssertionError\b|"
    r"E\s+\S|Traceback \(most recent call last\):|\s*File \".*\", line \d+|\s*-->\s+\S)"
)
SUCCESS_SUMMARY = re.compile(r"^(?:OK(?:\s|$)|test result: ok\b|=+\s+\d+ passed\b)")
SUMMARY_FAILURE = re.compile(r"\b[1-9]\d*\s+(?:failed|errors?)\b", re.I)
STAMP = re.compile(r"^\s*\d+\.\d{1,3}\s+")
ANSI = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))")


def lines(stream):
    """Read capped byte lines, consuming the remainder of oversized lines."""
    while True:
        raw = stream.readline(MAX_LINE + 1)
        if not raw:
            break
        long = len(raw) > MAX_LINE
        if long and not raw.endswith(b"\n"):
            while True:
                rest = stream.readline(MAX_LINE + 1)
                if not rest or rest.endswith(b"\n"):
                    break
        text = raw[:MAX_LINE].decode("utf-8", "replace").rstrip("\r\n")
        text = ANSI.sub("", text)
        text = "".join(c if c.isprintable() or c == "\t" else "?" for c in text)
        truncated = long or len(text) > 384
        yield text[:384] + (" [line truncated]" if truncated else "")


def failure_name(line):
    for regex in (
        UNITTEST_FAIL,
        RUST_FAIL,
        NEXTEST_FAIL,
        PYTEST_FAIL,
        PYTEST_VERBOSE_FAIL,
        GO_FAIL,
    ):
        match = regex.match(line)
        if match:
            return match.group(1)[:300]
    return None


def add_group(groups, name):
    if name in groups:
        groups[name] += 1
    elif len(groups) < MAX_DIAGNOSTICS:
        groups[name] = 1
    else:
        return 1
    return 0


def add_excerpt(target, line):
    if len(target) >= MAX_DIAGNOSTICS:
        return 1
    target.append(line)
    return 0


def collect(stream):
    groups = {}
    warnings = []
    errors = []
    omitted_groups = 0
    omitted_warnings = 0
    omitted_errors = 0
    passed = skipped = 0
    recognized = False
    success_summary = False
    input_truncated = False
    for line in lines(stream):
        input_truncated |= line.endswith(" [line truncated]")
        line = STAMP.sub("", line)
        name = failure_name(line)
        if name:
            recognized = True
            omitted_groups += add_group(groups, name)
        elif PASS.match(line):
            passed += 1
            recognized = True
        elif SKIP.match(line):
            skipped += 1
            recognized = True
        elif WARNING.match(line):
            omitted_warnings += add_excerpt(warnings, line)
        elif ERROR.match(line):
            omitted_errors += add_excerpt(errors, line)
        elif SUMMARY_FAILURE.search(line):
            omitted_errors += add_excerpt(errors, line)
        elif SUCCESS_SUMMARY.match(line):
            recognized = True
            success_summary = True
    return (
        groups,
        warnings,
        errors,
        omitted_groups,
        omitted_warnings,
        omitted_errors,
        passed,
        skipped,
        recognized,
        success_summary,
        input_truncated,
    )


def summarize(stream, exit_code, limit=MAX_OUTPUT):
    if not 128 <= limit <= MAX_OUTPUT:
        raise ValueError("insufficient report budget")
    (
        groups,
        warnings,
        errors,
        omitted_groups,
        omitted_warnings,
        omitted_errors,
        passed,
        skipped,
        _recognized,
        success_summary,
        input_truncated,
    ) = collect(stream)
    failed = bool(groups or errors or omitted_groups or omitted_errors)
    verdict = "anomalous" if exit_code == 0 and failed else "failed" if exit_code != 0 else "passed"
    if exit_code == 0 and not groups and errors and all(PANIC.match(x) for x in errors):
        verdict = "unknown"
    if exit_code == 0 and not failed and (input_truncated or not success_summary):
        verdict = "unknown"
    value = {
        "verdict": verdict,
        "diagnostic_state": "recognized"
        if failed
        else "not_needed"
        if verdict == "passed"
        else "unknown",
        "exit_code": exit_code,
        "input_truncated": input_truncated,
        "passed_lines": passed,
        "skipped_lines": skipped,
        "failure_groups": [{"name": k, "count": v} for k, v in groups.items()],
        "warnings": warnings,
        "diagnostics": errors,
        "omitted": {
            "failure_observations": omitted_groups,
            "warnings": omitted_warnings,
            "diagnostics": omitted_errors,
        },
    }
    return bounded_report(value, limit)


def bounded_report(value, limit):
    while True:
        encoded = json.dumps(value, ensure_ascii=True, separators=(",", ":"))
        if len(encoded.encode()) + 1 <= limit:
            return encoded
        for field, count in (
            ("failure_groups", "failure_observations"),
            ("warnings", "warnings"),
            ("diagnostics", "diagnostics"),
        ):
            if len(value[field]) > 1:
                item = value[field].pop()
                value["omitted"][count] += item["count"] if field == "failure_groups" else 1
                break
        else:
            # Preserve the first actionable excerpt, but make truncation explicit.
            candidate = next(
                (
                    field
                    for field in ("diagnostics", "warnings")
                    if value[field] and len(value[field][0]) > 80
                ),
                None,
            )
            if candidate is None:
                if value["failure_groups"] and len(value["failure_groups"][0]["name"]) > 16:
                    name = value["failure_groups"][0]["name"]
                    value["failure_groups"][0]["name"] = (
                        name[: max(4, (len(name) - 12) // 2)] + " [truncated]"
                    )
                    continue
                raise ValueError("report metadata exceeds output limit")
            value[candidate][0] = (
                value[candidate][0][: max(64, (len(value[candidate][0]) - 20) // 2)]
                + " [truncated]"
            )
            value["omitted"][candidate] += 1


ARTIFACT_NAME = re.compile(r"local-test-[A-Za-z0-9_-]{6,32}\.log\Z")


def open_artifact(path):
    path = Path(path)
    safe_display_path(path)
    root = artifact_root()
    if path.parent.resolve(strict=True) != root or not ARTIFACT_NAME.fullmatch(path.name):
        raise ValueError("artifact is outside the private test report directory")
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ValueError("artifact is not a private owned regular file")
        return os.fdopen(fd, "rb")
    except BaseException:
        os.close(fd)
        raise


def run_command(root, timeout, command):
    exe = os.environ.get("CLUD_TEST_BINARY") or os.environ.get("CLUD_EXE") or "clud"
    argv = [
        exe,
        "test",
        "capture",
        "--output-dir",
        str(root),
        "--timeout-secs",
        str(timeout),
        "--",
        *command,
    ]
    try:
        result = RunningProcess.run(argv, capture_output=True, text=False, timeout=timeout + 30)
    except (OSError, TimeoutError, TimeoutExpired) as error:
        raise RuntimeError("test capture helper failed or timed out") from error
    if result.returncode != 0 or len(result.stdout or b"") > 4096:
        raise RuntimeError("test capture helper failed")
    try:
        value = json.loads(result.stdout or b"")
    except (ValueError, UnicodeDecodeError) as error:
        raise ValueError("test capture helper returned malformed metadata") from error
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise ValueError("test capture helper returned invalid metadata")
    return value


def continuation(path, offset, count):
    if offset < 0 or not 1 <= count <= 1024:
        raise ValueError("invalid byte range")
    with open_artifact(path) as stream:
        size = os.fstat(stream.fileno()).st_size
        if offset > size:
            raise ValueError("byte offset is past the artifact")
        stream.seek(offset)
        data = stream.read(count)
    text = data.decode("utf-8", "replace")
    text = "".join(c if c.isprintable() else "?" for c in text)
    return json.dumps(
        {
            "offset": offset,
            "hex": data.hex(),
            "text": text,
            "next_offset": offset + len(data) if offset + len(data) < size else None,
        },
        ensure_ascii=True,
        separators=(",", ":"),
    )


class BoundedParser(argparse.ArgumentParser):
    def error(self, _message):
        raise ValueError("invalid command arguments; use --help for usage")


def compact_capture(state, exit_code):
    if state != "complete":
        return {}
    return {
        "verdict": "failed" if exit_code else "unknown",
        "exit_code": exit_code,
        "diagnostic_state": "unknown",
        "truncated": True,
    }


def render_capture(result, limit):
    path = Path(result.get("artifact", ""))
    state = result.get("state")
    if state not in {
        "complete",
        "timeout",
        "output_limit",
        "read_error",
        "write_error",
        "wait_error",
        "artifact_metadata_error",
    }:
        raise ValueError("capture returned an unknown state")
    exit_code = result.get("exit_code")
    if state == "complete" and (
        type(exit_code) is not int
        or result.get("pipe_eof") is not True
        or result.get("child_exited") is not True
        or exit_code > 255
        or (exit_code < 0 and (os.name == "nt" or -exit_code not in signal.valid_signals()))
    ):
        raise ValueError("capture reported incomplete completion metadata")
    with open_artifact(path) as stream:
        size = os.fstat(stream.fileno()).st_size
        if state == "complete" and (
            type(result.get("bytes")) is not int or result["bytes"] != size
        ):
            raise ValueError("capture artifact size does not match completion metadata")
        suffix = f"\nartifact: {quote_path(path)}"
        budget = limit - len(suffix.encode()) - 1 - (256 if state != "complete" else 0)
        try:
            if budget < 384:
                raise ValueError("insufficient detailed report budget")
            summary = json.loads(
                summarize(stream, exit_code if type(exit_code) is int else 1, limit=budget)
            )
        except ValueError:
            summary = compact_capture(state, exit_code)
    if state != "complete":
        summary["verdict"] = "incomplete"
        summary["capture_state"] = state
        summary["tree_cleanup"] = "unverified"
        summary["termination_requested"] = result.get("termination_requested") is True
    encoded = json.dumps(summary, ensure_ascii=True, separators=(",", ":"))
    if len((encoded + suffix).encode()) + 1 > limit:
        raise ValueError("report metadata exceeds output limit")
    status = (124 if state == "timeout" else 1) if state != "complete" else exit_code
    return encoded + suffix, status


def main(argv=None):
    parser = BoundedParser(description=__doc__)
    parser.add_argument("--artifact")
    parser.add_argument("--offset", type=int, default=0)
    parser.add_argument("--count", type=int, default=80)
    parser.add_argument("--timeout", type=int, default=3600)
    parser.add_argument("--limit", type=int, default=MAX_OUTPUT)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    try:
        args = parser.parse_args(argv)
        if not 512 <= args.limit <= MAX_OUTPUT:
            raise ValueError("limit must be 512..8192 bytes")
        if args.artifact:
            if args.command:
                raise ValueError("artifact continuation cannot execute a command")
            text = continuation(args.artifact, args.offset, args.count)
            if len(text.encode()) + 1 > args.limit:
                raise ValueError("byte range does not fit the requested limit")
            print(text)
            return 0
        command = args.command[1:] if args.command and args.command[0] == "--" else args.command
        if not command or not 1 <= args.timeout <= 86_400:
            raise ValueError("provide a command and a timeout of 1..86400 seconds")
        root = artifact_root()
        # Reject paths that could make even the compact result impossible before
        # starting a command; shell quoting can expand an otherwise short path.
        max_name = "local-test-" + "x" * 32 + ".log"
        projected_suffix = f"\nartifact: {quote_path(root / max_name)}"
        if len(projected_suffix.encode()) + 160 > args.limit:
            raise ValueError("artifact path does not fit the requested report limit")
        result = run_command(root, args.timeout, command)
        text, status = render_capture(result, args.limit)
        print(text)
        return status
    except OSError:
        print("test_report: private artifact I/O failed", file=sys.stderr)
        return 1
    except ValueError as error:
        message = (
            "macOS artifact ACL verification is not available yet"
            if str(error) == "macOS artifact ACL verification is not available yet"
            else "unable to produce a complete safe report"
        )
        print(f"test_report: {message}", file=sys.stderr)
        return 1
    except (RuntimeError, KeyError, TypeError):
        print("test_report: unable to produce a complete safe report", file=sys.stderr)
        return 1


if __name__ == "__main__":
    code = main()
    if code < 0 and os.name != "nt":
        signum = -code
        if signum not in (signal.SIGKILL, signal.SIGSTOP):
            signal.signal(signum, signal.SIG_DFL)
        os.kill(os.getpid(), signum)
    sys.exit(code)
