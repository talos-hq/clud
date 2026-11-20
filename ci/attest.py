"""Attest the hosted Clippy and Dylint lanes locally with prooft (DD-225).

    python -m ci.attest                 run every lane at once; stamp HEAD for each that passes
    python -m ci.attest clippy          run only the named lanes
    python -m ci.attest --serial        one lane at a time (lower peak load)
    python -m ci.attest --list          print the lanes and their commands
    python -m ci.attest --exec LANE     run one lane's commands in this process

Each lane's output goes to target/attest/LANE.log; a failing lane prints its
tail.

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
import re
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

from running_process import RunningProcess

from ci import process

ROOT = Path(__file__).resolve().parents[1]
POLICY = ROOT / "ci-attestations.yml"
# Each lane's full output; target/ is ignored, so writing here keeps the tree clean.
LOG_DIR = ROOT / "target" / "attest"
# Per-lane cargo target directories for the cross Dylint lanes (see below).
CROSS_TARGET_ROOT = ROOT / "target" / "attest-cross"
LINUX = "x86_64-unknown-linux-gnu"
# Must equal `SOLDR_DYLINT_TOOLCHAIN` in .github/workflows/_dylint.yml.
DYLINT_TOOLCHAIN = "nightly-2026-08-15"
DYLINT_CROSS_TARGETS = ("x86_64-pc-windows-msvc", "aarch64-apple-darwin")
DYLINT_CRATES = ("dylints/ban_manual_slash_normalize", "dylints/enforce_platform_boundary")
# Never inherited by a lane: hosted jobs do not set them, and they change what
# a nested tool sees. SOLDR_PORCELAIN replaces the stdout of the `cargo
# metadata` cargo-dylint runs through soldr's shim, so Dylint finds "No
# libraries" and exits 0 without linting anything.
HOST_ONLY_ENV = ("SOLDR_PORCELAIN",)
# soldr's verdict ledger stays on (DD-228): a replay re-prints the recorded
# output of a pass on byte-identical inputs, so a lane right after `bash lint`
# (same host Dylint command) costs seconds. The ledger once recorded a hollow
# porcelain Dylint run as a pass (forge-ci/soldr#4175); the Dylint guard below
# reads replayed output too, and reruns a rejected replay with verdicts off.
LANE_ENV: tuple[tuple[str, str], ...] = ()
_VERDICT_REPLAY = "inputs match a passing run"
_DYLINT_ENV = (
    ("SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS", "1"),
    ("SOLDR_DYLINT_TOOLCHAIN", DYLINT_TOOLCHAIN),
)


# A Dylint pass that exits 0 proves nothing unless it loaded every library
# and ran its check. Under SOLDR_PORCELAIN it printed "No libraries were
# found." and exited 0 (forge-ci/soldr#4175), and a verdict replay prints the
# recorded output of whatever pass it replays, so the guard reads the output
# either way.
_DYLINT_LIBRARY = re.compile(r"Building workspace metadata entry `([^`]+)`")
_DYLINT_CHECKED = re.compile(r"Checking with toolchain `")
_DYLINT_EMPTY = "No libraries were found"


def dylint_libraries(root: Path = ROOT) -> frozenset[str]:
    """`[workspace.metadata.dylint] libraries = [{ path = "dylints/*" }]`."""
    return frozenset(path.parent.name for path in (root / "dylints").glob("*/Cargo.toml"))


def dylint_pass_problem(lines: Sequence[str], libraries: frozenset[str]) -> str | None:
    """Why a successful `soldr dylint --all` run is not evidence, or None."""
    if not libraries:
        return "no Dylint libraries under dylints/"
    if any(_DYLINT_EMPTY in line for line in lines):
        return "Dylint found no libraries and linted nothing (a hollow pass)"
    built = {match.group(1) for line in lines for match in _DYLINT_LIBRARY.finditer(line)}
    missing = sorted(libraries - built)
    if missing:
        return f"Dylint did not load {', '.join(missing)} ({len(built)}/{len(libraries)} libraries)"
    if not any(_DYLINT_CHECKED.search(line) for line in lines):
        return "Dylint loaded its libraries but never ran the check"
    return None


@dataclass(frozen=True)
class Step:
    """One command of a lane: argv, a repo-relative cwd, env to set and unset.

    `dylint_guard` marks a `soldr dylint --all` pass whose output must prove
    it linted (`dylint_pass_problem`).
    """

    argv: tuple[str, ...]
    cwd: str = "."
    env: tuple[tuple[str, str], ...] = ()
    unset: tuple[str, ...] = field(default=())
    dylint_guard: bool = False

    def environment(self, base: dict[str, str]) -> dict[str, str]:
        dropped = {*HOST_ONLY_ENV, *self.unset}
        env = {key: value for key, value in base.items() if key not in dropped}
        env.update(LANE_ENV)
        env.update(self.env)
        return env


def _soldr_dylint(*args: str, env: tuple[tuple[str, str], ...] = ()) -> Step:
    # `env -u RUSTUP_TOOLCHAIN` in the workflow: soldr selects the nightly.
    return Step(
        ("soldr", "dylint", *args),
        env=(*_DYLINT_ENV, *env),
        unset=("RUSTUP_TOOLCHAIN",),
        dylint_guard=args[:1] == ("--all",),
    )


def _clippy_lane(python: str) -> tuple[Step, ...]:
    """`Clippy linux-x64`: _build-target.yml's Clippy and Doc tests steps."""
    xbuild = (python, "-m", "ci.xbuild")
    common = ("--target", LINUX, "--strategy", "soldr")
    return (Step((*xbuild, "clippy", *common)), Step((*xbuild, "doctest", *common)))


