"""Build clud Rust binary and package as a Python wheel via maturin."""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import platform
import shutil
import sys
import tempfile
import zipfile
from pathlib import Path
from typing import Literal

from ci import process
from ci.kitty_wheel import (
    KITTY_BUNDLE_FILES,
    KITTY_PASTE_HELPER,
    KITTY_TARGET,
    add_kitty_bundle,
    check_kitty_wheel,
    read_kitty_paste_helper,
    resolve_kitty_bundle,
)
from ci.webterm_wheel import add_companion, companion_name, desktop_target
from ci.wheel_repair import repair_windows_gnu_wheel
from ci.wheel_rewrite import rewrite_wheel, write_wheel

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"

BuildMode = Literal["dev", "release"]
# Every executable a shipped wheel carries: exactly one (#1551). `clud` is
# multicall: it installs itself at launch under every helper name
# (`clud-cmd-scan`, `clud-shim`, `python`, `rm`, `gh`, ...), so no installer
# or wheel lists, ships or keeps executable a second binary. The Windows
# wheel packer, the wheel verifier and the installed-scripts verifier all
# iterate this tuple. A new helper gets a dispatch name in `clud`
# (`crates/clud-bin/src/multicall.rs`), never a `[[bin]]` or an entry here;
# `test_wheel_ships_exactly_one_script` and
# `test_cargo_toml_has_no_second_shipped_bin` enforce that.
REQUIRED_SCRIPTS = ("clud",)


def prune_nonproduction_scripts(wheel: Path, *, compresslevel: int | None = None) -> bool:
    """Remove maturin's test-only executables from a binary wheel.

    Maturin's ``bindings = "bin"`` packages every enabled Cargo ``[[bin]]``.
    That is useful for the production helpers, but also used to publish
    ``clud-ctrlc-probe``, whose only callers are integration tests. Windows is
    hand-packed from ``REQUIRED_SCRIPTS`` already; this closes the equivalent
    native/maturin path without hiding the probe from test bundles.
    """
    with zipfile.ZipFile(wheel) as archive:
        members = archive.namelist()
    stale = {
        name
        for name in members
        if ".data/scripts/clud-" in name
        and Path(name).name.removesuffix(".exe") not in REQUIRED_SCRIPTS
    }
    record = next((name for name in members if name.endswith(".dist-info/RECORD")), None)
    if not stale or record is None:
        return False
    rewrite_wheel(
        wheel,
        lambda info, data: None if info.filename in stale else data,
        compresslevel=compresslevel,
    )
    return True


def elf_objcopy_candidates(target: str | None = None) -> list[str]:
    """Find an objcopy that can edit the configured Linux target's ELF files.

    The native developer environment normally exposes llvm-objcopy. Soldr's
    catalogue GNU cross toolchain instead exports a target-prefixed GNU binary
    next to its compiler (for example aarch64-conda-linux-gnu-objcopy).
    A target toolchain can expose either a target-prefixed binary or a generic
    ``objcopy`` beside its compiler. Keep every viable candidate: GNU objcopy
    only reveals some architecture mismatches when it opens the final ELF.
    """
    # `ci.xbuild` assembles a child environment for maturin. The parent
    # process that inspects the finished wheel does not inherit that mapping,
    # so callers that know the triple must pass it explicitly rather than
    # relying on CARGO_BUILD_TARGET being ambient.
    target = target or os.environ.get("CARGO_BUILD_TARGET", "")
    if target:
        from ci.env import host_target_triple

        is_cross_target = target != host_target_triple()
    else:
        is_cross_target = False
    target_key = target.upper().replace("-", "_")
    target_cc_key = target.lower().replace("-", "_")
    candidates = [
        os.environ.get("OBJCOPY"),
        os.environ.get(f"OBJCOPY_{target_key}") if target_key else None,
    ]

    target_compilers = (
        os.environ.get(f"CARGO_TARGET_{target_key}_LINKER") if target_key else None,
        # cc-rs and Soldr use the lowercase target spelling for CC_<target>.
        os.environ.get(f"CC_{target_cc_key}") if target_cc_key else None,
        os.environ.get(f"CC_{target_key}") if target_key else None,
    )
    compilers = (
        target_compilers
        if is_cross_target
        else (*target_compilers, os.environ.get("CC"))
    )
    for compiler in compilers:
        if not compiler:
            continue
        compiler_path = Path(compiler)
        for suffix in ("gcc", "clang", "cc"):
            if compiler_path.name.endswith(suffix):
                candidates.append(
                    str(
                        compiler_path.with_name(
                            f"{compiler_path.name.removesuffix(suffix)}objcopy"
                        )
                    )
                )
                candidates.append(str(compiler_path.with_name("objcopy")))
                break

    # LLVM is cross-capable; generic GNU objcopy is safe only for a native build.
    candidates.append("llvm-objcopy")
    if not is_cross_target:
        candidates.append("objcopy")
    resolved: list[str] = []
    for candidate in candidates:
        if candidate and shutil.which(candidate) and candidate not in resolved:
            resolved.append(candidate)
    return resolved


