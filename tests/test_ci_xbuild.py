"""Focused contracts for the build-once cross-compile driver."""

from __future__ import annotations

import argparse
import struct
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest
from running_process import PIPE, RunningProcess

from ci import build_wheel, bundle, wheel_repair, xbuild

ROOT = Path(__file__).resolve().parent.parent


def test_bundle_scratch_does_not_collide_with_the_build_entrypoint() -> None:
    assert (bundle.ROOT / "build").is_file()
    assert bundle.CI_SCRATCH == bundle.ROOT / ".ci-build"
    assert bundle.CI_SCRATCH != bundle.ROOT / "build"


def test_linux_runner_wraps_cargo_with_memory_limit(monkeypatch) -> None:
    monkeypatch.setattr(xbuild.platform, "system", lambda: "Linux")
    assert xbuild._run_argv(["cargo", "clippy"]) == [
        "bash",
        "-c",
        "ulimit -v 13000000; exec \"$@\"",
        "clud-xbuild",
        "cargo",
        "clippy",
    ]


def test_non_linux_runner_does_not_wrap_cargo(monkeypatch) -> None:
    monkeypatch.setattr(xbuild.platform, "system", lambda: "Darwin")
    assert xbuild._run_argv(["cargo", "clippy"]) == ["cargo", "clippy"]


def test_soldr_strategy_routes_linking_builds_through_blessed_surface() -> None:
    target = "x86_64-pc-windows-msvc"
    assert xbuild.cargo_argv(["build", "--workspace", "--bins"], target, "soldr") == [
        "soldr",
        "build",
        "--workspace",
        "--bins",
        "--target",
        target,
    ]


def test_soldr_strategy_routes_non_build_verbs_through_the_target_hook() -> None:
    """setup-soldr's target hooks (#648): `soldr cargo <verb> --target`."""
    target = "aarch64-apple-darwin"
    assert xbuild.cargo_argv(["test", "--workspace", "--no-run"], target, "soldr") == [
        "soldr",
        "cargo",
        "test",
        "--workspace",
        "--no-run",
        "--target",
        target,
    ]


def test_soldr_setup_installs_target_for_plain_cargo_verbs() -> None:
    setup = ROOT / ".github" / "actions" / "setup-build" / "action.yml"
    text = setup.read_text(encoding="utf-8")
    prepare = '"${SOLDR_BINARY:-soldr}" prepare'
    install = 'rustup target add "${{ inputs.target }}"'

    assert prepare in text
    assert install in text
    assert text.index(prepare) < text.index(install)


def test_gnu_linux_build_routes_through_the_blessed_soldr_surface() -> None:
    """soldr#2299: GNU/Linux is soldr-owned now, so its linking build goes
    through `soldr build`, never the zig cross."""
    target = "aarch64-unknown-linux-gnu"
    argv = xbuild.cargo_argv(["build", "--workspace", "--bins"], target, "soldr")
    assert argv == ["soldr", "build", "--workspace", "--bins", "--target", target]


def test_gnu_linux_zigbuild_is_refused() -> None:
    """Routing a GNU/Linux build through zigbuild is refused structurally, the
    same guard that protects Apple/MSVC (#637)."""
    import pytest

    for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"):
        with pytest.raises(ValueError, match="soldr"):
            xbuild.cargo_argv(["build"], target, "zigbuild")


def test_static_musl_zigbuild_is_refused() -> None:
    import pytest

    for target in ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"):
        with pytest.raises(ValueError, match="soldr"):
            xbuild.cargo_argv(["build"], target, "zigbuild")


