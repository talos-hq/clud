"""Cross-compile driver for CI builds.

Every cargo/maturin invocation in `.github/workflows/_build-target.yml` goes
through here, so the per-strategy environment lives in one testable place
instead of being smeared across YAML `if:` conditions.

One strategy now: `soldr`. Since soldr#2299 every target -- Apple, Windows-MSVC,
and Linux (via the catalogue GNU toolchain gcc-13.3.0-glibc-2.17-1, soldr#2238)
-- crosses through soldr's blessed surface. The `native`/`zigbuild` enum values
remain inert pending removal; zig is banned for every target by
`ci/banned_cross_tools.py`.

`soldr` is the blessed surface (soldr docs/CROSS_COMPILE.md): `soldr prepare`
provisions the sysroot in .github/actions/setup-build and exports the
target-scoped Cargo/cc-rs/linker env, then `soldr build --target <triple>`
links against it.

The link-producing `build` verb goes through `soldr build`; clippy, `cargo test
--no-run` and doc tests go through `soldr cargo <verb> --target <triple>`, the
target hooks setup-soldr advertises for every canonical triple (setup-soldr#443,
#648). Neither path reconstructs compiler, SDK or linker state: `soldr prepare`
exported it, and the only clud-specific addition is MSVC's `advapi32.lib` link
argument (see `target_env`). Wheels still run maturin from the dev venv
(`ci.env.maturin_argv`); moving them onto the PEP 517 hook is #2139.

Design: docs/architecture/ci.md
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shlex
import shutil
import sys
import tempfile
from collections.abc import MutableMapping
from pathlib import Path
from typing import TextIO


def _reject_by_path_invocation() -> None:
    r"""Refuse `python ci/xbuild.py ...` before its imports can mislead (#1017).

    Running this file *by path* puts `ci/` on `sys.path[0]` rather than the
    repo root, so `from ci import process` below resolves against whatever
    `ci` distribution happens to sit in site-packages. The traceback then
    points at an unrelated package:

        ImportError: cannot import name 'process' from 'ci'
            (C:\tools\python13\Lib\site-packages\ci\__init__.py)

    which sends a reader hunting through this repo for a missing symbol that
    is not missing. CI is unaffected because it uses the module form
    (`python -m ci.xbuild`), but the by-path form is the one a person types,
    and it is why a release wheel could not be reproduced locally.

    Rejecting rather than repairing `sys.path`: one canonical invocation is
    what keeps a local build and a CI build the same build, which is the whole
    point of being able to reproduce the wheel. A silently-repaired second
    entry point would drift.
    """
    if __package__:
        # Imported as `ci.xbuild`, or run with `-m`. Correct either way.
        return
    entry = Path(sys.path[0]).resolve() if sys.path and sys.path[0] else None
    if entry != Path(__file__).resolve().parent:
        return
    argv = " ".join(shlex.quote(arg) for arg in sys.argv[1:])
    print(
        "ci/xbuild.py cannot be run by path: that puts ci/ on sys.path and "
        "`from ci import process` resolves to an unrelated installed `ci` "
        "package instead of this repo's.\n"
        f"Run it as a module from the repo root instead:\n"
        f"    python -m ci.xbuild {argv}".rstrip(),
        file=sys.stderr,
    )
    raise SystemExit(2)


_reject_by_path_invocation()

from ci import cargo_messages, process  # noqa: E402 - must follow the guard above

ROOT = Path(__file__).resolve().parent.parent

PACKAGE_MANAGER = "cargo"


def _is_windows(target: str) -> bool:
    return "windows" in target


def _run_argv(argv: list[str]) -> list[str]:
    """Apply Linux's build memory limit without bypassing running-process.

    `running_process` intentionally rejects Python's `preexec_fn`. Use a small
    argv-only Bash wrapper on Linux instead: it applies the same 13 GB virtual
    memory cap the old pre-exec hook supplied, then replaces itself with cargo.
    """
    if platform.system() != "Linux":
        return argv
    return ["bash", "-c", 'ulimit -v 13000000; exec "$@"', "clud-xbuild", *argv]


def target_env(target: str, env: dict[str, str]) -> dict[str, str]:
    """The prepared env plus clud's one addition: MSVC links `advapi32.lib`.

    Setting generic RUSTFLAGS would override the target-scoped /LIBPATH
    arguments `soldr prepare` exports, leaving lld-link unable to find even
    kernel32.lib. Append to the existing target-specific value instead, and
    migrate any pre-existing generic flags: leaving RUSTFLAGS set would make
    Cargo ignore the target-specific value.
    """
    env = env.copy()
    if not _is_windows(target):
        return env
    rustflags_key = f"CARGO_TARGET_{target.upper().replace('-', '_')}_RUSTFLAGS"
    generic_rustflags = env.pop("RUSTFLAGS", "")
    target_rustflags = env.get(rustflags_key, "")
    if "CARGO_ENCODED_RUSTFLAGS" in env:
        # soldr >= 0.8.30 exports the prepared MSVC link configuration --
        # `-Clinker-flavor=lld-link` plus the SDK /LIBPATH arguments -- through
        # CARGO_ENCODED_RUSTFLAGS, which outranks both RUSTFLAGS and
        # target.<triple>.rustflags. Fold the target-scoped value in rather
        # than dropping it, so a toolchain that sets the encoded variable
        # without the SDK paths still keeps them.
        encoded = [part for part in env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f") if part]
        encoded += shlex.split(target_rustflags)
        encoded += shlex.split(generic_rustflags)
        encoded.append("-Clink-arg=advapi32.lib")
        env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(encoded)
        # Drop the now-shadowed variable so nothing reads a stale value.
        env.pop(rustflags_key, None)
    else:
        env[rustflags_key] = " ".join(
            filter(None, (target_rustflags, generic_rustflags, "-C link-arg=advapi32.lib"))
        )
    return env


def build_env(target: str, strategy: str) -> dict[str, str]:
    from ci.env import build_env as base_env

    env = target_env(target, base_env())
    env["CARGO_BUILD_TARGET"] = target
    # Fresh-checkout CI has no prior incremental state to reuse; incremental
    # only costs compile time and target/ size here.
    env.setdefault("CARGO_INCREMENTAL", "0")
    return env


def is_soldr_owned(target: str) -> bool:
    """Does soldr own this triple's cross toolchain end to end? (#637, soldr#2299)

    Apple, Windows-MSVC, and now GNU/Linux all do. soldr 0.8.39's catalogue GNU
    toolchain (gcc-13.3.0-glibc-2.17-1, soldr#2238) replaced zig for
    *-unknown-linux-gnu preparation and the manylinux wheel, so zig is no longer
    the supported Linux cross -- routing a linux-gnu build through zigbuild is
    now refused in cargo_argv, exactly as for Apple/MSVC.
    """
    return (
        target.endswith("-apple-darwin")
        or target.endswith("-pc-windows-msvc")
        or target.endswith("-unknown-linux-gnu")
        or target.endswith("-unknown-linux-musl")
    )


def cargo_argv(subcommand: list[str], target: str, strategy: str) -> list[str]:
    """Return the cargo argv for this strategy.

    Under `soldr`, `build` is `soldr build` and every other verb is setup-
    soldr's target hook `soldr cargo <verb> --target <triple>` (#648), so no
    cross operation falls through to a bare `cargo`. `native` (local
    development only) stays on plain cargo.
    """
    verb = subcommand[0]
    if strategy == "zigbuild" and is_soldr_owned(target):
        # Every clud triple is soldr-owned (Apple, MSVC, and Linux since
        # soldr#2299), so this covers every real target -- the strategy is not
        # what runs the compiler, this function is, so refuse structurally here
        # rather than relying on the matrix value alone. zig is not used for any
        # target; `ci/banned_cross_tools.py` bans it everywhere.
        raise ValueError(
            f"{target} must cross through soldr's blessed surface "
            "(`soldr prepare` / `soldr build`), not the zig cross wrapper, "
            "which is banned for every target. See docs/architecture/ci.md."
        )
    if strategy == "soldr" and verb == "build":
        # The blessed cross surface. Everything else under this strategy rides
        # on the env `soldr prepare` exported -- see the module docstring.
        return ["soldr", "build", *subcommand[1:], "--target", target]
    if strategy == "soldr":
        return ["soldr", "cargo", *subcommand, "--target", target]
    return ["cargo", *subcommand, "--target", target]


def run(argv: list[str], env: dict[str, str]) -> int:
    command = _run_argv(argv)
    print(f"+ {' '.join(command)}", flush=True)
    return process.run(command, cwd=ROOT, env=env).returncode


def cargo_target_dir(env: dict[str, str], default: Path) -> Path:
    """Resolve Cargo's output root against the build process working directory."""
    configured = env.get("CARGO_TARGET_DIR")
    if not configured:
        return default
    path = Path(configured)
    return path if path.is_absolute() else ROOT / path


def _write_terminated(stream: TextIO, text: str) -> None:
    """Write `text` ending in a newline, so the next line cannot swallow it.

    soldr's refusals end without one, and act dropped such a final line.
    """
    if text:
        stream.write(text if text.endswith("\n") else f"{text}\n")
        stream.flush()


def cmd_clippy(args: argparse.Namespace) -> int:
    env = build_env(args.target, args.strategy)
    base = cargo_argv(["clippy", "--workspace", "--all-targets"], args.target, args.strategy)
    return run([*base, "--", "-D", "warnings"], env)


def cmd_compile(args: argparse.Namespace) -> int:
    """Build the workspace binaries and, optionally, the test harnesses.

    Both happen in one job against one `target/` directory, so the dependency
    graph is compiled exactly once. The old layout spread these across three
    machines.
    """
    env = build_env(args.target, args.strategy)
    profile_args = ["--release"] if args.profile == "release" else []

    build = cargo_argv(
        ["build", "--workspace", "--bins", *profile_args], args.target, args.strategy
    )
    if run(build, env) != 0:
        return 1

    if not args.with_tests:
        return 0

    # `--message-format=json` is how we learn where cargo put each harness
    # binary; there is no stable path convention (they carry a hash suffix).
    harness = cargo_argv(
        ["test", "--workspace", "--no-run", "--message-format=json", *profile_args],
        args.target,
        args.strategy,
    )
    print(f"+ {' '.join(harness)}", flush=True)
    proc = process.run(harness, cwd=ROOT, env=env, capture_output=True, text=True)
    messages = cargo_messages.parse(proc.stdout or "")
    _write_terminated(sys.stderr, proc.stderr or "")
    if proc.returncode != 0:
        # #1726: with --message-format=json the compiler errors are records on
        # the captured stdout; without rendering them back out a failed build
        # said nothing at all.
        for rendered in messages.errors:
            _write_terminated(sys.stderr, rendered)
        print(f"cargo test --no-run failed (exit {proc.returncode})", file=sys.stderr, flush=True)
        return 1

    executables = messages.harnesses
    out = ROOT / "target" / args.target / "test-harnesses.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(sorted(executables), indent=2), encoding="utf-8")
    print(f"recorded {len(executables)} test harnesses -> {out}")
    return 0


def cmd_standalone(args: argparse.Namespace) -> int:
    """Build one static Linux clud binary without creating a Python wheel."""
    from installer.catalog import verify_static_musl_elf

    targets = {"x86_64-unknown-linux-musl": "x86_64", "aarch64-unknown-linux-musl": "aarch64"}
    if args.target not in targets or args.strategy != "soldr":
        raise ValueError("standalone build requires a soldr static-musl target")
    defer_native_check = getattr(args, "defer_native_version_check", False)
    if defer_native_check and args.target != "aarch64-unknown-linux-musl":
        raise ValueError("only the ARM64 cross-build can defer native execution")
    env = build_env(args.target, args.strategy)
    profile_args = ["--release"] if args.profile == "release" else []
    command = cargo_argv(
        ["build", "-p", "clud", "--bin", "clud", *profile_args], args.target, args.strategy
    )
    if run(command, env) != 0:
        return 1
    profile = "release" if args.profile == "release" else "debug"
    source = cargo_target_dir(env, ROOT / "target") / args.target / profile / "clud"
    payload = source.read_bytes()
    verify_static_musl_elf(payload, targets[args.target])
    version = _project_version()
    if not defer_native_check:
        result = process.run(
            [str(source), "--version"], cwd=ROOT, env=env, capture_output=True, text=True
        )
        if result.returncode != 0 or result.stdout.strip() != f"clud {version}":
            raise ValueError(
                f"standalone clud version check failed: {result.stdout!r} {result.stderr!r}"
            )
    destination = ROOT / "standalone" / f"clud-{version}-{args.target}"
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(payload)
    if hashlib.sha256(destination.read_bytes()).digest() != hashlib.sha256(payload).digest():
        raise ValueError("standalone clud changed while staging")
    destination.chmod(0o755)
    print(
        f"staged {destination.name}: {len(payload)} bytes "
        f"sha256={hashlib.sha256(payload).hexdigest()}"
    )
    return 0


def cmd_candidate(args: argparse.Namespace) -> int:
    """Build one dev-profile direct executable with exact-source provenance."""
    from ci.installer_candidate import TARGETS, candidate_filename
    from installer.verify_native_assets import verify_format

    if args.target not in TARGETS or args.strategy != "soldr" or args.profile != "dev":
        raise ValueError("candidate build requires a supported soldr dev target")
    env = build_env(args.target, args.strategy)
    command = cargo_argv(
        ["build", "-p", "clud", "--bin", "clud", "--features", "installer-ci-fixture"],
        args.target,
        args.strategy,
    )
    if run(command, env) != 0:
        return 1
    os_name, arch, kind, suffix = TARGETS[args.target]
    source = cargo_target_dir(env, ROOT / "target") / args.target / "debug" / f"clud{suffix}"
    payload = source.read_bytes()
    verify_format(payload, kind, arch)
    version = _project_version()
    filename = candidate_filename(version, args.target)
    destination_dir = ROOT / "candidate"
    destination_dir.mkdir(parents=True, exist_ok=True)
    destination = destination_dir / filename
    destination.write_bytes(payload)
    if kind == "elf":
        from ci.build_wheel import strip_elf_debug

        strip_elf_debug(destination, args.target)
        payload = destination.read_bytes()
        verify_format(payload, kind, arch)
    if os_name != "windows":
        destination.chmod(0o755)
    source_sha = process.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).strip()
    metadata = {
        "source_sha": source_sha,
        "target": args.target,
        "version": version,
        "profile": "dev",
        "filename": filename,
        "size_bytes": len(payload),
        "sha256": hashlib.sha256(payload).hexdigest(),
    }
    (destination_dir / "provenance.json").write_text(
        json.dumps(metadata, sort_keys=True, indent=2) + "\n", encoding="utf-8"
    )
    print(f"staged candidate {filename}: source={source_sha} sha256={metadata['sha256']}")
    return 0


def cmd_doctest(args: argparse.Namespace) -> int:
    """Run doc-tests.

    The old `cargo test --workspace` (ci/test.py:137) included these, but
    doc-tests produce no harness binary, so they cannot ride along in the
    bundle and be executed on an exec runner. They are architecture- and
    OS-independent, so running them once on the native host triple is full
    coverage rather than a reduction.
    """
    env = build_env(args.target, args.strategy)
    return run(cargo_argv(["test", "--workspace", "--doc"], args.target, args.strategy), env)


def _project_version() -> str:
    import tomllib

    with (ROOT / "pyproject.toml").open("rb") as project:
        return tomllib.load(project)["project"]["version"]


#: Where `cmd_wheel` stages sidecar debug info. Deliberately NOT `dist/`:
#: `_build-target.yml` uploads `dist/*` as the `wheels-*` artifact, which
#: `publish-pypi` feeds straight to twine as `packages-dir`. A non-package file
#: there fails the upload, which is the exact blocker split-debuginfo fixes.
DEBUGINFO_DIR = ROOT / "dist-debuginfo"

#: Deflate level for dev (CI-only) wheels: speed over size. Release wheels
#: keep maturin's and zipfile's defaults.
DEV_WHEEL_COMPRESSLEVEL = 1


def collect_debuginfo(target: str, profile: str) -> list[Path]:
    """Stage this triple's sidecar debug info for the release job.

    `[profile.release] split-debuginfo = "packed"` (Cargo.toml) writes the bulk
    of the DWARF beside each binary instead of inside it. On ELF that is a
    `.dwp`; on MSVC/Apple `packed` is already the default and the sidecar is a
    `.pdb` / `.dSYM`, neither of which this collects -- only the `.dwp` is a
    single file that is not already produced today, and only ELF embedded its
    debug info in the shipped wheel.

    Only `clud` itself, not the shipped shims. `crates/clud-bin/src/main.rs` is
    the sole binary that installs the crash reporter, and each shim currently
    shares clud-bin's whole dep tree, so their `.dwp`s are ~91 MB of duplicate
    DWARF apiece -- five per Linux triple would put ~900 MB on every release
    page to symbolicate crashes nothing reports. Their line tables stay
    embedded regardless, so a shim panic still resolves file:line unaided.

    Best-effort by contract: a target that produces no `.dwp` returns an empty
    list and logs it. Missing symbols must never block a release -- the wheel
    shrinking is the hard requirement, attaching symbols the secondary one.
    """
    source = ROOT / "target" / target / profile
    # Cargo uplifts the *binary* from `deps/clud-<hash>` to `release/clud`, but
    # 2.7.5 shipped with no sidecar because it does not reliably uplift the
    # `.dwp` alongside it under maturin's `cargo rustc --bin` invocation: the
    # wheel shrank 101 MB -> 47 MB, so the DWARF did leave the ELF, yet
    # `release/clud.dwp` did not exist. Look in `deps/` too, and match the
    # hash suffix exactly so a differently-named bin cannot be mistaken for
    # clud's own.
    deps = sorted(
        path
        for path in source.glob("deps/clud-*.dwp")
        if re.fullmatch(r"clud-[0-9a-f]{8,}", path.stem)
    )
    found = [path for path in (source / "clud.dwp", *deps) if path.is_file()][:1]
    if not found:
        # Self-diagnosing: name whatever split DWARF does exist so the next
        # release says where it went instead of repeating this silence.
        strays = [str(path.relative_to(source)) for path in sorted(source.rglob("*.dw[po]"))[:10]]
        print(
            f"no clud .dwp under {source}; split DWARF present: {strays or 'none'}",
            flush=True,
        )
        return []
    DEBUGINFO_DIR.mkdir(parents=True, exist_ok=True)
    staged: list[Path] = []
    for dwp in found:
        # Suffix the triple so `merge-multiple: true` cannot collide the
        # per-target artifacts on top of each other in the release job.
        # Fixed stem, not `dwp.stem`: the deps/ copy is named `clud-<hash>`,
        # and a release asset whose name carries a build hash is one #1016's
        # fetcher could not predict. The triple suffix keeps `merge-multiple`
        # from colliding the per-target artifacts.
        dest = DEBUGINFO_DIR / f"clud-{target}.dwp"
        dest.write_bytes(dwp.read_bytes())
        print(f"debug info: {dest} ({dest.stat().st_size:,} bytes)", flush=True)
        staged.append(dest)
    return staged


def verify_wheel_modes(wheel: Path) -> int:
    """#1545: fail a local or CI wheel build whose scripts lost their exec bits."""
    from ci.check_wheel_modes import check_wheel

    errors = check_wheel(wheel)
    for error in errors:
        print(f"error: {error}", file=sys.stderr, flush=True)
    if not errors:
        print(f"script modes ok: {wheel.name}", flush=True)
    return 1 if errors else 0


def cmd_wheel(args: argparse.Namespace) -> int:  # noqa: C901
    """Build the wheel into dist/ for this triple.

    Maturin normally reuses the same `target/<triple>/` directory the compile
    step populated. Release GNU Linux wheels are the exception: their
    manylinux link must not reuse a host-default glibc artifact.
    """
    from ci.env import maturin_argv
    from ci.webterm_wheel import add_companion, companion_name, desktop_target

    env = build_env(args.target, args.strategy)
    profile = "release" if args.profile == "release" else "debug"
    companion = (
        cargo_target_dir(env, ROOT / "clud-webterm" / "target")
        / args.target
        / profile
        / companion_name(args.target)
    )
    if desktop_target(args.target):
        command = [
            "soldr",
            "build",
            "--manifest-path",
            str(ROOT / "clud-webterm" / "Cargo.toml"),
            "--target",
            args.target,
        ]
        if args.profile == "release":
            command.append("--release")
        if run(command, env) != 0:
            return 1
    if _is_windows(args.target) and args.strategy == "soldr":
        target_dir = cargo_target_dir(env, ROOT / "target")
        from ci.build_wheel import build_windows_wheel_from_binaries, verify_wheel_scripts

        try:
            wheel = build_windows_wheel_from_binaries(
                target=args.target,
                profile=profile,
                target_dir=target_dir,
                dist_dir=ROOT / "dist",
                version=_project_version(),
            )
        except RuntimeError as error:
            print(error, file=sys.stderr)
            return 1
        add_companion(wheel, companion, args.target)
        if verify_wheel_scripts(wheel) != 0 or verify_wheel_modes(wheel) != 0:
            return 1
        print(f"packaged soldr-built Windows wheel: {wheel}")
        collect_debuginfo(args.target, profile)
        return 0
    subcommand = [
        "build",
        "--target",
        args.target,
        "--interpreter",
        sys.executable,
        "--out",
        str(ROOT / "dist"),
    ]
    if args.profile == "dev":
        # Dev wheels are CI artifacts that ci.bundle re-compresses anyway;
        # deflate level 1 packs the ~hundreds-of-MB debug binaries far faster.
        # Same entries and modes, only the zip compression level differs.
        subcommand += ["--profile", "dev", "--compression-level", str(DEV_WHEEL_COMPRESSLEVEL)]
        if args.target.endswith("-unknown-linux-gnu"):
            # Dev wheels are CI artifacts, not distributables, and must not be
            # audited for manylinux compliance. maturin audits by default on
            # Linux even with no --compatibility flag; --compatibility linux
            # opts out. The release path below asserts the floor properly.
            subcommand += ["--compatibility", "linux"]
    else:
        subcommand.append("--release")
        if args.target.endswith("-unknown-linux-gnu"):
            # No --zig. soldr's blessed catalogue GNU toolchain
            # (gcc-13.3.0-glibc-2.17-1, soldr#2238) is prepared by `soldr
            # prepare --target <triple>` in setup-build and supplies the pinned
            # glibc-2.17 floor for every object, so maturin's manylinux2014
            # audit passes without zig -- and, since #1207, without the static
            # C++/unwind link flags whisper-rs-sys needed: no C++ `-sys` crate
            # is left in the graph (`ring` and `blake3` are C). This replaces
            # the `maturin --zig` path and the env-scrub denylist it required.
            # See docs/architecture/ci.md and soldr#2299.
            subcommand += ["--compatibility", "manylinux2014"]
        else:
            subcommand += ["--compatibility", "pypi"]

    if run(maturin_argv(subcommand, env=env), env) != 0:
        return 1

    from ci.build_wheel import (
        built_wheels,
        prune_nonproduction_scripts,
        remove_elf_debug_metadata,
        verify_no_elf_debug_sections,
        verify_wheel_scripts,
    )
    from ci.wheel_repair import repair_windows_gnu_wheel

    wheels = built_wheels()
    if not wheels:
        print("build completed but produced no wheel", file=sys.stderr)
        return 1
    for wheel in wheels:
        prune_nonproduction_scripts(
            wheel, compresslevel=DEV_WHEEL_COMPRESSLEVEL if args.profile == "dev" else None
        )
        if args.profile == "release":
            remove_elf_debug_metadata(wheel, target=args.target)
            verify_no_elf_debug_sections(wheel)
        if desktop_target(args.target):
            add_companion(wheel, companion, args.target)
        repair_windows_gnu_wheel(wheel)
        if verify_wheel_scripts(wheel) != 0 or verify_wheel_modes(wheel) != 0:
            return 1
    if collect_debuginfo(args.target, profile):
        # #1016: publish which binary this `.dwp` belongs to. Read from the
        # final wheel -- the shipped bytes -- in the same job that staged the
        # sidecar; the release job folds the fragments into BUILD-IDS.txt.
        from ci.build_ids import record

        record(args.target, wheels, DEBUGINFO_DIR)
    return 0


def _host_triple() -> str | None:
    """This machine's Rust target triple, via rustc. `None` if unavailable."""
    try:
        from ci.env import soldr_path

        soldr = soldr_path()
        argv = [soldr, "rustc", "-vV"] if soldr else ["rustc", "-vV"]
        result = process.run(argv, capture_output=True, text=True, check=False)
    except OSError:
        return None
    for line in (result.stdout or "").splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    return None


def parse_github_env(text: str) -> dict[str, str]:
    """Parse a `$GITHUB_ENV`-format file into a mapping.

    GitHub's format is `KEY=VALUE` per line, plus the heredoc shape
    `KEY<<DELIM` ... `DELIM` for multi-line values. `soldr prepare
    --github-env` writes the former today; the latter is accepted so a future
    multi-line export does not silently corrupt the environment.
    """
    env: dict[str, str] = {}
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i]
        i += 1
        if not line.strip():
            continue
        if "<<" in line and "=" not in line.split("<<", 1)[0]:
            key, delim = line.split("<<", 1)
            if not delim:
                raise ValueError(f"heredoc with an empty delimiter: {line!r}")
            body: list[str] = []
            while i < len(lines) and lines[i] != delim:
                body.append(lines[i])
                i += 1
            if i >= len(lines):
                raise ValueError(f"unterminated heredoc for {key.strip()!r}")
            i += 1  # skip the closing delimiter
            env[key.strip()] = "\n".join(body)
            continue
        key, sep, value = line.partition("=")
        if not sep:
            raise ValueError(f"unparseable $GITHUB_ENV line: {line!r}")
        env[key.strip()] = value
    return env


