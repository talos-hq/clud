#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "running-process==4.10.1",
# ]
# ///

# managed-by: clud
"""pr_status.py — one bounded status line per pull request (#2233).

Usage:
  pr_status.py --repo OWNER/REPO --pr 3228 3244 3246
  pr_status.py --repo OWNER/REPO --pr 5093 --title --checks 1

Agents sweep several PRs with a shell loop: `for p in 3228 3244 3246; do gh pr
view $p --json state,mergeStateStatus; gh pr checks $p | grep fail; done`.
That is two `gh` calls per PR per sweep, each returning a whole
`statusCheckRollup`, and one session ran it 112 times. This tool answers the
same question once: state, draft flag, merge state, review decision, and the
counts plus the NAMES of the checks that are not green — never a check body.

The per-PR line is byte-bounded and the page is capped (`--limit`, default 12
PRs; `--checks`, default 3 names per PR). A PR that cannot be read is named
and the tool exits 2, so a partial sweep is never mistaken for a complete one.
Waiting for a PR is still `github/pr_merge_watch.py`: this reads, it does not
poll.

Run through clud's `tool run` subcommand:
  "$CLUD_EXE" tool run github/pr_status.py -- --repo OWNER/REPO --pr 42 43

Exit codes:
  0    every PR was read
  1    usage error
  2    at least one PR could not be read (named in the output)
"""

from __future__ import annotations

import argparse
import json
import re
import sys

from running_process import PIPE, RunningProcess, TimeoutExpired

GH_TIMEOUT = 30.0
MAX_OUTPUT = 4096
MAX_PRS = 60
MAX_NAME = 28
REPO = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
ORIGIN = re.compile(r"github\.com[:/]([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+?)(?:\.git)?\Z")
FIELDS = "number,state,isDraft,mergeStateStatus,reviewDecision,title,statusCheckRollup"
FAILED = {"failure", "cancelled", "timed_out", "startup_failure", "action_required", "stale"}
PENDING = {"pending", "queued", "in_progress", "waiting", "requested"}


def gh(*args: str) -> tuple[int, str, str]:
    """Run gh; return (exit code, stdout, stderr)."""
    try:
        # stderr=PIPE: running-process merges stderr into stdout otherwise (#1175).
        res = RunningProcess.run(
            ["gh", *args], capture_output=True, stderr=PIPE, text=True, timeout=GH_TIMEOUT
        )
    except (TimeoutError, TimeoutExpired):
        return 124, "", f"gh {' '.join(args)} timed out after {GH_TIMEOUT:g}s"
    except OSError as exc:
        return 127, "", f"could not start gh: {exc}"
    return res.returncode, res.stdout or "", res.stderr or ""


def resolve_repo(explicit: str | None) -> str | None:
    """The explicit --repo, else the origin remote's owner/name."""
    if explicit:
        return explicit if REPO.match(explicit) else None
    try:
        res = RunningProcess.run(
            ["git", "remote", "get-url", "origin"],
            capture_output=True,
            stderr=PIPE,
            text=True,
            timeout=GH_TIMEOUT,
        )
    except (TimeoutExpired, TimeoutError, OSError):
        return None
    if res.returncode != 0:
        return None
    match = ORIGIN.search((res.stdout or "").strip())
    return match.group(1) if match else None


def parse_prs(values: list[list[str]]) -> tuple[list[int], str | None]:
    """`--pr 1 2` and `--pr 1,2` both mean [1, 2]; duplicates collapse, order kept."""
    numbers: list[int] = []
    for group in values:
        for chunk in group:
            for part in re.split(r"[,\s]+", chunk.strip()):
                if not part:
                    continue
                if not part.isdigit() or int(part) <= 0:
                    return [], f"not a PR number: {part!r}"
                number = int(part)
                if number not in numbers:
                    numbers.append(number)
    if not numbers:
        return [], "no --pr given"
    if len(numbers) > MAX_PRS:
        return numbers[:MAX_PRS], None
    return numbers, None


