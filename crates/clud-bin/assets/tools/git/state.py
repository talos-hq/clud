#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "running-process==4.10.1",
# ]
# ///
# managed-by: clud
"""state.py — a checkout's whole git state in one call (#1339 item 7).

Usage:
  state.py [--commits N] [--files N] [--stashes N] [--worktrees N] [--all]
           [--fetch] [--json]

/grind agents probe a checkout with `git status`, `git branch`, `git log`,
`git stash list` and `git worktree list`, one call each, again and again.
This prints all of it at once: the branch, its upstream with ahead/behind,
`status --porcelain` entries, the last N commits (default 3), stashes and
worktrees.

Every list is bounded, because this output is re-sent on every later request
of the session: at most 20 changed paths, 3 commits, 5 stashes and 5
worktrees, each section headed by its true total and followed by an explicit
`+N more` count. `--all` lifts the caps; `--files/--commits/--stashes/
--worktrees N` sets one. A list that fits is printed whole, unchanged.

It never fetches, so the upstream counts are as of the last fetch; pass
`--fetch` when the question is what landed upstream (one network round trip,
and a fetch failure is reported on stderr without failing the call).

Run through clud's `tool run` subcommand:
  "$CLUD_EXE" tool run git/state.py

Exit codes:
  0  printed
  1  usage error
  2  not a git checkout, or a git call failed
"""

from __future__ import annotations

import argparse
import json
import sys

from running_process import PIPE, RunningProcess, TimeoutExpired

TIMEOUT = 60.0

# Default bounds. This output rides on every later request of the session, so
# the defaults favour a page that answers "where am I" over an exhaustive
# listing; `--all` restores the unbounded form.
DEFAULT_COMMITS = 3
MAX_FILES = 20
MAX_STASHES = 5
MAX_WORKTREES = 5


def git(*args: str) -> tuple[int, str]:
    try:
        res = RunningProcess.run(
            ["git", *args], capture_output=True, stderr=PIPE, text=True, timeout=TIMEOUT, check=False
        )
    except (TimeoutError, TimeoutExpired):
        return 124, ""
    except FileNotFoundError:
        return 127, ""
    return res.returncode, res.stdout or ""


def parse_status(text: str) -> dict:
    """Parse `git status --porcelain=v2 --branch` into branch facts and entries."""
    state: dict = {
        "branch": None,
        "head": None,
        "upstream": None,
        "ahead": None,
        "behind": None,
        "changes": [],
    }
    for line in text.splitlines():
        if line.startswith("# branch.oid "):
            oid = line.split(" ", 2)[2]
            state["head"] = None if oid == "(initial)" else oid[:12]
        elif line.startswith("# branch.head "):
            head = line.split(" ", 2)[2]
            state["branch"] = None if head == "(detached)" else head
        elif line.startswith("# branch.upstream "):
            state["upstream"] = line.split(" ", 2)[2]
        elif line.startswith("# branch.ab "):
            ahead, behind = line.split(" ")[2:4]
            state["ahead"] = int(ahead.lstrip("+"))
            state["behind"] = int(behind.lstrip("-"))
        elif line.startswith(("1 ", "2 ")):
            fields = line.split(" ")
            path = line.split(" ", 9 if line[0] == "2" else 8)[-1]
            state["changes"].append({"xy": fields[1], "path": path.split("\t")[0]})
        elif line.startswith("u "):
            state["changes"].append({"xy": "UU", "path": line.split(" ", 10)[-1]})
        elif line.startswith("? "):
            state["changes"].append({"xy": "??", "path": line[2:]})
    return state


def parse_worktrees(text: str) -> list[dict]:
    """Parse `git worktree list --porcelain`."""
    trees: list[dict] = []
    for line in text.splitlines():
        if line.startswith("worktree "):
            trees.append({"path": line[len("worktree ") :], "branch": None})
        elif line.startswith("branch ") and trees:
            trees[-1]["branch"] = line[len("branch ") :].removeprefix("refs/heads/")
        elif line == "detached" and trees:
            trees[-1]["branch"] = None
    return trees