def resolve_elf_objcopy(target: str | None = None) -> str:
    """Return the first available ELF objcopy candidate for callers that need one."""
    candidates = elf_objcopy_candidates(target)
    if candidates:
        return candidates[0]
    raise RuntimeError(
        "no compatible ELF objcopy found; set OBJCOPY or expose the target toolchain"
    )


def strip_elf_debug(path: Path, target: str) -> None:
    """Strip a staged ELF with the same target-aware resolver used for wheels."""
    objcopy = resolve_elf_objcopy(target)
    result = process.run([objcopy, "--strip-debug", str(path)], capture_output=True, text=True)
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise RuntimeError(f"cannot strip {target} binary {path.name}: {detail}")


def remove_elf_debug_metadata(wheel: Path, *, target: str | None = None) -> bool:
    """Remove the residual ELF debug-GDB section from release wheel scripts.

    Cargo's ``strip = "debuginfo"`` removes DWARF but intentionally retains
    ``.debug_gdb_scripts``. It is not symbol data, yet its debug-prefixed name
    violates the artifact contract and confused the original audit. objcopy
    removes that one section without touching ``.symtab``.
    """
    with zipfile.ZipFile(wheel) as archive:
        members = archive.namelist()
        record = next((name for name in members if name.endswith(".dist-info/RECORD")), None)
        has_elf = any(
            ".data/scripts/" in info.filename and archive.read(info)[:4] == b"\x7fELF"
            for info in archive.infolist()
        )
    if record is None or not has_elf:
        return False
    candidates = elf_objcopy_candidates(target)

    def strip(info: zipfile.ZipInfo, data: bytes) -> bytes:
        if ".data/scripts/" not in info.filename or data[:4] != b"\x7fELF":
            return data
        # objcopy edits a file, so round-trip through a temp copy; the bytes
        # go back under the entry's original ZipInfo, keeping its mode.
        with tempfile.TemporaryDirectory(prefix="clud-wheel-strip-") as temp_dir:
            script = Path(temp_dir) / Path(info.filename).name
            failures: list[str] = []
            for objcopy in candidates:
                script.write_bytes(data)
                result = process.run(
                    [objcopy, "--remove-section=.debug_gdb_scripts", str(script)],
                    check=False,
                )
                if result.returncode == 0:
                    return script.read_bytes()
                failures.append(objcopy)
            tried = ", ".join(failures) or "no objcopy candidates"
            raise RuntimeError(
                f"failed to remove debug metadata from {info.filename}; tried {tried}"
            )

    rewrite_wheel(wheel, strip)
    return True


