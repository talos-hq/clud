#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "running-process==4.10.1",
# ]
# ///
# managed-by: clud
"""detect_gate.py: the local gate and landing strategy for a /grind coordinator run.

Usage:
  detect_gate.py [--repo PATH] [--github-repo OWNER/REPO] [--base BRANCH] [--no-network]

Prints one JSON object the router copies into the coordinator's args
(talos-hq/clud#2873):

  {"gate": {"push": "./push" | null, "commands": [...]},
   "land": "merge_queue" | "merge" | "auto", "kind": "push_script" | "rust" | "scripts" | "none",
   "why": [...]}

Order of preference, fastest proof first:
  1. A `./push` script: it runs the repository's attested lanes and pushes,
     so the attestation lands on the pushed head and hosted CI can reduce
     the proven work. Nothing else is needed before it.
  2. A Rust workspace: `soldr lint --host-only` and host unit tests through
     soldr in porcelain mode. Cross-target lanes stay in hosted CI.
  3. The repository's `./lint` and `./test` scripts.

`land` is `merge_queue` when the base branch's rules include a merge queue,
`merge` when they do not, and `auto` with --no-network or when GitHub cannot
be read (the chaperone's pr_merge.py picks the right mutation either way).
Exit codes: 0 printed, 2 usage error.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from running_process import PIPE, RunningProcess, TimeoutExpired


def _gh_json(argv: list[str]) -> object | None:
    try:
        res = RunningProcess.run(
            ["gh", *argv], stdout=PIPE, stderr=PIPE, text=True, timeout=30, check=False
        )
    except (TimeoutExpired, OSError):
        return None
    if res.returncode != 0:
        return None
    try:
        return json.loads(res.stdout or "null")
    except json.JSONDecodeError:
        return None


def merge_queue(github_repo: str, base: str) -> str:
    rules = _gh_json(["api", f"repos/{github_repo}/rules/branches/{base}"])
    if not isinstance(rules, list):
        return "auto"
    return (
        "merge_queue"
        if any(isinstance(r, dict) and r.get("type") == "merge_queue" for r in rules)
        else "merge"
    )


def is_rust(repo: Path) -> bool:
    cargo = repo / "Cargo.toml"
    if not cargo.is_file():
        return False
    text = cargo.read_text(encoding="utf-8", errors="replace")
    return "[workspace]" in text or "[package]" in text


def detect(repo: Path) -> dict[str, object]:
    why: list[str] = []
    push = repo / "push"
    if push.is_file():
        why.append(
            "./push exists: it proves the attested lanes and pushes, so it is the whole gate"
        )
        return {"gate": {"push": "./push", "commands": []}, "kind": "push_script", "why": why}
    if is_rust(repo):
        why.append("Rust workspace without ./push: host lint and host unit tests through soldr")
        cmds = ["soldr lint --host-only", "SOLDR_PORCELAIN=1 soldr cargo test --workspace"]
        return {"gate": {"push": None, "commands": cmds}, "kind": "rust", "why": why}
    cmds = [f"./{name}" for name in ("lint", "test") if (repo / name).is_file()]
    if cmds:
        why.append(
            "repository scripts; the router may narrow ./test to a faster mode from the question round"
        )
        return {"gate": {"push": None, "commands": cmds}, "kind": "scripts", "why": why}
    why.append("no ./push, Rust workspace or ./lint/./test: the worker uses its focused test only")
    return {"gate": {"push": None, "commands": []}, "kind": "none", "why": why}


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    ap.add_argument("--repo", type=Path, default=Path.cwd())
    ap.add_argument("--github-repo", help="OWNER/REPO, to read the base branch's merge-queue rule")
    ap.add_argument("--base", default="main")
    ap.add_argument("--no-network", action="store_true")
    ns = ap.parse_args(argv)
    if not ns.repo.is_dir():
        print(f"not a directory: {ns.repo}", file=sys.stderr)
        return 2
    out = detect(ns.repo.resolve())
    out["land"] = (
        "auto" if ns.no_network or not ns.github_repo else merge_queue(ns.github_repo, ns.base)
    )
    print(json.dumps(out, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