def classify(pull: dict) -> dict:
    """Pure: counts and non-green names from one `gh pr view --json` record."""
    rollup = pull.get("statusCheckRollup") or []
    failed: list[str] = []
    pending = 0
    passed = 0
    for check in rollup:
        if not isinstance(check, dict):
            continue
        name = check.get("name") or check.get("context") or "?"
        conclusion = (check.get("conclusion") or check.get("state") or "").lower()
        status = (check.get("status") or "").lower()
        if conclusion in FAILED:
            failed.append(str(name))
        elif conclusion in PENDING or (not conclusion and status in PENDING):
            pending += 1
        elif conclusion in {"success", "neutral", "skipped"}:
            passed += 1
        else:
            pending += 1
    return {
        "number": pull.get("number"),
        "state": pull.get("state") or "?",
        "draft": bool(pull.get("isDraft")),
        "merge": pull.get("mergeStateStatus") or "?",
        "review": pull.get("reviewDecision") or "-",
        "title": pull.get("title") or "",
        "total": len(rollup),
        "passed": passed,
        "pending": pending,
        "failed": failed,
    }


def clip(text: str, limit: int = MAX_NAME) -> str:
    text = " ".join(str(text).split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def render_line(row: dict, checks_cap: int, want_title: bool) -> str:
    state = row["state"] + ("(draft)" if row["draft"] else "")
    # GitHub reports UNKNOWN outside an open PR; a merged PR's merge state is history.
    merge = row["merge"] if row["state"] == "OPEN" else "-"
    line = (
        f"#{row['number']} {state} merge={merge} review={row['review']} "
        f"checks={row['passed']}/{row['total']}"
    )
    if row["failed"]:
        shown = ",".join(clip(name) for name in row["failed"][:checks_cap])
        more = len(row["failed"]) - checks_cap
        line += f" fail={shown}" + (f" +{more} (--checks {len(row['failed'])})" if more > 0 else "")
    if row["pending"]:
        line += f" pending={row['pending']}"
    if want_title and row["title"]:
        line += f" · {clip(row['title'], 60)}"
    return line


def read_pr(repo: str, number: int) -> tuple[dict | None, str]:
    """One `gh pr view`; returns (record, error)."""
    code, out, err = gh("pr", "view", str(number), "--repo", repo, "--json", FIELDS)
    if code != 0:
        detail = clip((err or out).strip().splitlines()[-1] if (err or out).strip() else "", 80)
        return None, f"gh exit {code}{': ' + detail if detail else ''}"
    try:
        return json.loads(out), ""
    except json.JSONDecodeError:
        return None, "gh printed invalid JSON"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="pr_status.py",
        description="One bounded status line per pull request.",
    )
    parser.add_argument("--repo", help="owner/name (default: the gh-resolved repo)")
    parser.add_argument(
        "--pr",
        action="append",
        nargs="+",
        default=[],
        metavar="N",
        help="PR numbers, repeatable or comma separated: --pr 42 43 [--pr 44], --pr 42,43",
    )
    parser.add_argument(
        "--checks", type=int, default=3, help="failing check names per PR (default 3)"
    )
    parser.add_argument(
        "--limit", type=int, default=12, help="PRs printed before the footer (default 12)"
    )
    parser.add_argument("--title", action="store_true", help="append the PR title")
    args = parser.parse_args(argv)

    numbers, error = parse_prs(args.pr)
    if error:
        print(f"pr_status: {error}", file=sys.stderr)
        return 1
    repo = resolve_repo(args.repo)
    if repo is None:
        print("pr_status: --repo OWNER/REPO is required (no repo resolved)", file=sys.stderr)
        return 1

    limit = max(1, min(args.limit, len(numbers)))
    checks_cap = max(0, args.checks)
    lines: list[str] = []
    unreadable = 0
    used = 0
    for number in numbers[:limit]:
        pull, failure = read_pr(repo, number)
        if pull is None:
            unreadable += 1
            line = f"#{number} unreachable ({failure})"
        else:
            line = render_line(classify(pull), checks_cap, args.title)
        if lines and used + len(line) + 1 > MAX_OUTPUT:
            remaining = len(numbers[:limit]) - len(lines)
            lines.append(f"+{remaining} more PRs (page cap {MAX_OUTPUT} B)")
            break
        lines.append(line)
        used += len(line) + 1

    read_count = sum(1 for line in lines if not line.startswith("+"))
    failing = sum(1 for line in lines if " fail=" in line)
    footer = f"{len(numbers)} PR(s) from {repo}: {read_count - unreadable} read, {failing} failing"
    if unreadable:
        footer += f", {unreadable} unreachable"
    unread_rest = len(numbers) - read_count
    if unread_rest > 0:
        footer += f", +{unread_rest} not read (--limit)"
    lines.append(footer)
    print("\n".join(lines))
    return 2 if unreadable else 0


if __name__ == "__main__":
    sys.exit(main())
