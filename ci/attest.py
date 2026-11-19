"""Attest the hosted Clippy and Dylint lanes locally with prooft (DD-225).

    python -m ci.attest                 run every lane; stamp HEAD for each that passes
    python -m ci.attest clippy          run only the named lanes
    python -m ci.attest --list          print the lanes and their commands
    python -m ci.attest --exec LANE     run one lane's commands (what `prooft run` executes)

Commit first: prooft refuses a dirty tree, and a lane that changes a tracked
file or moves HEAD stamps nothing. Each passing lane amends HEAD's message
with its `Ci-Attestation:` trailers (forge-ci/prooft, pinned in pyproject),
so run this before the first push, or push with `--force-with-lease` after.

Every lane runs the same commands as the hosted job or Dylint leg it stands
in for (`tests/test_attest.py` pins them against the workflows). A lane that
fails, or cannot run on this host, stamps nothing, and CI runs that job.
"""

from __future__ import annotations

import os
import sys
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path

from ci import process

ROOT = Path(__file__).resolve().parents[1]
POLICY = ROOT / "ci-attestations.yml"
LINUX = "x86_64-unknown-linux-gnu"
# Must equal `SOLDR_DYLINT_TOOLCHAIN` in .github/workflows/_dylint.yml.
DYLINT_TOOLCHAIN = "nightly-2026-08-15"
DYLINT_CROSS_TARGETS = ("x86_64-pc-windows-msvc", "aarch64-apple-darwin")
DYLINT_CRATES = ("dylints/ban_manual_slash_normalize", "dylints/enforce_platform_boundary")
_DYLINT_ENV = (
    ("SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS", "1"),
    ("SOLDR_DYLINT_TOOLCHAIN", DYLINT_TOOLCHAIN),
)


@dataclass(frozen=True)
class Step:
    """One command of a lane: argv, a repo-relative cwd, env to set and unset."""

    argv: tuple[str, ...]
    cwd: str = "."
    env: tuple[tuple[str, str], ...] = ()
    unset: tuple[str, ...] = field(default=())

    def environment(self, base: dict[str, str]) -> dict[str, str]:
        env = {key: value for key, value in base.items() if key not in self.unset}
        env.update(self.env)
        return env


def _soldr_dylint(*args: str, env: tuple[tuple[str, str], ...] = ()) -> Step:
    # `env -u RUSTUP_TOOLCHAIN` in the workflow: soldr selects the nightly.
    return Step(("soldr", "dylint", *args), env=(*_DYLINT_ENV, *env), unset=("RUSTUP_TOOLCHAIN",))


def _clippy_lane(python: str) -> tuple[Step, ...]:
    """`Clippy linux-x64`: _build-target.yml's Clippy and Doc tests steps."""
    xbuild = (python, "-m", "ci.xbuild")
    common = ("--target", LINUX, "--strategy", "soldr")
    return (Step((*xbuild, "clippy", *common)), Step((*xbuild, "doctest", *common)))


def _dylint_host_lane() -> tuple[Step, ...]:
    """`Dylint (host)`: crate formatting and tests, then the workspace pass."""
    nightly = (("RUSTUP_TOOLCHAIN", DYLINT_TOOLCHAIN),)
    fmt = tuple(
        Step(("soldr", "--no-cache", "cargo", "fmt", "--all", "--", "--check"), cwd=crate)
        for crate in DYLINT_CRATES
    )
    tests = (
        Step(("soldr", "--no-cache", "cargo", "test"), cwd=DYLINT_CRATES[0], env=nightly),
        Step(
            ("soldr", "cargo", "test"),
            cwd=DYLINT_CRATES[1],
            env=(*nightly, ("CARGO_TARGET_DIR", str(ROOT / "target" / "dylint-tests"))),
        ),
    )
    return (
        _soldr_dylint("prepare"),
        Step(("soldr", "rustup", "component", "add", "--toolchain", DYLINT_TOOLCHAIN, "rustfmt")),
        *fmt,
        *tests,
        _soldr_dylint("--all", "--", "--workspace", "--all-targets"),
    )


def _dylint_cross_lane(target: str) -> tuple[Step, ...]:
    """`Dylint (<target>)`: the check-only cross pass, warnings denied."""
    deny = (("RUSTFLAGS", "-D warnings"),)
    return (
        _soldr_dylint("prepare", "--target", target, env=deny),
        _soldr_dylint("--all", "--", "--workspace", "--all-targets", "--target", target, env=deny),
    )


def lanes(python: str = sys.executable) -> dict[str, tuple[Step, ...]]:
    """Every lane in ci-attestations.yml, by name, in run order."""
    out = {"clippy": _clippy_lane(python), "dylint": _dylint_host_lane()}
    for target in DYLINT_CROSS_TARGETS:
        out[f"dylint-{target}"] = _dylint_cross_lane(target)
    return out


def exec_lane(name: str, steps: Sequence[Step], *, environ: dict[str, str] | None = None) -> int:
    """Run one lane's steps in order; the first failure ends it."""
    base = dict(os.environ if environ is None else environ)
    for step in steps:
        print(f"+ [{name}] ({step.cwd}) {' '.join(step.argv)}", flush=True)
        code = process.run(
            list(step.argv), cwd=ROOT / step.cwd, env=step.environment(base)
        ).returncode
        if code != 0:
            print(f"attest: lane {name} failed (exit {code}); it stamps nothing", file=sys.stderr)
            return code
    return 0


def _prooft(*args: str) -> int:
    return process.run(
        [sys.executable, "-m", "prooft.cli", "-C", str(ROOT), *args], cwd=ROOT
    ).returncode


def attest(names: Sequence[str]) -> int:
    """`prooft run` each lane, so a pass stamps HEAD; report every gate after."""
    failed = []
    for name in names:
        if _prooft("run", name, "--", sys.executable, "-m", "ci.attest", "--exec", name) != 0:
            failed.append(name)
    _prooft("verify")
    if failed:
        print(f"attest: not attested (CI runs them): {', '.join(failed)}", file=sys.stderr)
        return 1
    return 0


def _print_lanes(table: dict[str, tuple[Step, ...]]) -> int:
    for name, steps in table.items():
        print(name)
        for step in steps:
            print(f"  ({step.cwd}) {' '.join(step.argv)}")
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    table = lanes()
    if args[:1] == ["--list"]:
        return _print_lanes(table)
    if args[:1] == ["--exec"]:
        if len(args) != 2 or args[1] not in table:
            print(f"usage: python -m ci.attest --exec {{{','.join(table)}}}", file=sys.stderr)
            return 2
        return exec_lane(args[1], table[args[1]])
    unknown = [name for name in args if name not in table]
    if unknown:
        print(
            f"attest: unknown lane(s) {', '.join(unknown)}; known: {', '.join(table)}",
            file=sys.stderr,
        )
        return 2
    return attest(args or list(table))


if __name__ == "__main__":
    sys.exit(main())