def verify_no_elf_debug_sections(wheel: Path) -> None:
    """Fail a release build if any shipped ELF script has `.debug_*` data."""
    with zipfile.ZipFile(wheel) as archive, tempfile.TemporaryDirectory(
        prefix="clud-wheel-verify-"
    ) as temp_dir:
        for info in archive.infolist():
            if ".data/scripts/" not in info.filename:
                continue
            data = archive.read(info)
            if data[:4] != b"\x7fELF":
                continue
            script = Path(temp_dir) / Path(info.filename).name
            script.write_bytes(data)
            name = info.filename
            result = process.run(
                ["readelf", "-SW", str(script)], capture_output=True, text=True, check=False
            )
            if result.returncode != 0:
                raise RuntimeError(f"readelf failed for shipped script {name}")
            if ".debug_" in result.stdout:
                raise RuntimeError(f"shipped script retains debug sections: {name}")


def local_webterm_target() -> str | None:
    from ci.env import host_target_triple

    target = host_target_triple()
    return target if desktop_target(target) else None


def build_local_webterm_companion(
    *, mode: BuildMode, target: str, env: dict[str, str]
) -> Path:
    """Build the native Tauri companion for a local desktop wheel."""
    command = [
        "soldr",
        "build",
        "--manifest-path",
        str(ROOT / "clud-webterm" / "Cargo.toml"),
    ]
    if mode == "release":
        command.append("--release")
    result = process.run(command, cwd=ROOT, check=False, env=env)
    if result.returncode != 0:
        raise RuntimeError("failed to build clud-webterm companion")
    profile = "release" if mode == "release" else "debug"
    # build_env sets CARGO_BUILD_TARGET on desktop hosts, so Cargo places the
    # artifact under the target triple even though this local invocation does
    # not repeat `--target` on its command line.
    companion = ROOT / "clud-webterm" / "target" / target / profile / companion_name(target)
    if not companion.is_file():
        raise RuntimeError(f"web terminal build produced no companion: {companion}")
    return companion


def build_local_kitty_paste_helper(*, mode: BuildMode, env: dict[str, str]) -> Path:
    """Build the extra Windows binary that maturin does not promise to emit."""
    command = [
        "soldr",
        "build",
        "--manifest-path",
        str(ROOT / "crates" / "clud-bin" / "Cargo.toml"),
        "--bin",
        "clud-kittyterm-paste",
        "--target",
        KITTY_TARGET,
    ]
    if mode == "release":
        command.append("--release")
    result = process.run(command, cwd=ROOT, check=False, env=env)
    if result.returncode != 0:
        raise RuntimeError("failed to build clud-kittyterm-paste")
    target_root = Path(env.get("CARGO_TARGET_DIR", ROOT / "target"))
    if not target_root.is_absolute():
        target_root = ROOT / target_root
    profile = "release" if mode == "release" else "debug"
    helper = target_root / KITTY_TARGET / profile / KITTY_PASTE_HELPER
    read_kitty_paste_helper(helper)
    return helper


def build_command(mode: BuildMode, env: dict[str, str] | None = None) -> list[str]:
    from ci.env import maturin_argv

    subcommand = [
        "build",
        "--interpreter",
        sys.executable,
        "--out",
        str(DIST),
    ]
    if mode == "dev":
        subcommand.extend(["--profile", "dev"])
    else:
        subcommand.append("--release")
        if platform.system() == "Linux":
            # Local, non-shipping wheel: no zig (banned everywhere, soldr#2299).
            # The manylinux_2_17 release wheel is produced by CI's blessed path
            # (ci/xbuild.py: soldr's catalogue toolchain + static libstdc++);
            # a bare local build cannot meet that floor, so tag it `linux` and
            # skip the audit rather than claim a floor it did not enforce.
            subcommand.extend(["--compatibility", "linux"])
        else:
            subcommand.extend(["--compatibility", "pypi"])
    # Use the dev-venv maturin via `python -m maturin`. setup-soldr shims keep
    # maturin-spawned cargo in the soldr/zccache path; routing maturin itself
    # through soldr fails on Linux because PyO3/maturin only publishes musl
    # Linux release assets.
    return maturin_argv(subcommand, env=env)