def _capped(lines: list[str], limit: int | None, flag: str) -> list[str]:
    """Bound one section, naming the exact continuation for what was cut."""
    if limit is None or len(lines) <= limit:
        return lines
    more = len(lines) - limit
    return [*lines[:limit], f"  +{more} more (--{flag} N or --all)"]


def render(state: dict) -> str:
    """Render the state, honouring the caps carried in `state["limits"]`."""
    limits = state.get("limits") or {}
    branch = state["branch"] or f"(detached at {state['head']})"
    lines = [f"branch: {branch}"]
    if state["upstream"]:
        lines.append(
            f"upstream: {state['upstream']} ahead {state['ahead']} behind {state['behind']}"
        )
    else:
        lines.append("upstream: none")
    lines.append(f"changes: {len(state['changes'])}")
    lines.extend(
        _capped(
            [f"  {c['xy']} {c['path']}" for c in state["changes"]],
            limits.get("files"),
            "files",
        )
    )
    lines.append(f"commits: {len(state['commits'])}")
    lines.extend(
        _capped([f"  {c}" for c in state["commits"]], limits.get("commits"), "commits")
    )
    lines.append(f"stashes: {len(state['stashes'])}")
    lines.extend(
        _capped([f"  {s}" for s in state["stashes"]], limits.get("stashes"), "stashes")
    )
    lines.append(f"worktrees: {len(state['worktrees'])}")
    lines.extend(
        _capped(
            [f"  {w['path']} [{w['branch'] or 'detached'}]" for w in state["worktrees"]],
            limits.get("worktrees"),
            "worktrees",
        )
    )
    return "\n".join(lines)


def fetch() -> tuple[int, str]:
    """Refresh the remote-tracking refs so ahead/behind is current (#2233)."""
    return git("fetch", "--quiet")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="state.py", description=(__doc__ or "").split("\n")[0]
    )
    parser.add_argument("--commits", type=int, default=DEFAULT_COMMITS)
    parser.add_argument("--files", type=int, default=MAX_FILES)
    parser.add_argument("--stashes", type=int, default=MAX_STASHES)
    parser.add_argument("--worktrees", type=int, default=MAX_WORKTREES)
    parser.add_argument(
        "--all",
        action="store_true",
        help="print every change, stash and worktree",
    )
    parser.add_argument(
        "--fetch",
        action="store_true",
        help="fetch first so the upstream ahead/behind counts are current",
    )
    parser.add_argument("--json", action="store_true")
    try:
        args = parser.parse_args(argv)
    except SystemExit:
        return 1

    if args.fetch:
        rc, _ = fetch()
        if rc != 0:
            print(
                f"state.py: git fetch failed (exit {rc}); counts are from the last fetch",
                file=sys.stderr,
            )

    rc, status = git("status", "--porcelain=v2", "--branch")
    if rc != 0:
        print("state.py: not a git checkout, or git status failed", file=sys.stderr)
        return 2
    state = parse_status(status)

    # --commits bounds the `git log` range itself: `--all` never means "print
    # the whole history", which is the one list whose full form is unbounded by
    # the repository's age. The other caps only trim what is printed, so a
    # bounded run still reports the true count of each list in its header.
    log_n = max(args.commits, 0)
    _, log = git("log", f"-{log_n}", "--format=%h %s") if log_n else (0, "")
    _, stashes = git("stash", "list", "--format=%gd %s")
    _, worktrees = git("worktree", "list", "--porcelain")
    state["commits"] = [line for line in log.splitlines() if line]
    state["stashes"] = [line for line in stashes.splitlines() if line]
    state["worktrees"] = parse_worktrees(worktrees)
    state["limits"] = {
        "files": None if args.all else max(args.files, 0),
        "stashes": None if args.all else max(args.stashes, 0),
        "worktrees": None if args.all else max(args.worktrees, 0),
    }
    print(json.dumps(state, indent=2) if args.json else render(state))
    return 0


if __name__ == "__main__":
    sys.exit(main())