def prepare_toolchain_locally(
    target: str, env: MutableMapping[str, str] | None = None
) -> list[str] | None:
    """Run `soldr prepare` and apply its exported env in-process (#1017).

    CI's `.github/actions/setup-build` runs `soldr prepare --target <t>
    --github-env "$GITHUB_ENV"`, which persists the target toolchain env for
    every later step. That mechanism has no local counterpart, which is why a
    release wheel could not be reproduced on a developer machine: the build
    proceeded as if the variables were present and died inside `ring`.

    This is the local counterpart. Outside GitHub Actions it runs the same
    `prepare` into a temporary file and applies every `KEY=VALUE` it wrote to
    this process's environment, which every child (cargo, soldr build,
    maturin) inherits -- the same env, by the same command, as CI.

    Returns the list of keys applied, or `None` when nothing ran: in CI (the
    env is already there), when `CLUD_XBUILD_SKIP_PREPARE` is set, or when no
    soldr is on PATH or in the repo `.venv` (the preflight then explains
    what is missing).
    """
    if env is None:
        env = os.environ
    if env.get("GITHUB_ACTIONS") == "true" or env.get("CLUD_XBUILD_SKIP_PREPARE"):
        return None
    from ci.env import soldr_path

    soldr = env.get("SOLDR_BINARY") or soldr_path(dict(env))
    if soldr is None:
        return None

    with tempfile.TemporaryDirectory(prefix="clud-xbuild-prepare-") as tmp:
        env_file = Path(tmp) / "github.env"
        command = [soldr, "prepare", "--target", target, "--github-env", str(env_file)]
        print(f"+ {' '.join(command)}", flush=True)
        result = process.run(command, cwd=ROOT, env=dict(env), check=False)
        if result.returncode != 0:
            raise SystemExit(
                f"error: `soldr prepare --target {target}` exited {result.returncode}; "
                "the target toolchain is not prepared, so the build would fail "
                "later inside a dependency. Fix the prepare failure above first."
            )
        exported = parse_github_env(env_file.read_text(encoding="utf-8"))

    for key, value in exported.items():
        env[key] = value
    print(
        f"soldr prepare: applied {len(exported)} exported variable(s) for {target}",
        flush=True,
    )
    return list(exported)