def build_windows_wheel_from_binaries(
    *,
    target: str,
    profile: str,
    target_dir: Path,
    dist_dir: Path,
    version: str,
) -> Path:
    """Package executables already built by soldr into a Windows wheel.

    Maturin's Linux-to-MSVC path brings its own xwin downloader, duplicating
    the SDK preparation that soldr already owns. The binaries in this wheel
    have therefore been built exclusively by the preceding soldr invocation.
    """
    # Resolve before writing a wheel, so an absent fork artifact cannot leave
    # behind a plausible but incomplete distributable in dist/.
    kitty_bundle = resolve_kitty_bundle() if target == KITTY_TARGET else None
    platform_tag = {"x86_64": "win_amd64", "aarch64": "win_arm64"}[target.split("-", 1)[0]]
    distribution = f"clud-{version}"
    wheel = dist_dir / f"{distribution}-py3-none-{platform_tag}.whl"
    binaries = target_dir / target / profile
    if kitty_bundle is not None:
        read_kitty_paste_helper(binaries / KITTY_PASTE_HELPER)
    scripts = []
    for name in REQUIRED_SCRIPTS:
        binary = binaries / f"{name}.exe"
        if not binary.is_file():
            raise RuntimeError(f"soldr-built Windows executable is missing: {binary}")
        scripts.append((f"{distribution}.data/scripts/{name}.exe", binary.read_bytes()))

    metadata = (
        "Metadata-Version: 2.1\n"
        "Name: clud\n"
        f"Version: {version}\n"
        "Summary: Fast Rust CLI for running Claude Code and Codex in YOLO mode\n"
    ).encode()
    wheel_metadata = (
        "Wheel-Version: 1.0\n"
        "Generator: clud ci.build_wheel\n"
        "Root-Is-Purelib: false\n"
        f"Tag: py3-none-{platform_tag}\n"
    ).encode()
    package_source = ROOT / "src" / "clud" / "__init__.py"
    members = [
        ("clud/__init__.py", package_source.read_bytes(), 0o644),
        *((name, data, None) for name, data in scripts),
        (f"{distribution}.dist-info/METADATA", metadata, 0o644),
        (f"{distribution}.dist-info/WHEEL", wheel_metadata, 0o644),
    ]
    dist_dir.mkdir(parents=True, exist_ok=True)
    write_wheel(wheel, members)
    if kitty_bundle is not None:
        add_kitty_bundle(
            wheel,
            kitty_bundle,
            ROOT / "crates" / "clud-bin" / "assets" / "kitty" / "clud-kittyterm.lua",
            target,
            binaries / KITTY_PASTE_HELPER,
        )
    return wheel


def built_wheels() -> list[Path]:
    return sorted(DIST.glob("clud-*.whl"), key=lambda path: path.stat().st_mtime)


def wheel_snapshot() -> dict[str, int]:
    return {path.name: path.stat().st_mtime_ns for path in built_wheels()}


def wheels_changed_since(snapshot: dict[str, int]) -> list[Path]:
    return [path for path in built_wheels() if snapshot.get(path.name) != path.stat().st_mtime_ns]


def latest_wheel() -> Path:
    wheels = built_wheels()
    if not wheels:
        raise RuntimeError(f"no built wheel found in {DIST}")
    return wheels[-1]


def install_wheel(wheel: Path, *, env: dict[str, str]) -> int:
    install = process.run(
        [
            "uv",
            "pip",
            "install",
            "--python",
            sys.executable,
            "--reinstall",
            "--no-deps",
            str(wheel),
        ],
        cwd=ROOT,
        check=False,
        env=env,
    )
    if install.returncode != 0:
        return install.returncode

    for pth in (ROOT / ".venv").glob("**/site-packages/clud.pth"):
        with contextlib.suppress(OSError):
            pth.unlink()
    return verify_installed_scripts(env=env)


