"""Pack/unpack the CI test bundle that crosses from a build job to an exec job.

A bundle is everything an exec runner needs to test one target triple without a
Rust toolchain: the workspace binaries, every `cargo test --no-run` harness
binary, and the dev wheel.

Why a tar rather than uploading the files directly: `actions/upload-artifact`
round-trips through a zip and **drops the POSIX executable bit**. Every binary
in the bundle would arrive on the macOS/Linux exec runner as mode 0644 and fail
with EACCES. Tar preserves permissions, so the archive is the unit of transfer
and the zip is just a container. (This repo has already been bitten by exactly
this failure mode from a different direction -- see the soldr#1880 note in
.github/actions/setup-build/action.yml.)

Layout produced under `--dest` after `unpack`:

    bundle/
      manifest.json                     triple, profile, sha, harness list
      target/<profile-dir>/             CARGO_TARGET_DIR points here, so
        clud, mock-agent, ...           crates/clud-bin/tests/integration/common/mod.rs:33
        mock-agent                      resolves mock_agent_path() with no
                                        source change
      tests/                            cargo test harness binaries
      dist/                             the dev wheel (test_trampoline.py)

Design: docs/architecture/ci.md
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"

#: Scratch root for CI staging. NOT `ROOT / "build"` -- `build` is a tracked
#: shell script at the repo root (the `bash build` entrypoint), so creating a
#: directory of that name fails with NotADirectoryError on every platform.
CI_SCRATCH = ROOT / ".ci-build"

#: Workspace binaries the test suites resolve by name. `mock-agent`,
#: `daemon-stub` and the probe binaries come from testbins/.
WORKSPACE_BINARIES = (
    # The helper names (`clud-cmd-scan`, `clud-shim`, ...) are argv[0] aliases
    # of `clud` (#1551); `ci.aliases.materialize` creates them on the exec side.
    "clud",
    "clud-ctrlc-probe",
    "daemon-stub",
    "mock-agent",
    # #1067: `crates/tap/tests/cli.rs` runs this one. Without it in the bundle
    # the harness falls back to its compile-time `CARGO_BIN_EXE_tap`, which is
    # a path on the *build* runner and does not exist on the exec runner --
    # `Spawn(NotFound)` on every lane at once.
    "tap",
)
#: Written by `ci.xbuild compile --with-tests`; the parsed `executable` fields
#: from `cargo test --no-run --message-format=json`.
HARNESS_MANIFEST = "test-harnesses.json"

ARCHIVE_NAME = "bundle.tar.gz"

#: Harnesses whose entire file is `#![cfg(windows)]` / `#![cfg(unix)]`. They are
#: still *built* for every target -- a fully cfg'd-out harness compiles to a
#: binary that reports zero tests -- so shipping them is pure transfer cost on a
#: bundle where each harness statically links the whole workspace. Dropping them
#: is a size optimization only: an entry listed here can never contain a test
#: that would have run on the excluded platform.
WINDOWS_ONLY = ("ctrlc_windows_events", "utf8_codepage", "shift_enter_dual_reader")
UNIX_ONLY = ("ctrlc_signal_kinds",)


def _harness_runs_on(stem: str, target: str) -> bool:
    """Whether a harness can contain runnable tests for `target`.

    Cargo appends a hash to harness filenames (`symbols-1a2b3c`), so match on
    the leading stem rather than the whole name.
    """
    name = stem.rsplit("-", 1)[0]
    if "windows" in target:
        return name not in UNIX_ONLY
    return name not in WINDOWS_ONLY


def profile_dir(profile: str) -> str:
    """Cargo writes the `dev` profile into `target/<triple>/debug`."""
    return "debug" if profile == "dev" else profile


def target_dir(target: str, profile: str) -> Path:
    return ROOT / "target" / target / profile_dir(profile)


def _exe(name: str, target: str) -> str:
    return f"{name}.exe" if "windows" in target else name


def _copy(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dst)
    if os.name != "nt":
        dst.chmod(dst.stat().st_mode | 0o111)


def _strip_debug(staging: Path, target: str) -> None:
    """Drop DWARF from the staged Linux binaries (#2066).

    Even at `debug = "line-tables-only"` about two thirds of each ELF is
    `.debug_*`: stripping took the measured bundle from 273 MB to 125 MB, and
    the exec runner downloads it on the build -> unit critical path. Panic
    messages keep `file:line` (from `Location`, not DWARF); only symbolized
    backtrace lines are lost. Windows and macOS keep debug info in sidecars,
    so only Linux is stripped. Resolve a target-capable ELF editor, as wheel
    staging does: host GNU strip cannot read ARM binaries and leaves them
    above the installer's bounded executable size. Failed stripping is fatal.
    """
    if "linux" not in target:
        return
    from ci import process
    from ci.build_wheel import resolve_elf_objcopy

    objcopy = resolve_elf_objcopy(target)
    binaries = [
        *sorted((staging / "target").rglob("*")),
        *sorted((staging / "tests").rglob("*")),
    ]
    for path in binaries:
        if not path.is_file():
            continue
        result = process.run([objcopy, "--strip-debug", str(path)], capture_output=True, text=True)
        if result.returncode != 0:
            detail = (result.stderr or result.stdout).strip()
            raise RuntimeError(f"cannot strip {target} binary {path.name}: {detail}")


def pack(target: str, profile: str, dest: Path) -> int:  # noqa: C901
    built = target_dir(target, profile)
    if not built.is_dir():
        print(f"no build output at {built}", file=sys.stderr)
        return 1

    staging = CI_SCRATCH / f"bundle-{target}"
    if staging.exists():
        shutil.rmtree(staging)
    out_bin = staging / "target" / profile_dir(profile)
    out_bin.mkdir(parents=True)

    missing: list[str] = []
    for name in WORKSPACE_BINARIES:
        src = built / _exe(name, target)
        if src.is_file():
            _copy(src, out_bin / src.name)
        else:
            missing.append(name)
    if missing:
        # Hard failure, not a warning: a missing binary degrades into the exec
        # runner's poisoned-cargo error much later, in a job that is expensive
        # to reach and confusing to read.
        print(f"missing workspace binaries in {built}: {', '.join(missing)}", file=sys.stderr)
        return 1

    harness_list = built.parent / HARNESS_MANIFEST
    harnesses: list[str] = []
    skipped: list[str] = []
    if harness_list.is_file():
        for raw in json.loads(harness_list.read_text(encoding="utf-8")):
            src = Path(raw)
            if not src.is_file():
                continue
            if not _harness_runs_on(src.stem, target):
                skipped.append(src.name)
                continue
            _copy(src, staging / "tests" / src.name)
            harnesses.append(src.name)
    if skipped:
        # Never silent: a bundle that quietly omits a harness reads as "all
        # tests passed" when they were not shipped.
        print(f"skipped {len(skipped)} platform-excluded harnesses: {', '.join(sorted(skipped))}")

    wheels = sorted(DIST.glob("clud-*.whl"))
    for wheel in wheels:
        _copy(wheel, staging / "dist" / wheel.name)

    manifest = {
        "target": target,
        "profile": profile,
        "profile_dir": profile_dir(profile),
        "sha": os.environ.get("GITHUB_SHA", ""),
        "binaries": [_exe(name, target) for name in WORKSPACE_BINARIES],
        "harnesses": sorted(harnesses),
        "wheels": [wheel.name for wheel in wheels],
    }
    (staging / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    _strip_debug(staging, target)

    dest.mkdir(parents=True, exist_ok=True)
    archive = dest / ARCHIVE_NAME
    # compresslevel=1: these are debug binaries, so the archive is large and
    # highly compressible. Level 1 gets most of the ratio at a fraction of the
    # CPU, and this sits on the critical path between build and test.
    with tarfile.open(archive, "w:gz", compresslevel=1) as tar:
        tar.add(staging, arcname=".")

    size_mb = archive.stat().st_size / (1024 * 1024)
    print(f"packed {archive} ({size_mb:.1f} MiB, {len(harnesses)} harnesses)")
    return 0


def unpack(archive_dir: Path, dest: Path) -> int:
    archive = archive_dir / ARCHIVE_NAME
    if not archive.is_file():
        print(f"no bundle archive at {archive}", file=sys.stderr)
        return 1
    dest.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "r:gz") as tar:
        # `filter="data"` is the Python 3.12+ safe-extraction default; passing it
        # explicitly keeps 3.11 from emitting a DeprecationWarning and keeps the
        # behaviour identical across interpreter versions.
        tar.extractall(dest, filter="data")  # wheel-write-lint: allow (tar bundle, not a wheel)
    manifest = json.loads((dest / "manifest.json").read_text(encoding="utf-8"))
    print(f"unpacked bundle for {manifest['target']} ({len(manifest['harnesses'])} harnesses)")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Pack/unpack a CI test bundle")
    sub = parser.add_subparsers(dest="command", required=True)

    pack_parser = sub.add_parser("pack")
    pack_parser.add_argument("--target", required=True)
    pack_parser.add_argument("--profile", default="dev")
    pack_parser.add_argument("--dest", type=Path, required=True)

    unpack_parser = sub.add_parser("unpack")
    unpack_parser.add_argument("--archive-dir", type=Path, required=True)
    unpack_parser.add_argument("--dest", type=Path, required=True)

    args = parser.parse_args(argv)
    if args.command == "pack":
        return pack(args.target, args.profile, args.dest)
    return unpack(args.archive_dir, args.dest)


if __name__ == "__main__":
    sys.exit(main())