def dylint_driver_path(environ: Mapping[str, str]) -> Path:
    """The verified Dylint driver root: the caller's `DYLINT_DRIVER_PATH`, else soldr's.

    soldr resolves the same way (`dylint_driver.rs::driver_root`) and stages
    each prebuilt driver there under both `<channel>-<host>` and the short
    `<channel>` name (soldr 0.9.47+). It exports the variable only to the
    cargo-dylint it launches; setup-soldr exports it job-wide on a runner.
    """
    explicit = environ.get("DYLINT_DRIVER_PATH")
    if explicit:
        return Path(explicit)
    home = environ.get("SOLDR_HOME")
    return (Path(home) if home else Path.home() / ".soldr") / "dylint" / "drivers"


def check_dylint_driver(environ: Mapping[str, str]) -> int:
    """`_dylint.yml`'s `test -f` checks: the short-name driver dylint_testing asks for.

    Without it, `enforce_platform_boundary`'s UI test builds `dylint_driver`
    from crates.io, which fails with "Could not parse toolchain".
    """
    driver = dylint_driver_path(environ) / DYLINT_TOOLCHAIN / "dylint-driver"
    if driver.is_file():
        print(f"attest: Dylint driver {driver}", flush=True)
        return 0
    print(
        f"attest: no verified Dylint driver at {driver}; run `soldr dylint prepare` "
        "(soldr 0.9.47+ stages the short channel name) or set DYLINT_DRIVER_PATH",
        file=sys.stderr,
    )
    return 1


def _dylint_host_lane(python: str, environ: Mapping[str, str]) -> tuple[Step, ...]:
    """`Dylint (host)`: crate formatting and tests, then the workspace pass."""
    nightly = (("RUSTUP_TOOLCHAIN", DYLINT_TOOLCHAIN),)
    # The UI test launches nested Cargo through dylint_testing, outside
    # soldr's Dylint front door, so it needs the driver root that
    # setup-soldr exports on a runner (`_dylint.yml`'s Test dylint crate).
    driver = (("DYLINT_DRIVER_PATH", str(dylint_driver_path(environ))),)
    fmt = tuple(
        Step(("soldr", "--no-cache", "cargo", "fmt", "--all", "--", "--check"), cwd=crate)
        for crate in DYLINT_CRATES
    )
    tests = (
        Step(("soldr", "--no-cache", "cargo", "test"), cwd=DYLINT_CRATES[0], env=nightly),
        Step(
            ("soldr", "cargo", "test"),
            cwd=DYLINT_CRATES[1],
            env=(
                *nightly,
                *driver,
                ("CARGO_TARGET_DIR", str(ROOT / "target" / "dylint-tests")),
            ),
        ),
    )
    return (
        _soldr_dylint("prepare"),
        Step((python, "-m", "ci.attest", "--check-driver"), env=driver),
        Step(("soldr", "rustup", "component", "add", "--toolchain", DYLINT_TOOLCHAIN, "rustfmt")),
        *fmt,
        *tests,
        _soldr_dylint("--all", "--", "--workspace", "--all-targets"),
    )