def _script_name(name: str) -> str:
    return f"{name}.exe" if platform.system() == "Windows" else name


def _wheel_script_name(wheel: Path, name: str) -> str:
    """Return the script filename for the wheel target, not the build host."""
    platform_tag = wheel.stem.rsplit("-", 1)[-1].lower()
    is_windows = any(tag.startswith("win") for tag in platform_tag.split("."))
    return f"{name}.exe" if is_windows else name


#: Where `_installed_script` looks. Defaults to the running interpreter's
#: scripts dir; `verify_installed_scripts(scripts_dir=...)` points it at a
#: fresh venv for the release-wheel smoke (#1545).
_SCRIPTS_DIR: Path | None = None


def _installed_script(name: str) -> Path:
    return (_SCRIPTS_DIR or Path(sys.executable).parent) / _script_name(name)


def verify_installed_scripts(*, env: dict[str, str], scripts_dir: Path | None = None) -> int:
    global _SCRIPTS_DIR
    previous, _SCRIPTS_DIR = _SCRIPTS_DIR, scripts_dir
    try:
        return _verify_installed_scripts(env=env)
    finally:
        _SCRIPTS_DIR = previous


def _verify_installed_scripts(*, env: dict[str, str]) -> int:
    required = list(REQUIRED_SCRIPTS)
    target = local_webterm_target()
    if target is not None:
        required.append(companion_name(target).removesuffix(".exe"))
    missing = [name for name in required if not _installed_script(name).is_file()]
    if missing:
        print(
            "installed wheel is missing scripts: " + ", ".join(missing),
            file=sys.stderr,
            flush=True,
        )
        return 1
    # #1544/#1545: `.is_file()` passed a pip-installed 0644 `clud`. What the
    # user needs is a script the OS will execute.
    if os.name != "nt":
        not_executable = [
            name for name in required if not os.access(_installed_script(name), os.X_OK)
        ]
        if not_executable:
            print(
                "installed wheel scripts are not executable: " + ", ".join(not_executable),
                file=sys.stderr,
                flush=True,
            )
            return 1
    version = process.run(
        [str(_installed_script("clud")), "--version"],
        text=True,
        capture_output=True,
        check=False,
        timeout=30,
        env=env,
    )
    if version.returncode != 0:
        print(
            "installed clud --version smoke failed: "
            f"rc={version.returncode} stdout={version.stdout!r} stderr={version.stderr!r}",
            file=sys.stderr,
            flush=True,
        )
        return 1
    print(f"installed {version.stdout.strip()}", flush=True)
    if platform.system() == "Windows" and platform.machine().lower() in {"amd64", "x86_64"}:
        kitty_dir = _installed_script("clud").parent.parent / "clud-kittyterm"
        kitty_missing = [
            name for name in (*KITTY_BUNDLE_FILES, "clud-kittyterm.lua", KITTY_PASTE_HELPER)
            if not (kitty_dir / name).is_file()
        ]
        if kitty_missing:
            print(
                "installed wheel is missing Kitty GUI files: " + ", ".join(kitty_missing),
                file=sys.stderr,
                flush=True,
            )
            return 1

    # Hook smoke tests need the same trusted rm PATH contract as a session.
    # Never invoke this alias: the only payloads below are inert hook JSON.
    with tempfile.TemporaryDirectory(prefix="clud-wheel-rm-") as directory:
        alias = Path(directory) / _script_name("rm")
        shutil.copyfile(_installed_script("clud"), alias)
        alias.chmod(0o755)
        smoke_env = env | {"PATH": directory + os.pathsep + env.get("PATH", "")}
        return _verify_installed_smokes(env=smoke_env, target=target)


