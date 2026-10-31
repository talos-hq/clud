"""Lint orchestrator for clud: ruff + banned imports + banned cross tools
+ cargo fmt + clippy."""

from __future__ import annotations

import os
import sys
from pathlib import Path

from ci import process

ROOT = Path(__file__).resolve().parent.parent


def run(cmd: list[str]) -> int:
    from ci.env import clean_env

    return process.run(cmd, cwd=ROOT, env=clean_env()).returncode


def _cargo(subcommand: list[str]) -> list[str]:
    """Return the cargo argv for the configured CI environment."""
    from ci.env import cargo_argv, clean_env

    return cargo_argv(subcommand, env=clean_env())


# How many read-only artifacts to look at before concluding the tree is
# cache-linked. The check runs before every lint, so it stops at the first hit
# in the common (healthy) case and never walks the whole tree.
_READONLY_PROBE_LIMIT = 4000


def _readonly_artifact(target: Path) -> Path | None:
    """First read-only file under `target/debug/deps`, or None.

    soldr's target-tree cache hardlinks artifacts in read-only -- they are
    shared inodes, so making them writable would corrupt the cache for every
    other consumer. That is correct, and `soldr cargo` handles it. A plain
    cargo run against the same tree cannot, and fails trying to write one.
    """
    deps = target / "debug" / "deps"
    if not deps.is_dir():
        return None
    seen = 0
    try:
        for entry in deps.iterdir():
            seen += 1
            if seen > _READONLY_PROBE_LIMIT:
                return None
            try:
                if entry.is_file() and not os.access(entry, os.W_OK):
                    return entry
            except OSError:
                continue
    except OSError:
        return None
    return None


def explain_readonly_failure(argv: list[str]) -> None:
    """Add the missing sentence after a lint step has already failed (#1158).

    **Explains, never gates.** An earlier draft of this checked the same
    condition *before* running and returned non-zero, and that was wrong:
    `soldr cargo build` repopulates the read-only hardlinks on every build, so
    the condition is close to permanent in a soldr-driven tree -- while
    `fmt` does not compile anything and usually succeeds against it anyway.
    Predicting failure from it blocks lint runs that would have passed, which
    is worse than the confusing message this exists to explain.

    So it runs only after a real non-zero exit, and only adds context:

        error: output file .../libserde_core-<hash>.rmeta is not writeable

    names whichever dependency compiled first, changes run to run, and reads
    as a permissions problem in your checkout rather than a toolchain-routing
    one. That cost three separate diagnoses in one session, and once let a
    formatter-dirty commit reach CI because the failure was read as
    environmental.
    """
    if argv and "soldr" in Path(argv[0]).name:
        return
    offender = _readonly_artifact(ROOT / "target")
    if offender is None:
        return
    print(
        f'\nlint: if the failure above says "is not writeable", this is why:\n'
        f"  {offender} is read-only.\n"
        f"  soldr's target-tree cache hardlinks artifacts in read-only, and the\n"
        f"  cargo resolved here is not soldr's, so it cannot rewrite them.\n"
        f"  Re-run the step through soldr, e.g. `soldr cargo clippy --workspace`.\n",
        file=sys.stderr,
    )


#: The Windows triple `--windows` type-checks locally (#1441). soldr prepares
#: the MSVC sysroot, so a Linux or macOS host can run the same clippy the
#: Windows build job runs, before a push spends a `ci-windows` run on it.
WINDOWS_TARGET = "x86_64-pc-windows-msvc"


def clippy_subcommands(windows: bool) -> list[list[str]]:
    """The cargo clippy invocations to run: the host, plus Windows on request."""
    host = ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]
    if not windows:
        return [host]
    target = ["clippy", "--workspace", "--all-targets", "--target", WINDOWS_TARGET]
    return [host, [*target, "--", "-D", "warnings"]]


