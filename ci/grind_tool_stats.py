"""Tool-call statistics for `/grind` workflow runs (#1339).

Usage:
  python -m ci.grind_tool_stats [--json] [RUN_DIR ...]

Each RUN_DIR is a Claude Code workflow run directory
(`~/.claude/projects/<project>/<session>/subagents/workflows/wf_*`) holding
one `agent-*.jsonl` transcript and `agent-*.meta.json` per agent. With no
arguments every such directory under `~/.claude/projects` is read.

It reports the numbers #1339's Data table is built from: tool calls per
role, repeated and sliced `Read`s and their share of tool-result context,
repeated `gh issue view`s, improvised waits, Python edit scripts, failed
`Edit`s, manual CI polling, git state probes, shell `grep`/`ls` against the
Grep tool, and how often the bundled replacements are used. Run it over a
batch of runs before and after a change and compare.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path

WAIT = re.compile(r"\bsleep\s+\d|\b(until|while)\b[^;]*;\s*do\b")
PY_EDIT = re.compile(r"\bpython(?:3)?\s+(-c\b|-\s*<<|<<)")
GIT_PROBE = re.compile(r"\bgit\s+(?:-C\s+\S+\s+)?(log|status|diff|fetch|branch)\b")
REPO = r"(?:-R\s+\S+\s+|--repo\s+\S+\s+)?"
CI_POLL = re.compile(r"\bgh\s+" + REPO + r"(pr\s+checks|run\s+view|run\s+list)\b")
ISSUE_VIEW = re.compile(r"\bgh\s+issue\s+view\s+" + REPO + r"#?(\d+)")
SHELL_SEARCH = re.compile(r"(?:^|[;&|(]\s*)(grep|ls)\b")
BUNDLED = (
    "git/state.py",
    "code/outline.py",
    "code/slice.py",
    "code/patch.py",
    "code/ast_grep.py",
    "pr_merge_watch",
)


@dataclass
class Stats:
    runs: int = 0
    agents: Counter = field(default_factory=Counter)
    calls: Counter = field(default_factory=Counter)
    counts: Counter = field(default_factory=Counter)
    result_chars: Counter = field(default_factory=Counter)
    largest_result: int = 0


def role_of(meta: dict) -> str:
    kind = str(meta.get("agentType") or "")
    if kind.startswith("grind-"):
        return kind.removeprefix("grind-")
    return str(meta.get("description") or kind or "unknown").split(":")[0]


def records(path: Path):
    with path.open(encoding="utf-8", errors="replace") as lines:
        for line in lines:
            try:
                yield json.loads(line)
            except ValueError:
                continue


def blocks(record: dict, kind: str):
    content = (record.get("message") or {}).get("content")
    if isinstance(content, list):
        yield from (b for b in content if isinstance(b, dict) and b.get("type") == kind)


def result_text(block: dict) -> str:
    content = block.get("content")
    if isinstance(content, list):
        return "".join(str(c.get("text", "")) for c in content if isinstance(c, dict))
    return str(content or "")


def count_bash(stats: Stats, command: str, issues: Counter) -> None:
    c = stats.counts
    c["bash_wait_loops"] += bool(WAIT.search(command))
    c["bash_python_edit_scripts"] += bool(PY_EDIT.search(command))
    c["bash_git_probes"] += len(GIT_PROBE.findall(command))
    c["bash_ci_polls"] += len(CI_POLL.findall(command))
    for name in SHELL_SEARCH.findall(command):
        c[f"bash_{name}"] += 1
    for issue in ISSUE_VIEW.findall(command):
        c["gh_issue_view"] += 1
        c["gh_issue_view_repeat"] += issues[issue] > 0
        issues[issue] += 1
    for tool in BUNDLED:
        c[f"uses_{tool}"] += tool in command


def count_call(stats: Stats, block: dict, reads: Counter, issues: Counter) -> None:
    name = str(block.get("name"))
    args = block.get("input") or {}
    if name == "Read":
        path = str(args.get("file_path"))
        stats.counts["read"] += 1
        stats.counts["read_repeat"] += reads[path] > 0
        stats.counts["read_sliced"] += "offset" in args or "limit" in args
        reads[path] += 1
    elif name == "Bash":
        count_bash(stats, str(args.get("command", "")), issues)
    elif name == "Grep":
        stats.counts["grep_tool"] += 1
    elif name == "ToolSearch" and "monitor" in str(args.get("query", "")).lower():
        stats.counts["toolsearch_monitor"] += 1


def add_agent(stats: Stats, transcript: Path, reads: Counter, issues: Counter) -> None:
    meta_path = transcript.with_name(transcript.name.removesuffix(".jsonl") + ".meta.json")
    try:
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        meta = {}
    role = role_of(meta)
    stats.agents[role] += 1
    names: dict[str, str] = {}
    for record in records(transcript):
        for block in blocks(record, "tool_use"):
            names[str(block.get("id"))] = str(block.get("name"))
            stats.calls[role] += 1
            count_call(stats, block, reads, issues)
        for block in blocks(record, "tool_result"):
            tool = names.get(str(block.get("tool_use_id")), "?")
            size = len(result_text(block))
            stats.result_chars[tool] += size
            stats.largest_result = max(stats.largest_result, size)
            stats.counts["edit_failed"] += tool == "Edit" and bool(block.get("is_error"))


def collect(run_dirs: list[Path]) -> Stats:
    stats = Stats()
    for run in run_dirs:
        transcripts = sorted(run.glob("agent-*.jsonl"))
        if not transcripts:
            continue
        stats.runs += 1
        reads: Counter = Counter()
        issues: Counter = Counter()
        for transcript in transcripts:
            add_agent(stats, transcript, reads, issues)
    return stats


def report(stats: Stats) -> dict:
    total_chars = sum(stats.result_chars.values()) or 1
    return {
        "runs": stats.runs,
        "agents_by_role": dict(stats.agents),
        "calls_per_agent_by_role": {
            role: round(stats.calls[role] / n, 1) for role, n in stats.agents.items() if n
        },
        "read_result_share": round(stats.result_chars["Read"] / total_chars, 3),
        "largest_result_chars": stats.largest_result,
        **dict(sorted(stats.counts.items())),
    }


def default_dirs() -> list[Path]:
    return sorted((Path.home() / ".claude" / "projects").glob("*/*/subagents/workflows/wf_*"))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--json", action="store_true")
    parser.add_argument("runs", nargs="*", type=Path)
    args = parser.parse_args(argv)
    result = report(collect(args.runs or default_dirs()))
    if args.json:
        print(json.dumps(result, indent=2))
        return 0
    for key, value in result.items():
        print(f"{key:32} {value}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