def _dylint_cross_lane(target: str) -> tuple[Step, ...]:
    """`Dylint (<target>)`: the check-only cross pass, warnings denied.

    Each cross lane gets its own target directory. cargo-dylint checks under
    `<target-dir>/dylint/target/<toolchain>`, and a `--target` build also
    locks that directory's host layout, so lanes sharing it run one at a
    time. The hosted legs run on separate runners and never share one.
    """
    env = (("RUSTFLAGS", "-D warnings"), ("CARGO_TARGET_DIR", str(CROSS_TARGET_ROOT / target)))
    return (
        _soldr_dylint("prepare", "--target", target, env=env),
        _soldr_dylint("--all", "--", "--workspace", "--all-targets", "--target", target, env=env),
    )


def lanes(
    python: str = sys.executable, environ: Mapping[str, str] | None = None
) -> dict[str, tuple[Step, ...]]:
    """Every lane in ci-attestations.yml, by name, in run order."""
    environ = os.environ if environ is None else environ
    out = {"clippy": _clippy_lane(python), "dylint": _dylint_host_lane(python, environ)}
    for target in DYLINT_CROSS_TARGETS:
        out[f"dylint-{target}"] = _dylint_cross_lane(target)
    return out


def _run_step(step: Step, env: dict[str, str]) -> tuple[int, list[str]]:
    """Run one step, streaming its merged output; keep the lines a guard reads."""
    if not step.dylint_guard:
        return process.run(list(step.argv), cwd=ROOT / step.cwd, env=env).returncode, []
    proc = RunningProcess(list(step.argv), cwd=ROOT / step.cwd, env=env)
    lines = []
    for line in proc.line_iter(timeout=None):
        print(line, flush=True)
        lines.append(line)
    return proc.wait(), lines


def _guarded_step(
    name: str, step: Step, env: dict[str, str], libraries: frozenset[str]
) -> tuple[int, str | None]:
    """Run a step; for a Dylint pass, also return why its output is not evidence.

    A replayed verdict that the guard rejects is rerun once with the ledger
    off, so a poisoned record costs a real run instead of a failed lane.
    """
    code, lines = _run_step(step, env)
    if code != 0 or not step.dylint_guard:
        return code, None
    problem = dylint_pass_problem(lines, libraries)
    if problem and any(_VERDICT_REPLAY in line for line in lines):
        print(f"attest: lane {name}: replayed verdict rejected ({problem}); rerunning", flush=True)
        code, lines = _run_step(step, {**env, "SOLDR_VERDICTS": "off"})
        problem = dylint_pass_problem(lines, libraries) if code == 0 else None
    return code, problem


def exec_lane(
    name: str,
    steps: Sequence[Step],
    *,
    environ: dict[str, str] | None = None,
    libraries: frozenset[str] | None = None,
) -> int:
    """Run one lane's steps in order; the first failure, or a hollow Dylint pass, ends it."""
    base = dict(os.environ if environ is None else environ)
    libs = dylint_libraries() if libraries is None else libraries
    for step in steps:
        print(f"+ [{name}] ({step.cwd}) {' '.join(step.argv)}", flush=True)
        code, problem = _guarded_step(name, step, step.environment(base), libs)
        if problem:
            print(f"attest: lane {name}: {problem}; it stamps nothing", file=sys.stderr)
            return 1
        if code != 0:
            print(f"attest: lane {name} failed (exit {code}); it stamps nothing", file=sys.stderr)
            return code
    return 0