def _verify_installed_smokes(*, env: dict[str, str], target: str | None) -> int:
    # `clud __cmd-scan` is the argv[0]-independent spelling of `clud-cmd-scan`.
    guard = [str(_installed_script("clud")), "__cmd-scan"]
    deny_payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "bad" + " cmd"},
        }
    )
    deny = process.run(
        guard,
        input=deny_payload,
        text=True,
        capture_output=True,
        check=False,
        timeout=5,
        env=env,
    )
    if deny.returncode != 2 or "permissionDecision" not in deny.stdout or "deny" not in deny.stdout:
        print(
            "installed clud __cmd-scan deny smoke failed: "
            f"rc={deny.returncode} stdout={deny.stdout!r} stderr={deny.stderr!r}",
            file=sys.stderr,
            flush=True,
        )
        return 1

    allow_payload = json.dumps(
        {
            "tool_name": "Bash",
            "tool_input": {"command": "echo ok"},
        }
    )
    allow = process.run(
        guard,
        input=allow_payload,
        text=True,
        capture_output=True,
        check=False,
        timeout=5,
        env=env,
    )
    if allow.returncode != 0:
        print(
            "installed clud __cmd-scan allow smoke failed: "
            f"rc={allow.returncode} stdout={allow.stdout!r} stderr={allow.stderr!r}",
            file=sys.stderr,
            flush=True,
        )
        return 1

    if target is not None:
        webterm = _installed_script(companion_name(target).removesuffix(".exe"))
        startup = process.run(
            [str(webterm), "--startup-check"],
            text=True,
            capture_output=True,
            check=False,
            timeout=5,
            env=env,
        )
        if startup.returncode != 0:
            print(
                "installed clud-webterm startup smoke failed: "
                f"rc={startup.returncode} stdout={startup.stdout!r} stderr={startup.stderr!r}",
                file=sys.stderr,
                flush=True,
            )
            return 1

    if platform.system() == "Windows" and platform.machine().lower() in {"amd64", "x86_64"}:
        wezterm = _installed_script("clud").parent.parent / "clud-kittyterm" / "wezterm.exe"
        version = process.run(
            [str(wezterm), "--version"],
            text=True,
            capture_output=True,
            check=False,
            timeout=10,
            env=env,
        )
        if version.returncode != 0 or "wezterm" not in (
            version.stdout + version.stderr
        ).lower():
            print(
                "installed Kitty GUI headless smoke failed: "
                f"rc={version.returncode} stdout={version.stdout!r} stderr={version.stderr!r}",
                file=sys.stderr,
                flush=True,
            )
            return 1

        # wezterm-gui.exe is a Windows GUI-subsystem executable, so its help
        # text is not reliably attached to the CI runner's console.
        help_result = process.run(
            [str(wezterm), "start", "--help"],
            text=True,
            capture_output=True,
            check=False,
            timeout=10,
            env=env,
        )
        if help_result.returncode != 0 or "--wait-exit" not in (
            help_result.stdout + help_result.stderr
        ):
            print(
                "installed Kitty GUI start help smoke failed: "
                f"rc={help_result.returncode} stdout={help_result.stdout!r} "
                f"stderr={help_result.stderr!r}",
                file=sys.stderr,
                flush=True,
            )
            return 1

    return 0


