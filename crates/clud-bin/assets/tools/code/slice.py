#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# managed-by: clud
"""slice.py — one bounded read of the exact line ranges you need (#1676).

Usage:
  slice.py <file> [--lines A-B]... [--grep PAT [-C N]] [--head N]
                  [--max-bytes N] [--max-hits N] [--max-line N]

Agents page through source with `sed -n '60,110p' file; sed -n '175,400p' file`
and pay for every byte again on every later request in the session. A single
`sed` call over three disjoint ranges returns ~12 KB when the agent needed ~3.
This tool takes all the ranges at once, prints them line-numbered as
`LINE| text` under a header naming the file (repeating the path on every line
costs more than the code it prefixes), merges overlapping ranges so nothing
prints twice, and stops
at a hard byte cap with a footer naming the line to continue from. Truncation
is always visible: the footer is never omitted, and it never claims a range was
shown in full when it was not.

Selectors (at least one is recommended):
  --lines A-B     inclusive 1-based range; `A` alone means the single line A,
                  `A-` means A to end of file. Repeatable, order does not
                  matter, overlaps are merged.
  --grep PAT      Python regular expression; prints matches with context.
  -C N            context lines around each `--grep` match (default 0).
  --head N        (default selector) the first N lines, default 40.

Bounds:
  --max-bytes     hard cap on printed text, default 8192. Each selected range
                  gets an equal share, so one huge range cannot starve the
                  others.
  --max-hits      max `--grep` matches printed, default 20.
  --max-line      max characters per source line, default 400 (a minified or
                  generated line is clamped, never dropped silently).

Exit codes:
  0  everything selected was printed in full
  1  usage error
  2  the file was missing, unreadable or binary
  3  output hit a cap (text printed is complete up to the footer, which names
     the next range to ask for)
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

DEFAULT_BYTES = 8192
DEFAULT_HEAD = 40
DEFAULT_HITS = 20
DEFAULT_LINE = 400
BINARY_PROBE = 8192


class SliceError(Exception):
    """A message plus the exit code it maps to."""

    def __init__(self, message: str, code: int) -> None:
        super().__init__(message)
        self.code = code


def parse_range(text: str) -> tuple[int, int | None]:
    match = re.fullmatch(r"\s*(\d+)\s*(?:-\s*(\d*)\s*)?", text)
    if not match:
        raise SliceError(f"--lines expects A-B, A or A-: {text!r}", 1)
    start = int(match.group(1))
    if start < 1:
        raise SliceError(f"--lines is 1-based: {text!r}", 1)
    if match.group(2) is None:
        return (start, start)
    if match.group(2) == "":
        return (start, None)
    end = int(match.group(2))
    if end < start:
        raise SliceError(f"--lines range is empty: {text!r}", 1)
    return (start, end)


def merge(ranges: list[tuple[int, int | None]], total: int) -> list[tuple[int, int]]:
    """Clamp to the file and merge touching or overlapping ranges."""
    bounded: list[tuple[int, int]] = []
    for start, end in ranges:
        first = min(start, total) if total else start
        last = total if end is None else min(end, total)
        if last < first:
            continue
        bounded.append((first, last))
    merged: list[tuple[int, int]] = []
    for start, end in sorted(bounded):
        if merged and start <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return merged


def read_text(path: Path) -> str:
    try:
        raw = path.read_bytes()
    except FileNotFoundError:
        raise SliceError(f"{path}: no such file", 2) from None
    except OSError as exc:
        raise SliceError(f"{path}: {exc}", 2) from exc
    if b"\x00" in raw[:BINARY_PROBE]:
        raise SliceError(f"{path}: binary file; read it with a format-aware tool", 2)
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return raw.decode("utf-8", errors="replace")


def clamp(line: str, limit: int) -> str:
    text = line.rstrip("\r\n")
    if len(text) <= limit:
        return text
    return f"{text[:limit]}… (+{len(text) - limit} chars)"


def check_bounds(args: argparse.Namespace) -> str | None:
    """The first bound that is negative, or None."""
    for flag, value in (
        ("--max-bytes", args.max_bytes),
        ("--max-hits", args.max_hits),
        ("--max-line", args.max_line),
        ("--context", args.context),
    ):
        if value < 0:
            return flag
    return None


def select(args: argparse.Namespace, lines: list[str], total: int) -> list[tuple[int, int]]:
    """The merged ranges to print: `--lines`, `--grep` windows, or a head window."""
    selected: list[tuple[int, int]] | None = None
    if args.lines:
        selected = merge([parse_range(spec) for spec in args.lines], total)
    if args.grep is not None:
        try:
            pattern = re.compile(args.grep)
        except re.error as exc:
            raise SliceError(f"--grep: {exc}", 1) from exc
        hits = [i for i, line in enumerate(lines, 1) if pattern.search(line)]
        if not hits:
            raise SliceError(f"file has no line matching {args.grep!r}", 3)
        windows = [
            (max(1, hit - args.context), min(total, hit + args.context))
            for hit in hits[: args.max_hits]
        ]
        merged = merge(windows, total)
        selected = merged if selected is None else merge(selected + merged, total)
    if selected is None:
        head = DEFAULT_HEAD if args.head is None else args.head
        selected = merge([(1, head)], total)
    return selected


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="slice.py", description=__doc__.split("\n")[0])
    parser.add_argument("file", type=Path)
    parser.add_argument("--lines", action="append", default=[], metavar="A-B")
    parser.add_argument("--grep", metavar="PAT")
    parser.add_argument("-C", "--context", type=int, default=0)
    parser.add_argument("--head", type=int, default=None, metavar="N")
    parser.add_argument("--max-bytes", type=int, default=DEFAULT_BYTES)
    parser.add_argument("--max-hits", type=int, default=DEFAULT_HITS)
    parser.add_argument("--max-line", type=int, default=DEFAULT_LINE)
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return 1 if exc.code else 0
    invalid = check_bounds(args)
    if invalid:
        print(f"slice.py: {invalid} must be >= 0", file=sys.stderr)
        return 1

    try:
        source = read_text(args.file)
        lines = source.splitlines()
        total = len(lines)
        selected = select(args, lines, total)
    except SliceError as exc:
        print(f"slice.py: {exc}", file=sys.stderr)
        return exc.code

    out: list[str] = []
    spent = 0
    omitted_hits = 0
    wanted = sum(end - start + 1 for start, end in selected)
    if args.grep is not None:
        match_count = sum(1 for line in lines if re.search(args.grep, line))
        omitted_hits = max(0, match_count - args.max_hits)

    size = len(source.encode("utf-8"))
    header = f"# {args.file}: {total} lines, {size} B; cap {args.max_bytes} B"
    out.append(header)
    spent += len(header.encode("utf-8")) + 1
    shown = 0
    cuts: list[tuple[int, int]] = []
    for index, (start, end) in enumerate(selected):
        # Equal share per range: a long range cannot starve the shorter ones.
        remaining = len(selected) - index
        quota = max(1, (args.max_bytes - spent) // remaining)
        range_spent = 0
        for number in range(start, end + 1):
            text = clamp(lines[number - 1], args.max_line)
            row = f"{number}| {text}"
            cost = len(row.encode("utf-8")) + 1
            if range_spent + cost > quota and range_spent:
                cuts.append((number, end))
                break
            out.append(row)
            range_spent += cost
            spent += cost
            shown += 1

    truncated = bool(cuts) or omitted_hits > 0
    if cuts:
        first, last = cuts[0]
        out.append(
            f"# slice: truncated at {args.max_bytes} B — {shown} of {wanted} selected lines shown; "
            f"continue with --lines {first}-{last} ({len(cuts)} range(s) cut)"
        )
    if omitted_hits:
        shown_hits = args.max_hits + omitted_hits
        out.append(f"# slice: {args.max_hits} of {shown_hits} matches shown; narrow --grep")
    print("\n".join(out))
    return 3 if truncated else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