def _prooft(*args: str) -> int:
    return process.run(
        [sys.executable, "-m", "prooft.cli", "-C", str(ROOT), *args], cwd=ROOT
    ).returncode


@dataclass(frozen=True)
class LaneResult:
    name: str
    code: int
    secs: int


def run_lane_child(name: str, log_dir: Path = LOG_DIR) -> LaneResult:
    """Run `--exec NAME` in a child, teeing its merged output to `<log_dir>/NAME.log`."""
    log_dir.mkdir(parents=True, exist_ok=True)
    log = log_dir / f"{name}.log"
    started = time.monotonic()
    proc = RunningProcess(
        [sys.executable, "-m", "ci.attest", "--exec", name], cwd=ROOT, env=dict(os.environ)
    )
    with log.open("w", encoding="utf-8", errors="replace") as handle:
        for line in proc.line_iter(timeout=None):
            handle.write(line + "\n")
            handle.flush()
    code = proc.wait()
    secs = int(time.monotonic() - started)
    verdict = "passed" if code == 0 else f"FAILED (exit {code})"
    print(f"attest: lane {name} {verdict} in {secs} s; log {log}", flush=True)
    if code != 0:
        tail = log.read_text(encoding="utf-8", errors="replace").splitlines()[-40:]
        print("\n".join(f"  [{name}] {line}" for line in tail), flush=True)
    return LaneResult(name, code, secs)


def run_lanes(
    names: Sequence[str],
    *,
    parallel: bool = True,
    run: Callable[[str], LaneResult] = run_lane_child,
) -> list[LaneResult]:
    """Every lane at once (soldr's daemon caps the compiles), or one after another."""
    if not parallel or len(names) < 2:
        return [run(name) for name in names]
    with ThreadPoolExecutor(max_workers=len(names)) as pool:
        return list(pool.map(run, names))


def attest(
    names: Sequence[str],
    *,
    parallel: bool = True,
    root: Path = ROOT,
    run: Callable[[str], LaneResult] = run_lane_child,
) -> int:
    """Run the lanes, then stamp HEAD once per lane that passed.

    `prooft run` cannot share HEAD between concurrent lanes (each amends it),
    so this applies its rules around the whole batch instead: a clean tree
    before, the same HEAD and a clean tree after, and only then one
    `LaneRun.record` per passing lane. Recording amends only the message, so
    every stamp binds the same tree and parents.
    """
    import prooft

    repo = prooft.open(root)
    if not repo.git.is_clean():
        print("attest: dirty tree: commit first, prooft attests commits", file=sys.stderr)
        return 2
    before = repo.commit().sha
    results = run_lanes(names, parallel=parallel, run=run)
    if repo.commit().sha != before or not repo.git.is_clean():
        print(
            "attest: a lane moved HEAD or changed tracked files; nothing stamped", file=sys.stderr
        )
        return 1
    policy = repo.policy()
    for result in results:
        if result.code == 0:
            repo.lane(result.name, policy=policy).record(secs=result.secs)
    failed = [r.name for r in results if r.code != 0]
    if root == ROOT:
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
    if args == ["--check-driver"]:
        return check_dylint_driver(os.environ)
    if args[:1] == ["--exec"]:
        if len(args) != 2 or args[1] not in table:
            print(f"usage: python -m ci.attest --exec {{{','.join(table)}}}", file=sys.stderr)
            return 2
        return exec_lane(args[1], table[args[1]])
    serial = "--serial" in args
    args = [arg for arg in args if arg != "--serial"]
    unknown = [name for name in args if name not in table]
    if unknown:
        print(
            f"attest: unknown lane(s) {', '.join(unknown)}; known: {', '.join(table)}",
            file=sys.stderr,
        )
        return 2
    return attest(args or list(table), parallel=not serial)


if __name__ == "__main__":
    sys.exit(main())