@pytest.mark.parametrize("profile", ["dev", "release"])
@pytest.mark.parametrize(
    ("target", "machine", "defer"),
    [
        ("x86_64-unknown-linux-musl", 62, False),
        ("aarch64-unknown-linux-musl", 183, True),
    ],
)
def test_static_musl_build_stages_one_verified_native_binary(
    tmp_path, monkeypatch, profile, target, machine, defer
) -> None:
    output_profile = "release" if profile == "release" else "debug"
    binary = tmp_path / "target" / target / output_profile / "clud"
    binary.parent.mkdir(parents=True)
    payload = bytearray(128)
    payload[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<HH", payload, 16, 2, machine)
    struct.pack_into("<Q", payload, 32, 64)
    struct.pack_into("<HH", payload, 54, 56, 1)
    struct.pack_into("<I", payload, 64, 1)
    binary.write_bytes(payload)
    commands = []
    monkeypatch.setattr(xbuild, "ROOT", tmp_path)
    monkeypatch.setattr(xbuild, "build_env", lambda *_: {})
    monkeypatch.setattr(xbuild, "_project_version", lambda: "2.9.0")
    monkeypatch.setattr(xbuild, "run", lambda command, _env: commands.append(command) or 0)
    native_calls = []
    monkeypatch.setattr(
        xbuild.process,
        "run",
        lambda *_args, **_kwargs: native_calls.append(_args)
        or SimpleNamespace(returncode=0, stdout="clud 2.9.0\n", stderr=""),
    )
    args = argparse.Namespace(
        target=target, strategy="soldr", profile=profile, defer_native_version_check=defer
    )
    assert xbuild.cmd_standalone(args) == 0
    expected = ["soldr", "build", "-p", "clud", "--bin", "clud"]
    if profile == "release":
        expected.append("--release")
    assert commands == [[*expected, "--target", target]]
    assert (tmp_path / "standalone" / f"clud-2.9.0-{target}").read_bytes() == payload
    assert len(native_calls) == (0 if defer else 1)
    assert not (tmp_path / "dist").exists()


def test_candidate_build_stages_exact_head_and_dev_binary(tmp_path, monkeypatch) -> None:
    from ci.installer_candidate import verify_artifact
    from tests.test_installer_verify_native_assets import native_payload

    target = "x86_64-unknown-linux-musl"
    source = tmp_path / "target" / target / "debug" / "clud"
    source.parent.mkdir(parents=True)
    stripped = native_payload("elf", "x86_64")
    original = stripped + b"debug-data"
    source.write_bytes(original)
    stripped_paths = []

    def strip_candidate(path: Path, triple: str) -> None:
        stripped_paths.append((path, triple))
        path.write_bytes(stripped)

    monkeypatch.setattr(build_wheel, "strip_elf_debug", strip_candidate)
    monkeypatch.setattr(xbuild, "ROOT", tmp_path)
    monkeypatch.setattr(xbuild, "build_env", lambda *_args: {})
    monkeypatch.setattr(xbuild, "_project_version", lambda: "2.8.14")
    commands = []
    monkeypatch.setattr(xbuild, "run", lambda argv, _env: commands.append(argv) or 0)
    monkeypatch.setattr(xbuild.process, "check_output", lambda *_args, **_kwargs: "a" * 40)
    args = argparse.Namespace(target=target, strategy="soldr", profile="dev")

    assert xbuild.cmd_candidate(args) == 0
    assert ["--features", "installer-ci-fixture"] == commands[0][6:8]
    assert verify_artifact(tmp_path / "candidate", target, "a" * 40, "2.8.14")
    assert len(stripped_paths) == 1
    assert stripped_paths[0][1] == target
    assert stripped_paths[0][0].read_bytes() == stripped
    assert source.read_bytes() == original
    args.profile = "release"
    with pytest.raises(ValueError, match="dev"):
        xbuild.cmd_candidate(args)


def test_release_linux_wheel_builds_without_zig() -> None:
    """The release GNU wheel links through soldr's blessed catalogue toolchain,
    not the old zig path. The glibc-2.17 floor comes from `soldr prepare`'s
    catalogue sysroot; maturin just tags the wheel manylinux2014. soldr#2299.
    """
    source = (ROOT / "ci" / "xbuild.py").read_text(encoding="utf-8")
    assert 'subcommand += ["--compatibility", "manylinux2014"]' in source
    assert 'subcommand += ["--zig"' not in source  # cross-lint: allow
    assert 'subcommand.append("--zig")' not in source  # cross-lint: allow
    assert "CARGO_ZIGBUILD_PYTHON_PATH" not in source


def test_release_profile_strips_debug_sections_but_keeps_runtime_symbols() -> None:
    import tomllib

    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    assert cargo["profile"]["release"]["strip"] == "debuginfo"


def test_release_linux_wheel_env_sets_no_whisper_vars(monkeypatch) -> None:
    """The release manylinux wheel no longer carries whisper's static C++ link
    mechanism (#1207).

    whisper-rs was removed, and with it the only C++ `-sys` crate in the graph:
    `Cargo.lock`'s native deps are `ring` and `blake3`, both C. Nothing emits
    `cargo:rustc-link-lib=dylib=stdc++` any more, so `WHISPER_LINK_CXX_STATIC`
    has no directive to flip and `-static-libstdc++` has no libstdc++ to
    bundle. soldr's catalogue sysroot (gcc-13.3.0-glibc-2.17-1, soldr#2238) is
    the whole manylinux_2_17 floor now.
    """
    monkeypatch.delenv("WHISPER_LINK_CXX_STATIC", raising=False)
    calls: list[tuple[list[str], dict[str, str]]] = []

    def fake_run(argv: list[str], env: dict[str, str]) -> int:
        calls.append((argv, env))
        # Stop before maturin actually runs -- the env is the whole assertion.
        return 1

    monkeypatch.setattr(xbuild, "run", fake_run)
    args = argparse.Namespace(
        target="x86_64-unknown-linux-gnu", strategy="soldr", profile="release"
    )
    assert xbuild.cmd_wheel(args) == 1

    assert len(calls) == 1, "the GNU release wheel is a single maturin invocation"
    argv, env = calls[0]
    assert "maturin" in argv
    assert "WHISPER_LINK_CXX_STATIC" not in env
    offenders = {key: value for key, value in env.items() if "static-libstdc++" in value}
    assert not offenders, f"static-libstdc++ RUSTFLAGS survived: {offenders}"


def test_release_wheel_stripping_receives_the_explicit_cross_target(monkeypatch, tmp_path) -> None:
    """The wheel post-processing parent must not rely on maturin's child env."""
    target = "aarch64-unknown-linux-gnu"
    wheel = tmp_path / "clud.whl"
    stripped: list[tuple[Path, str | None]] = []

    monkeypatch.setattr(xbuild, "build_env", lambda *_args: {})
    monkeypatch.setattr(xbuild, "run", lambda *_args, **_kwargs: 0)
    monkeypatch.setattr(build_wheel, "built_wheels", lambda: [wheel])
    monkeypatch.setattr(build_wheel, "prune_nonproduction_scripts", lambda _wheel, **_kw: False)
    monkeypatch.setattr(
        build_wheel,
        "remove_elf_debug_metadata",
        lambda candidate, *, target=None: stripped.append((candidate, target)) or True,
    )
    monkeypatch.setattr(build_wheel, "verify_no_elf_debug_sections", lambda _wheel: None)
    monkeypatch.setattr(build_wheel, "verify_wheel_scripts", lambda _wheel: 0)
    monkeypatch.setattr(xbuild, "verify_wheel_modes", lambda _wheel: 0)
    monkeypatch.setattr(wheel_repair, "repair_windows_gnu_wheel", lambda _wheel: None)
    monkeypatch.setattr(xbuild, "collect_debuginfo", lambda *_args: None)

    args = argparse.Namespace(target=target, strategy="soldr", profile="release")
    assert xbuild.cmd_wheel(args) == 0
    assert stripped == [(wheel, target)]


def test_windows_wheel_resolves_cargo_target_dir_for_binaries_and_companion(
    monkeypatch, tmp_path
) -> None:
    target = "x86_64-pc-windows-msvc"
    wheel = tmp_path / "clud.whl"
    seen: dict[str, Path] = {}
    monkeypatch.setattr(xbuild, "ROOT", tmp_path)
    monkeypatch.setattr(xbuild, "build_env", lambda *_args: {"CARGO_TARGET_DIR": "build-output"})
    monkeypatch.setattr(xbuild, "run", lambda *_args, **_kwargs: 0)
    monkeypatch.setattr(xbuild, "_project_version", lambda: "2.0.0")
    monkeypatch.setattr(xbuild, "collect_debuginfo", lambda *_args: None)

    def fake_build(**kwargs):
        seen["target_dir"] = kwargs["target_dir"]
        return wheel

    monkeypatch.setattr(build_wheel, "build_windows_wheel_from_binaries", fake_build)
    monkeypatch.setattr(build_wheel, "verify_wheel_scripts", lambda _wheel: 0)
    monkeypatch.setattr(xbuild, "verify_wheel_modes", lambda _wheel: 0)
    from ci import webterm_wheel

    monkeypatch.setattr(
        webterm_wheel,
        "add_companion",
        lambda _wheel, companion, _target: seen.update(companion=companion),
    )

    args = argparse.Namespace(target=target, strategy="soldr", profile="dev")
    assert xbuild.cmd_wheel(args) == 0
    assert seen["target_dir"] == tmp_path / "build-output"
    assert seen["companion"] == tmp_path / "build-output" / target / "debug" / "clud-webterm.exe"


def test_cargo_target_dir_preserves_absolute_and_default_paths(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(xbuild, "ROOT", tmp_path / "source")
    default = tmp_path / "source" / "clud-webterm" / "target"
    assert xbuild.cargo_target_dir({}, default) == default
    assert xbuild.cargo_target_dir({"CARGO_TARGET_DIR": str(tmp_path / "output")}, default) == (
        tmp_path / "output"
    )


def test_no_soldr_cross_verb_falls_through_to_bare_cargo() -> None:
    """Every verb clud runs under the soldr strategy uses soldr's surface (#648)."""
    for target in ("aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "x86_64-apple-darwin"):
        for verb in (["clippy", "--workspace"], ["test", "--no-run"], ["test", "--doc"]):
            argv = xbuild.cargo_argv(verb, target, "soldr")
            assert argv[:3] == ["soldr", "cargo", verb[0]], argv
            assert argv[-2:] == ["--target", target]


def test_native_strategy_stays_on_plain_cargo() -> None:
    argv = xbuild.cargo_argv(["clippy"], "x86_64-unknown-linux-gnu", "native")
    assert argv == ["cargo", "clippy", "--target", "x86_64-unknown-linux-gnu"]


def test_non_windows_targets_get_the_prepared_env_unchanged() -> None:
    prepared = {"SDKROOT": "/opt/soldr/MacOSX.sdk", "RUSTFLAGS": "-C debuginfo=0"}
    for target in ("aarch64-apple-darwin", "aarch64-unknown-linux-gnu"):
        assert xbuild.target_env(target, prepared) == prepared


def test_windows_env_keeps_the_advapi_contract() -> None:
    rustflags_key = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS"
    env = xbuild.target_env(
        "x86_64-pc-windows-msvc",
        {
            "RUSTFLAGS": "-C debuginfo=0 -C opt-level=3",
            rustflags_key: ("-C link-arg=/LIBPATH:/opt/xwin/sdk/lib/um/x86_64 -C opt-level=1"),
        },
    )
    # No compiler or CMake state is reconstructed: soldr prepare owns it.
    assert not {"CC", "CXX", "CXXFLAGS"} & set(env)
    assert not any(key.startswith(("CMAKE_", "WHISPER_", "GGML_", "BINDGEN_")) for key in env)
    assert "RUSTFLAGS" not in env
    assert env[rustflags_key] == (
        "-C link-arg=/LIBPATH:/opt/xwin/sdk/lib/um/x86_64 "
        "-C opt-level=1 "
        "-C debuginfo=0 "
        "-C opt-level=3 "
        "-C link-arg=advapi32.lib"
    )


def test_windows_soldr_env_preserves_encoded_sdk_paths_from_soldr_0_8_30() -> None:
    """soldr >= 0.8.30 exports the MSVC link config in the encoded variable.

    This previously raised, which was correct while the SDK paths lived only
    in the target-scoped variable that CARGO_ENCODED_RUSTFLAGS shadows. Once
    soldr moved them into the winning variable, refusing to proceed rejected
    the correct configuration and reddened every Windows lane.
    """
    encoded = "\x1f".join(
        [
            "-Clinker-flavor=lld-link",
            "-Clink-arg=/NODEFAULTLIB:libucrt.lib",
            "-Clink-arg=/LIBPATH:/opt/xwin/sdk/lib/um/x86_64",
        ]
    )
    env = xbuild.target_env(
        "x86_64-pc-windows-msvc",
        {"CARGO_ENCODED_RUSTFLAGS": encoded},
    )
    parts = env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
    # The prepared link configuration survives, in order...
    assert parts[:3] == encoded.split("\x1f")
    # ...and our own library is appended rather than replacing it.
    assert parts[-1] == "-Clink-arg=advapi32.lib"


def test_windows_soldr_env_folds_shadowed_rustflags_into_encoded() -> None:
    """Anything the encoded variable shadows must be merged, not dropped.

    CARGO_ENCODED_RUSTFLAGS outranks both RUSTFLAGS and
    target.<triple>.rustflags, so a toolchain that sets it without the SDK
    paths would otherwise silently lose them.
    """
    rustflags_key = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS"
    env = xbuild.target_env(
        "x86_64-pc-windows-msvc",
        {
            "CARGO_ENCODED_RUSTFLAGS": "-Clinker-flavor=lld-link",
            "RUSTFLAGS": "-C debuginfo=0",
            rustflags_key: "-C link-arg=/LIBPATH:/opt/xwin/sdk/lib/um/x86_64",
        },
    )
    parts = env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
    assert "/LIBPATH:/opt/xwin/sdk/lib/um/x86_64" in " ".join(parts)
    assert "debuginfo=0" in " ".join(parts)
    assert parts[-1] == "-Clink-arg=advapi32.lib"
    # Both shadowed sources are removed so nothing reads a stale value.
    assert "RUSTFLAGS" not in env
    assert rustflags_key not in env


def test_debuginfo_is_staged_outside_dist_so_it_cannot_reach_pypi(
    tmp_path, monkeypatch, capsys
) -> None:
    """The `.dwp` must never enter `dist/`.

    `_build-target.yml` uploads `dist/*` as the `wheels-*` artifact and
    `publish-pypi` hands that straight to twine, which rejects a non-package
    file -- the exact 400 that killed 2.7.2-2.7.4.
    """
    target = "x86_64-unknown-linux-gnu"
    release = tmp_path / "target" / target / "release"
    release.mkdir(parents=True)
    (release / "clud.dwp").write_bytes(b"dwarf")
    # Neither shim installs the crash reporter and each .dwp is ~91 MB of the
    # same DWARF, so only the main binary's symbols are published.
    (release / "clud-shim.dwp").write_bytes(b"dwarf")
    (release / "mock-agent.dwp").write_bytes(b"dwarf")
    monkeypatch.setattr(xbuild, "ROOT", tmp_path)
    monkeypatch.setattr(xbuild, "DEBUGINFO_DIR", tmp_path / "dist-debuginfo")

    staged = xbuild.collect_debuginfo(target, "release")

    assert [path.name for path in staged] == [f"clud-{target}.dwp"]
    assert all(path.parent == tmp_path / "dist-debuginfo" for path in staged)
    assert not (tmp_path / "dist").exists()
    assert f"clud-{target}.dwp (5 bytes)" in capsys.readouterr().out


def test_debuginfo_is_found_in_deps_when_cargo_does_not_uplift_it(
    tmp_path, monkeypatch, capsys
) -> None:
    """2.7.5 shipped with no sidecar because only the uplifted path was checked.

    The wheel shrank 101 MB -> 47 MB, proving the DWARF left the ELF, yet
    `release/clud.dwp` did not exist: under maturin's `cargo rustc --bin`,
    cargo uplifts the binary out of `deps/` but not reliably its `.dwp`. The
    hash suffix is matched exactly so a sibling bin is never mistaken for
    clud's own.
    """
    target = "x86_64-unknown-linux-gnu"
    deps = tmp_path / "target" / target / "release" / "deps"
    deps.mkdir(parents=True)
    (deps / "clud-2f8a1c9d4e6b7a05.dwp").write_bytes(b"dwarf")
    (deps / "clud_shim-9b3c7e1a2d4f6508.dwp").write_bytes(b"other")
    monkeypatch.setattr(xbuild, "ROOT", tmp_path)
    monkeypatch.setattr(xbuild, "DEBUGINFO_DIR", tmp_path / "dist-debuginfo")

    staged = xbuild.collect_debuginfo(target, "release")

    assert [path.name for path in staged] == [f"clud-{target}.dwp"]
    assert staged[0].read_bytes() == b"dwarf"
    assert not (tmp_path / "dist").exists()
    assert f"clud-{target}.dwp (5 bytes)" in capsys.readouterr().out


def test_missing_debuginfo_is_reported_but_never_fatal(tmp_path, monkeypatch) -> None:
    """Targets where `packed` writes no `.dwp` must not block a release."""
    target = "aarch64-pc-windows-msvc"
    (tmp_path / "target" / target / "release").mkdir(parents=True)
    monkeypatch.setattr(xbuild, "ROOT", tmp_path)
    monkeypatch.setattr(xbuild, "DEBUGINFO_DIR", tmp_path / "dist-debuginfo")

    assert xbuild.collect_debuginfo(target, "release") == []
    assert not (tmp_path / "dist-debuginfo").exists()


def test_by_path_invocation_is_rejected_with_the_module_form(tmp_path) -> None:
    """Issue #1017 blocker 1: `python ci/xbuild.py ...` used to die with

        ImportError: cannot import name 'process' from 'ci'
            (.../site-packages/ci/__init__.py)

    because running by path puts `ci/` on `sys.path[0]`, so `from ci import
    process` binds to an unrelated installed distribution. The traceback names
    the wrong package entirely, and this is a large part of why a release wheel
    could not be reproduced on a developer machine.
    """
    result = RunningProcess.run(
        [sys.executable, str(ROOT / "ci" / "xbuild.py"), "wheel", "--target", "x"],
        # Explicit pipes rather than `capture_output=True`: the latter leaves
        # `result.stderr` as None here, and the guard writes to stderr.
        stdout=PIPE,
        stderr=PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=ROOT,
    )

    assert result.returncode == 2, result.stderr
    # Names the real cause, not a missing symbol.
    assert "cannot be run by path" in result.stderr
    assert "unrelated installed `ci` package" in result.stderr
    # Hands over the exact command, arguments preserved so it can be pasted.
    assert "python -m ci.xbuild wheel --target x" in result.stderr
    # The misleading failure is gone.
    assert "cannot import name 'process'" not in result.stderr


def test_module_form_is_untouched_by_the_guard() -> None:
    """The guard must be a no-op for the invocation CI actually uses."""
    assert xbuild.__package__ == "ci"
    # Importable as a module means the guard did not fire at import time.
    assert hasattr(xbuild, "main")


@pytest.mark.parametrize("target", ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"])
def test_bundle_stripping_uses_the_target_elf_editor(
    monkeypatch, tmp_path: Path, target: str
) -> None:
    from ci import process

    binary = tmp_path / "target/debug/clud"
    harness = tmp_path / "tests/integration"
    for path in (binary, harness):
        path.parent.mkdir(parents=True)
        path.write_bytes(b"\x7fELFdebug-data")
    selected: list[str] = []
    calls: list[list[str]] = []

    def resolve(triple: str) -> str:
        selected.append(triple)
        return "/managed/target-objcopy"

    def strip(argv: list[str], **kwargs):
        calls.append(argv)
        Path(argv[-1]).write_bytes(b"\x7fELFstripped")
        return SimpleNamespace(returncode=0)

    monkeypatch.setattr(build_wheel, "resolve_elf_objcopy", resolve)
    monkeypatch.setattr(process, "run", strip)
    bundle._strip_debug(tmp_path, target)
    assert set(selected) == {target}
    assert calls == [
        ["/managed/target-objcopy", "--strip-debug", str(path)] for path in (binary, harness)
    ]
    assert binary.read_bytes() == harness.read_bytes() == b"\x7fELFstripped"


def test_bundle_rejects_an_unstripped_executable(monkeypatch, tmp_path: Path) -> None:
    from ci import process

    binary = tmp_path / "target/debug/clud"
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b"\x7fELFdebug-data")
    monkeypatch.setattr(build_wheel, "resolve_elf_objcopy", lambda target: "objcopy")
    monkeypatch.setattr(
        process,
        "run",
        lambda *args, **kwargs: SimpleNamespace(
            returncode=1,
            stderr="wrong architecture",
            stdout="",
        ),
    )
    with pytest.raises(RuntimeError, match=r"cannot strip aarch64.*wrong architecture"):
        bundle._strip_debug(tmp_path, "aarch64-unknown-linux-gnu")


@pytest.mark.parametrize("target", ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"])
def test_bundle_leaves_non_elf_targets_untouched(monkeypatch, tmp_path: Path, target: str) -> None:
    def unexpected(target: str) -> str:
        pytest.fail("non-ELF bundle must not resolve an ELF editor")

    monkeypatch.setattr(build_wheel, "resolve_elf_objcopy", unexpected)
    bundle._strip_debug(tmp_path, target)
