"""`./push`: attest a committed branch, push it, and open its PR.

    ./push                  attest the host lanes (clippy, dylint), push, open or reuse the PR
    ./push --cross          also attest the cross Dylint lanes (Windows, macOS)
    ./push --test           run `bash test --host` first (Tier 0 tests)
    ./push --no-pr          attest and push only
    ./push --dry-run        print the steps and run nothing

The single entrypoint /grind workers and humans use to publish a change
(talos-hq/clud#2873). `python -m ci.attest` stamps HEAD with prooft
`Ci-Attestation:` trailers for each lane that passes (DD-225), which amends
the commit, so the push uses `--force-with-lease`. A failing lane stops the
push: the gate is red, so fix it. A lane that cannot run on this host is a
failure too; leave it out (the default host lanes run everywhere soldr does)
and hosted CI runs that job.

Exit codes: 0 pushed, 1 a step failed, 2 usage, 3 refused (protected or
detached branch, dirty tree).
"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path

from ci import process

ROOT = Path(__file__).resolve().parents[1]
HOST_LANES = ("clippy", "dylint")
CROSS_LANES = ("dylint-x86_64-pc-windows-msvc", "dylint-aarch64-apple-darwin")
PROTECTED = ("main", "master")


@dataclass(frozen=True)
class Step:
    name: str
    argv: tuple[str, ...]


def plan(
    branch: str, *, python: str, cross: bool, test: bool, pr: bool, base: str = "main"
) -> list[Step]:
    """The steps `./push` runs, in order. Pure, so tests pin it."""
    steps: list[Step] = []
    if test:
        steps.append(Step("test", ("bash", "test", "--host")))
    lanes = HOST_LANES + (CROSS_LANES if cross else ())
    steps.append(Step("attest", (python, "-m", "ci.attest", *lanes)))
    steps.append(
        Step(
            "push",
            ("git", "push", "--force-with-lease", "-u", "origin", f"HEAD:refs/heads/{branch}"),
        )
    )
    if pr:
        steps.append(Step("pr", ("gh", "pr", "create", "--fill", "--base", base, "--head", branch)))
    return steps


def refusal(branch: str, dirty: bool) -> str | None:
    if not branch or branch == "HEAD":
        return "detached HEAD: switch to a feature branch first"
    if branch in PROTECTED:
        return f"refusing to push {branch}: open a feature branch (git switch -c <name>)"
    if dirty:
        return "dirty tree: commit first, prooft attests commits"
    return None


def _out(argv: Sequence[str]) -> tuple[int, str]:
    res = process.run(list(argv), cwd=ROOT, capture_output=True, text=True, check=False)
    return res.returncode, (res.stdout or "").strip()


def _step(argv: Sequence[str]) -> int:
    print(f"./push: $ {' '.join(argv)}", flush=True)
    return process.run(list(argv), cwd=ROOT, check=False).returncode


def main(argv: Sequence[str] | None = None, *, run: Callable[[Sequence[str]], int] = _step) -> int:
    ap = argparse.ArgumentParser(prog="./push", description=__doc__.split("\n\n", 1)[0])
    ap.add_argument("--cross", action="store_true", help="also attest the cross Dylint lanes")
    ap.add_argument("--test", action="store_true", help="run `bash test --host` first")
    ap.add_argument("--no-pr", action="store_true", help="push without opening a PR")
    ap.add_argument("--base", default="main")
    ap.add_argument("--dry-run", action="store_true")
    ns = ap.parse_args(argv)
    _, branch = _out(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    _, status = _out(["git", "status", "--porcelain", "--untracked-files=no"])
    why = refusal(branch, bool(status))
    if why:
        print(f"./push: {why}", file=sys.stderr)
        return 3
    steps = plan(
        branch, python=sys.executable, cross=ns.cross, test=ns.test, pr=not ns.no_pr, base=ns.base
    )
    for step in steps:
        if ns.dry_run:
            print(f"./push: would run {step.name}: {' '.join(step.argv)}")
            continue
        if step.name == "pr":
            code, url = _out(["gh", "pr", "view", branch, "--json", "url", "-q", ".url"])
            if code == 0 and url:
                print(f"./push: PR {url}")
                continue
        code = run(step.argv)
        if code != 0:
            print(
                f"./push: {step.name} failed (exit {code}); nothing after it ran", file=sys.stderr
            )
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
