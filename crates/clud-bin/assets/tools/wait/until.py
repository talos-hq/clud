#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
# managed-by: clud
"""wait/until.py — block on one command until its output changes (#2631).

Polling with a model turn costs the whole conversation again on every tick.
Ran once, this tool blocks instead and returns only the delta:

  "$CLUD_EXE" tool run wait/until.py -- --cmd 'gh issue view 42 --repo o/r --json comments'
  "$CLUD_EXE" tool run wait/until.py -- --cmd 'systemctl is-active twp-catchup' --interval 300
  "$CLUD_EXE" tool run wait/until.py -- --cmd 'gh pr view 12 --json state' --match 'MERGED'

The first run is the baseline. Every `--interval` seconds the command runs
again; its combined stdout+stderr is digested, and the call returns as soon
as the digest changes (or, with `--match`, as soon as a regex matches, which
also fires on the baseline). Nothing is printed while the state holds still,
so a wait that finds nothing costs one small result instead of N of them.

Exit codes:
  0    the output changed, or `--match` matched
  3    the command could not be started
  64   usage error
  124  the timeout expired with no change; the last output is shown
"""

from __future__ import annotations

import argparse
import hashlib
import re
import subprocess
import sys
import time

MAX_BYTES = 4096
RUN_TIMEOUT_CEILING = 300.0
EXIT_CHANGED = 0
EXIT_START = 3
EXIT_USAGE = 64
EXIT_TIMEOUT = 124


def bounded(text: str, limit: int) -> str:
    """`text` cut to `limit` characters with an explicit omission marker."""
    if len(text) <= limit:
        return text
    omitted = len(text) - limit
    return (
        f"{text[:limit]}\n[... {omitted} more characters omitted; re-run the command to read more]"
    )


def run_once(command: str, timeout: float) -> tuple[int, str]:
    """Run `command`; return its exit code and combined output.

    A command that never finishes reports code 124 and a marker, so a hung
    query cannot wedge the wait: the marker changes the digest like any other
    output would.
    """
    started = time.monotonic()
    try:
        proc = subprocess.run(
            command,
            shell=True,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return EXIT_TIMEOUT, f"<command still running after {timeout:.0f}s>"
    except OSError as exc:
        return EXIT_START, f"<command failed to start: {exc}>"
    elapsed = time.monotonic() - started
    if elapsed > timeout:
        return EXIT_TIMEOUT, f"<command exceeded {timeout:.0f}s>"
    return proc.returncode, (proc.stdout or "") + (proc.stderr or "")


def digest(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8", "replace")).hexdigest()[:12]


def positive_float(text: str) -> float:
    value = float(text)
    if value <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return value


class Parser(argparse.ArgumentParser):
    """Report usage errors as 64, like the other bundled waiters."""

    def error(self, message: str) -> None:  # type: ignore[override]
        self.print_usage(sys.stderr)
        self.exit(EXIT_USAGE, f"{self.prog}: error: {message}\n")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = Parser(
        prog="until.py",
        description="Block until a command's output changes.",
    )
    parser.add_argument("--cmd", required=True, help="shell command to run on every poll")
    parser.add_argument(
        "--interval", type=positive_float, default=120.0, help="seconds between polls"
    )
    parser.add_argument("--timeout", type=positive_float, default=1800.0, help="total wait budget")
    parser.add_argument("--match", default=None, help="return as soon as this regex matches")
    parser.add_argument(
        "--max-bytes", type=int, default=MAX_BYTES, help="cap on the reported output"
    )
    args = parser.parse_args(argv)
    if args.max_bytes < 200:
        parser.error("--max-bytes must be at least 200")
    if args.match is not None:
        try:
            args.match = re.compile(args.match, re.MULTILINE)
        except re.error as exc:
            parser.error(f"--match is not a valid regex: {exc}")
    return args


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    started = time.monotonic()
    polls = 1
    code, output = run_once(args.cmd, min(args.interval, RUN_TIMEOUT_CEILING))
    baseline = digest(output)
    if args.match is not None and args.match.search(output):
        print(f"matched on the first run (exit {code}):")
        print(bounded(output, args.max_bytes))
        return EXIT_CHANGED
    while True:
        remaining = args.timeout - (time.monotonic() - started)
        if remaining <= 0:
            print(
                f"unchanged after {args.timeout:.0f}s ({polls} poll{'s' if polls > 1 else ''}); "
                f"no change from `{args.cmd}`. Last result, exit {code}:"
            )
            print(bounded(output, args.max_bytes))
            return EXIT_TIMEOUT
        time.sleep(min(args.interval, remaining))
        polls += 1
        code, output = run_once(args.cmd, min(args.interval, RUN_TIMEOUT_CEILING))
        if code == EXIT_START:
            print(f"could not run the command: {bounded(output, args.max_bytes)}")
            return EXIT_START
        elapsed = time.monotonic() - started
        matched = args.match is not None and args.match.search(output) is not None
        if not matched and digest(output) == baseline:
            continue
        reason = "matched" if matched else "changed"
        print(f"{reason} after {elapsed:.0f}s ({polls} polls), exit {code}:")
        print(bounded(output, args.max_bytes))
        return EXIT_CHANGED


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
