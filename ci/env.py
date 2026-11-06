"""Build environment setup for clud CI."""

from __future__ import annotations

import os
import platform
import shutil
import sys
from pathlib import Path

import tomllib

from ci import process


def cargo_home() -> Path:
    if os.environ.get("CARGO_HOME"):
        return Path(os.environ["CARGO_HOME"]).expanduser()
    return Path.home() / ".cargo"


def cargo_bin() -> Path:
    return cargo_home() / "bin"


def rustup_home() -> Path:
    if os.environ.get("RUSTUP_HOME"):
        return Path(os.environ["RUSTUP_HOME"]).expanduser()
    return Path.home() / ".rustup"


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def toolchain_file() -> Path:
    return repo_root() / "rust-toolchain.toml"


def load_toolchain_channel() -> str:
    with toolchain_file().open("rb") as handle:
        data = tomllib.load(handle)
    toolchain = data.get("toolchain")
    if not isinstance(toolchain, dict):
        raise RuntimeError(f"missing [toolchain] in {toolchain_file()}")
    channel = toolchain.get("channel")
    if not isinstance(channel, str) or not channel:
        raise RuntimeError(f"missing toolchain.channel in {toolchain_file()}")
    return channel


def host_target_triple() -> str:
    system = platform.system()
    machine = platform.machine().lower()
    arch = {
        "amd64": "x86_64",
        "x86_64": "x86_64",
        "arm64": "aarch64",
        "aarch64": "aarch64",
    }.get(machine)
    if arch is None:
        raise RuntimeError(f"unsupported architecture: {machine}")
    if system == "Windows":
        # This repo builds Windows artifacts with the rust-toolchain override,
        # which points at MSVC. `cargo -Vv` can still report the ambient GNU
        # rustup proxy host, and using that value here causes wheel builds to
        # pick the wrong target triple.
        return f"{arch}-pc-windows-msvc"
    detected = _cargo_host_triple()
    if detected:
        return detected
    if system == "Linux":
        return f"{arch}-unknown-linux-gnu"
    if system == "Darwin":
        return f"{arch}-apple-darwin"
    raise RuntimeError(f"unsupported platform: {system}")