def main(argv: list[str] | None = None) -> int:  # noqa: C901
    """Run the lint suite.

    `--static-only` skips clippy. CI splits the suite: the platform-independent
    checks (banned imports, banned cross tools, cargo fmt, ruff) run once in
    the `static` job, while
    clippy runs once per target triple inside the build job, where that triple's
    dependency graph is already compiled. Running the whole suite six times, as
    the old per-platform workflows did, meant 6x ruff and 6x cargo fmt for
    checks whose result cannot vary by platform.

    `bash lint` (local, no flags) still runs everything, unchanged.
    `--windows` also runs clippy for `x86_64-pc-windows-msvc` through soldr,
    catching Windows-only `cfg` code before a `ci-windows` run does.
    """
    argv = list(sys.argv[1:] if argv is None else argv)
    static_only = "--static-only" in argv
    windows = "--windows" in argv

    from ci.env import activate

    activate()

    from ci.banned_cross_tools import main as check_banned_cross_tools
    from ci.banned_empty_harnesses import main as check_banned_empty_harnesses
    from ci.banned_home_dir import main as check_banned_home_dir
    from ci.banned_hook_paths import main as check_banned_hook_paths
    from ci.banned_imports import main as check_banned_imports
    from ci.banned_legacy_deletion import main as check_banned_legacy_deletion
    from ci.banned_python3 import main as check_banned_python3
    from ci.banned_skill_sources import main as check_banned_skill_sources
    from ci.banned_wheel_writes import main as check_banned_wheel_writes
    from ci.fetch_fallbacks import main as check_fetch_fallbacks
    from ci.harness_budget import main as check_harness_budget

    # Ordered cheapest-first so the common failure reds out soonest: ruff is a
    # pure-Python scan (~1s), the two banned-* scans are source greps, and only
    # then do we pay for a cargo process. The old order put ruff last,
    # behind fmt.
    if run([sys.executable, "-m", "ruff", "check", "src", "tests", "ci"]) != 0:
        return 1
    # The complexity ratchet (zackees/ci.yml#229, PY-004) covers every tracked
    # Python file, not just the three trees above: bench/, installer/ and the
    # bundled tool assets carry C901 noqa baselines too.
    if run([sys.executable, "-m", "ruff", "check", "--select", "C901", "."]) != 0:
        return 1
    if check_banned_imports() != 0:
        return 1
    if check_banned_legacy_deletion() != 0:
        return 1
    # #637: soldr owns Apple/MSVC cross builds. Static-only by nature (it reads
    # .github/ and ci/), so it belongs in the platform-independent half that CI
    # runs once rather than per triple.
    if check_banned_cross_tools() != 0:
        return 1
    # #847: bundled skills have exactly one source of truth. Another source
    # grep, same tier as the two above — no toolchain, no compile.
    if check_banned_skill_sources() != 0:
        return 1
    # The interpreter is `python` everywhere; clud's shim makes that name work
    # on Linux, macOS and Windows; the versioned name does not exist on Windows.
    if check_banned_python3() != 0:
        return 1
    # #1836: the user's home is resolved only in home.rs; a second resolver
    # disagreed with it on Windows in #1829.
    if check_banned_home_dir() != 0:
        return 1
    # #1334: committed hook configs must resolve clud's helpers from PATH,
    # never a machine-local binary or a `target/` build output.
    if check_banned_hook_paths() != 0:
        return 1
    # #1545: ci/wheel_rewrite.py is the only wheel writer, so no rewriter can
    # drop the scripts' exec bits again.
    if check_banned_wheel_writes() != 0:
        return 1
    # #2449: every published-metadata fetch ends with a GitHub fallback, so a
    # lost Pages domain degrades instead of breaking every installed clud.
    if check_fetch_fallbacks() != 0:
        return 1
    # #1714: a cargo target with no tests must not build a test harness; each
    # one is a full, uncached compile and link on every CI run.
    if check_banned_empty_harnesses() != 0:
        return 1
    # #1726: the `clud` package builds two harnesses, its lib unit tests and
    # the one `integration` target; each extra one links the whole workspace.
    if check_harness_budget() != 0:
        return 1
    # setup-soldr's cargo shim can otherwise omit repository discovery for
    # `.rustfmt.toml` on some runner/architecture combinations. Pin the
    # checked formatting contract explicitly so local and CI layout agree.
    fmt_args = ["fmt", "--all", "--check", "--", "--config-path", ".rustfmt.toml"]
    fmt_argv = _cargo(fmt_args)
    if run(fmt_argv) != 0:
        # #1158: the toolchain's own message names an arbitrary dependency.
        explain_readonly_failure(fmt_argv)
        return 1

    if static_only:
        return 0
    for clippy in clippy_subcommands(windows):
        clippy_argv = _cargo(clippy)
        if run(clippy_argv) != 0:
            explain_readonly_failure(clippy_argv)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