def cross_toolchain_preflight(target: str) -> str | None:
    """Explain a missing cross C toolchain before cargo fails obscurely.

    #1017 blocker 2: a local `wheel` for a non-host GNU triple dies minutes in
    with

        error: failed to run custom build command for `ring v0.17.14`
          error occurred in cc-rs: failed to find tool
          "x86_64-linux-gnu-gcc": program not found

    The issue asks whether that is a soldr bug or clud failing to export
    `CC_<target>` / `TARGET_CC`. It is neither. CI's
    `.github/actions/setup-build` runs `soldr prepare --target <t>
    --github-env "$GITHUB_ENV"`, which persists the toolchain env for every
    later step. Nothing does that locally: `ci/xbuild.py` never invokes
    `prepare`, and only documents that those variables "are exported by"
    it -- so the build proceeds as if they were.

    `prepare_toolchain_locally` closes that gap when soldr is installed; this
    preflight is what remains for a machine without it, making the gap legible
    at the point of entry rather than as a cc-rs error inside a dependency,
    which is what sent the issue looking in two wrong places.

    Returns an explanation, or `None` when there is nothing to warn about.
    """
    # CI prepares the toolchain in a prior step, so its env is already right.
    if os.environ.get("GITHUB_ACTIONS") == "true":
        return None
    # Only GNU/Linux cross builds hit this. Apple and MSVC fail earlier and
    # more clearly on their own SDK checks.
    if not target.endswith("-unknown-linux-gnu"):
        return None
    host = _host_triple()
    if host is None or host == target:
        return None

    arch = target.split("-", 1)[0]
    gcc = f"{arch}-linux-gnu-gcc"
    env_key = "CC_" + target.replace("-", "_")
    if os.environ.get(env_key) or os.environ.get("TARGET_CC"):
        return None
    if shutil.which(gcc):
        return None

    return (
        f"error: no C cross toolchain for {target}.\n"
        f"  `{gcc}` is not on PATH and neither {env_key} nor TARGET_CC is set,\n"
        f"  so cc-rs will fail inside a dependency (ring) rather than here.\n"
        f"\n"
        f'  CI gets this from `soldr prepare --target {target} --github-env "$GITHUB_ENV"`\n'
        f"  in .github/actions/setup-build. This driver runs the same `soldr prepare`\n"
        f"  locally when `soldr` is on PATH (#1017 blocker 2), and it did not run\n"
        f"  or did not export a compiler for this target.\n"
        f"\n"
        f"  Either install soldr (`./install --global`) and re-run, install {gcc},\n"
        f"  or export {env_key} to a working cross compiler."
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Cross-compile driver for clud CI")
    sub = parser.add_subparsers(dest="command", required=True)

    def add_common(p: argparse.ArgumentParser) -> None:
        p.add_argument("--target", required=True)
        p.add_argument("--strategy", default="native", choices=("native", "zigbuild", "soldr"))
        p.add_argument("--profile", default="dev", choices=("dev", "release"))

    clippy = sub.add_parser("clippy")
    add_common(clippy)
    clippy.set_defaults(func=cmd_clippy)

    compile_parser = sub.add_parser("compile")
    add_common(compile_parser)
    compile_parser.add_argument("--with-tests", action="store_true")
    compile_parser.set_defaults(func=cmd_compile)

    doctest = sub.add_parser("doctest")
    add_common(doctest)
    doctest.set_defaults(func=cmd_doctest)

    wheel = sub.add_parser("wheel")
    add_common(wheel)
    wheel.set_defaults(func=cmd_wheel)

    standalone = sub.add_parser("standalone")
    add_common(standalone)
    standalone.add_argument("--defer-native-version-check", action="store_true")
    standalone.set_defaults(func=cmd_standalone)

    candidate = sub.add_parser("candidate")
    add_common(candidate)
    candidate.set_defaults(func=cmd_candidate)

    args = parser.parse_args(argv)

    if args.strategy == "soldr":
        prepare_toolchain_locally(args.target)
    problem = cross_toolchain_preflight(args.target)
    if problem is not None:
        print(problem, file=sys.stderr)
        return 1

    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