def _cargo_host_triple() -> str | None:
    soldr = soldr_path()
    cargo = shutil.which("cargo") if not soldr else None
    if not soldr and not cargo:
        return None
    argv = [soldr, "cargo", "-Vv"] if soldr else [cargo, "-Vv"]
    result = process.run(
        argv,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip() or None
    return None


def toolchain_name() -> str:
    return f"{load_toolchain_channel()}-{host_target_triple()}"


def toolchain_bin() -> Path:
    return rustup_home() / "toolchains" / toolchain_name() / "bin"


def _env_flag_enabled(value: str | None) -> bool:
    return value is not None and value.strip().lower() in {"1", "true", "yes", "on"}


def _path_is_within(path: str | Path, directory: str | Path) -> bool:
    try:
        path_norm = os.path.normcase(os.path.abspath(str(path)))
        directory_norm = os.path.normcase(os.path.abspath(str(directory)))
        return os.path.commonpath([path_norm, directory_norm]) == directory_norm
    except ValueError:
        return False


def _soldr_shims_requested(env: dict[str, str] | None = None) -> bool:
    source = os.environ if env is None else env
    if _env_flag_enabled(source.get("CLUD_USE_SOLDR_SHIMS")):
        return True

    path = source.get("PATH")
    if path is None:
        return False
    cargo = shutil.which("cargo", path=path)
    shims_dir = source.get("SOLDR_SHIMS_DIR")
    if cargo and shims_dir and _path_is_within(cargo, shims_dir):
        return True

    soldr = shutil.which("soldr", path=path)
    return bool(cargo and soldr and Path(cargo).parent.name.lower() == "shims")


def _find_vswhere() -> Path | None:
    candidates = [
        Path(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe"),
        Path(r"C:\Program Files\Microsoft Visual Studio\Installer\vswhere.exe"),
    ]
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return None


def _find_vsdevcmd() -> Path | None:
    vswhere = _find_vswhere()
    if vswhere is None:
        return None
    result = process.run(
        [
            str(vswhere),
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    installation_path = result.stdout.strip()
    if not installation_path:
        return None
    candidate = Path(installation_path) / "Common7" / "Tools" / "VsDevCmd.bat"
    if candidate.is_file():
        return candidate
    return None


def _windows_build_env() -> dict[str, str]:
    env = os.environ.copy()
    toolchain_bin_dir = toolchain_bin()
    use_soldr_shims = _soldr_shims_requested(env)
    if toolchain_bin_dir.is_dir():
        if not use_soldr_shims:
            env["PATH"] = str(toolchain_bin_dir) + os.pathsep + env.get("PATH", "")
            cargo_exe = toolchain_bin_dir / "cargo.exe"
            rustc_exe = toolchain_bin_dir / "rustc.exe"
            if cargo_exe.is_file():
                env["CARGO"] = str(cargo_exe)
            if rustc_exe.is_file():
                env["RUSTC"] = str(rustc_exe)
        env["RUSTUP_TOOLCHAIN"] = toolchain_name()
        env["CARGO_BUILD_TARGET"] = host_target_triple()

    gnu_runtime = _find_windows_gnu_runtime_bin()
    if gnu_runtime is not None and env.get("CARGO_BUILD_TARGET", "").endswith("-gnu"):
        env["PATH"] = str(gnu_runtime) + os.pathsep + env.get("PATH", "")

    vsdevcmd = _find_vsdevcmd()
    if vsdevcmd is None:
        return env

    command = f'"{vsdevcmd}" -arch=x64 -host_arch=x64 >nul && set'
    result = process.run(
        ["cmd", "/d", "/s", "/c", command],
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )
    if result.returncode != 0:
        return env
    for line in result.stdout.splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        env[key] = value
    return env


def _find_windows_gnu_runtime_bin() -> Path | None:
    candidates = [
        Path(r"C:\msys64\ucrt64\bin"),
        Path(r"C:\msys64\mingw64\bin"),
        Path(r"C:\Qt\Tools\mingw1120_64\bin"),
        Path(r"C:\MinGW\bin"),
    ]
    for candidate in candidates:
        if (candidate / "libstdc++-6.dll").is_file():
            return candidate
    return None


def activate() -> None:
    bin_dir = cargo_bin()
    if not bin_dir.is_dir():
        return

    current_path = os.environ.get("PATH", "")
    path_parts = current_path.split(os.pathsep) if current_path else []
    normalized_cargo_bin = os.path.normcase(os.path.normpath(str(bin_dir)))
    normalized_parts = {os.path.normcase(os.path.normpath(part)) for part in path_parts if part}
    if _soldr_shims_requested(os.environ):
        filtered_parts = [
            part
            for part in path_parts
            if os.path.normcase(os.path.normpath(part)) != normalized_cargo_bin
        ]
        filtered_parts.append(str(bin_dir))
        os.environ["PATH"] = os.pathsep.join(filtered_parts)
        return
    if normalized_cargo_bin in normalized_parts:
        return
    os.environ["PATH"] = (
        str(bin_dir) if not current_path else str(bin_dir) + os.pathsep + current_path
    )


# `CLUD_*` variables the test harness itself owns. Every other `CLUD_*`
# variable is clud *session* state (e.g. `CLUD_SKIP_RM_IDENTITY`, which clud
# auto-enables in its own repo, or `CLUD_ROUTE_CONTEXT`) that leaks into
# `bash test` when it runs from inside a clud session and flips hook
# behavior under test (#1423).
_TEST_HARNESS_CLUD_PREFIXES = (
    "CLUD_TEST_",
    "CLUD_HARNESS_",
    "CLUD_REAL_CLAUDE",
    "CLUD_CANDIDATE_",
    "CLUD_INSTALLER_CI_",
)
_TEST_HARNESS_CLUD_NAMES = frozenset(
    {
        "CLUD_INTEGRATION_TESTS",
        "CLUD_NO_UNLOCK",
        "CLUD_USE_RUNTIME_CACHE",
        "CLUD_USE_SOLDR_SHIMS",
        "CLUD_XBUILD_SKIP_PREPARE",
        "CLUD_REQUIRE_PTY",
        "CLUD_PTY_PUMP_TRACE",
        "CLUD_PUBLIC_MODE",
        "CLUD_PUBLIC_RELEASE_TAG",
        "CLUD_PUBLIC_EVIDENCE",
        # Plumbing, not behavior: a clud session puts its `python` shim on
        # PATH (#1423). Without its target and ABI stamp the shim now passes
        # through to the next real python (#1546), so this is belt and braces.
        "CLUD_PYTHON_SHIM_TARGET",
    }
)


def is_clud_session_var(name: str) -> bool:
    """True for a `CLUD_*` variable that is session state, not test config."""
    upper = name.upper()
    if not upper.startswith("CLUD_"):
        return False
    if upper in _TEST_HARNESS_CLUD_NAMES:
        return False
    return not upper.startswith(_TEST_HARNESS_CLUD_PREFIXES)


def scrub_clud_session_env(env: dict[str, str] | os._Environ[str]) -> list[str]:
    """Remove inherited clud session variables in place; return what was removed."""
    removed = [name for name in list(env) if is_clud_session_var(name)]
    for name in removed:
        del env[name]
    return removed


# The directories a clud session prepends to PATH: its `rm` shim, which routes
# deletions to the trash, and its `python`/tool shims (#1817).
_CLUD_SESSION_PATH_DIRS = ((".clud", "state", "rm-shim"), (".clud", "state", "shims"))


def is_clud_session_path_dir(entry: str) -> bool:
    """True for a PATH entry that is one of a clud session's shim directories."""
    parts = tuple(part.lower() for part in Path(entry).parts)
    if any(parts[-len(tail) :] == tail for tail in _CLUD_SESSION_PATH_DIRS):
        return True
    # A build's own directories, `.clud/state/versions/<build>/<leaf>` (DD-203).
    return parts[-5:-2] == (".clud", "state", "versions") and parts[-1] in {"rm-shim", "shims"}


def scrub_clud_session_path(env: dict[str, str] | os._Environ[str]) -> list[str]:
    """Drop a clud session's shim directories from PATH in place (#1817).

    Inherited by the suite, the session's `rm` shim answers the installers and
    shims under test instead of the real `rm`. Returns the removed entries.
    """
    path = env.get("PATH")
    if not path:
        return []
    entries = path.split(os.pathsep)
    removed = [entry for entry in entries if entry and is_clud_session_path_dir(entry)]
    if removed:
        env["PATH"] = os.pathsep.join(e for e in entries if e not in removed)
    return removed


def suite_env() -> dict[str, str]:
    """`clean_env()` minus inherited clud session variables (#1423) and shims (#1817)."""
    env = clean_env()
    scrub_clud_session_env(env)
    scrub_clud_session_path(env)
    return env


def clean_env() -> dict[str, str]:
    activate()
    env = os.environ.copy()
    env.pop("VIRTUAL_ENV", None)
    env.setdefault("PYTHONUTF8", "1")
    if platform.system() == "Windows":
        env = env | _windows_build_env()
        env.pop("VIRTUAL_ENV", None)
        env.setdefault("PYTHONUTF8", "1")
    env.setdefault("RUSTUP_TOOLCHAIN", toolchain_name())
    if _soldr_shims_requested(env):
        env.pop("CARGO", None)
        env.pop("RUSTC", None)
        env.pop("RUSTC_WRAPPER", None)
    return env


def build_env() -> dict[str, str]:
    return clean_env()


def soldr_path(env: dict[str, str] | None = None) -> str | None:
    """Return the path to the soldr binary, or None if not available."""
    path = None if env is None else env.get("PATH")
    found = shutil.which("soldr") if path is None else shutil.which("soldr", path=path)
    if found:
        return found
    # `./install` (the documented default) puts soldr in the repo's .venv,
    # which is not on PATH for `.venv/bin/python -m ci.xbuild ...` run without
    # activating the venv. Probe it so the local path does not silently miss
    # the repo's own recommended install (#1017).
    scripts = "Scripts" if os.name == "nt" else "bin"
    binary = "soldr.exe" if os.name == "nt" else "soldr"
    candidate = repo_root() / ".venv" / scripts / binary
    if candidate.is_file():
        return str(candidate)
    return None


def cargo_argv(subcommand: list[str], env: dict[str, str] | None = None) -> list[str]:
    """Prefer the explicit Soldr driver when available, including under shims.

    Keep existing fallback behavior for environments without the driver.
    """
    soldr = soldr_path(env)
    if soldr:
        return [soldr, "cargo", *subcommand]

    if _soldr_shims_requested(env):
        return ["cargo", *subcommand]

    cargo = None if env is None else env.get("CARGO")
    if cargo:
        return [cargo, *subcommand]

    return ["cargo", *subcommand]


def maturin_argv(subcommand: list[str], env: dict[str, str] | None = None) -> list[str]:
    """Return the maturin argv, always using the dev-venv install.

    Originally this routed through `soldr maturin` (issues #27, #68) on the
    assumption that soldr would resolve maturin via the venv. In practice,
    `soldr <tool>` always tries to fetch the tool from GitHub Releases,
    and PyO3/maturin only publishes `x86_64-unknown-linux-musl.tar.gz` /
    `aarch64-unknown-linux-musl.tar.gz` for Linux — there is no
    `*-unknown-linux-gnu` asset. soldr's asset matcher rejects the musl
    archives on GNU Linux runners (its musl→gnu fallback rule does not
    fire here), producing `tool not found: no asset matches target
    x86_64-unknown-linux-gnu` and breaking every Linux Build job.

    maturin is already pinned in pyproject.toml dev deps and installed into
    the uv-managed venv on every runner, so `python -m maturin` works uniformly
    across platforms without any GitHub-Releases lookup. In CI, setup-soldr's
    cargo shim remains first on PATH and `clean_env()` removes explicit
    CARGO/RUSTC/RUSTC_WRAPPER overrides, so the cargo process spawned by
    maturin still goes through soldr/zccache. The Windows MSVC pin is
    preserved with RUSTUP_TOOLCHAIN and CARGO_BUILD_TARGET.
    """
    return [sys.executable, "-m", "maturin", *subcommand]