def verify_wheel_scripts(wheel: Path) -> int:  # noqa: C901
    with zipfile.ZipFile(wheel) as archive:
        members = {name.replace("\\", "/") for name in archive.namelist()}
    required = list(REQUIRED_SCRIPTS)
    platform_tag = wheel.stem.rsplit("-", 1)[-1].lower()
    if any(tag.startswith(("win", "macosx")) for tag in platform_tag.split(".")):
        required.append("clud-webterm")
    missing = []
    for name in required:
        script = _wheel_script_name(wheel, name)
        if not any(member.endswith(f".data/scripts/{script}") for member in members):
            missing.append(script)
    if missing:
        print(
            f"built wheel {wheel.name} is missing scripts: " + ", ".join(missing),
            file=sys.stderr,
            flush=True,
        )
        return 1
    unexpected = [
        member
        for member in members
        if ".data/scripts/clud-" in member
        and Path(member).name.removesuffix(".exe") not in required
    ]
    if platform_tag == "win_amd64":
        prefix = next(
            (name.removesuffix(".dist-info/WHEEL") + ".data/data/clud-kittyterm/"
             for name in members if name.endswith(".dist-info/WHEEL")),
            None,
        )
        kitty_required = (*KITTY_BUNDLE_FILES, "clud-kittyterm.lua", KITTY_PASTE_HELPER)
        if prefix is None or any(prefix + name not in members for name in kitty_required):
            print(f"built wheel {wheel.name} is missing Kitty GUI bundle files", file=sys.stderr)
            return 1
        errors = check_kitty_wheel(wheel)
        if errors:
            print("\n".join(errors), file=sys.stderr)
            return 1
    elif platform_tag == "win_arm64":
        errors = check_kitty_wheel(wheel)
        if errors:
            print("\n".join(errors), file=sys.stderr)
            return 1
    if unexpected:
        print(
            f"built wheel {wheel.name} contains non-production scripts: " + ", ".join(unexpected),
            file=sys.stderr,
            flush=True,
        )
        return 1
    return 0


def run_build(mode: BuildMode) -> int:
    from ci.env import build_env

    env = build_env()
    target = local_webterm_target()
    # Fail before maturin creates a plausible but incomplete Windows wheel.
    kitty_bundle = resolve_kitty_bundle() if target == KITTY_TARGET else None
    paste_helper = (
        build_local_kitty_paste_helper(mode=mode, env=env) if kitty_bundle is not None else None
    )
    DIST.mkdir(parents=True, exist_ok=True)
    before = wheel_snapshot()
    cmd = build_command(mode, env=env)
    print(f"build mode: {mode}", file=sys.stderr, flush=True)
    result = process.run(cmd, cwd=ROOT, check=False, env=env)
    if result.returncode != 0:
        return result.returncode
    changed_wheels = wheels_changed_since(before)
    if not changed_wheels:
        print("build completed but produced no wheel", file=sys.stderr, flush=True)
        return 1
    companion = (
        build_local_webterm_companion(mode=mode, target=target, env=env)
        if target is not None
        else None
    )
    for wheel in changed_wheels:
        prune_nonproduction_scripts(wheel)
        if mode == "release":
            remove_elf_debug_metadata(wheel)
            verify_no_elf_debug_sections(wheel)
        repair_windows_gnu_wheel(wheel)
        if companion is not None and target is not None:
            add_companion(wheel, companion, target)
        if kitty_bundle is not None:
            add_kitty_bundle(
                wheel,
                kitty_bundle,
                ROOT / "crates" / "clud-bin" / "assets" / "kitty" / "clud-kittyterm.lua",
                target,
                paste_helper,
            )
        verify = verify_wheel_scripts(wheel)
        if verify != 0:
            return verify
    if mode != "dev":
        return 0

    wheel = latest_wheel()
    action = "reinstalling existing dev wheel" if wheel.name in before else "installing dev wheel"
    print(f"{action}: {wheel.name}", file=sys.stderr, flush=True)
    return install_wheel(wheel, env=env)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build clud")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--dev", action="store_true", help="build dev-profile wheel and reinstall")
    mode.add_argument("--release", action="store_true", help="build release wheel(s) into dist/")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None, *, default_mode: BuildMode = "release") -> int:
    args = parse_args(argv)
    mode: BuildMode = default_mode
    if args.dev:
        mode = "dev"
    if args.release:
        mode = "release"
    return run_build(mode)


if __name__ == "__main__":
    sys.exit(main())
