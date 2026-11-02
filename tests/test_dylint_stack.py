"""Contract tests for the Dylint tool, library, and nightly pins.

Issue #911 normalized clud onto a supported Dylint and deleted the recovery
scaffolding that earlier 6.0.x releases needed. These tests guard both halves:
the version/nightly lockstep, and the *absence* of the scaffolding — the
latter matters because the failure it worked around is intermittent, so a
future debugging session could plausibly re-add the crutch instead of moving
the pin.
"""

from __future__ import annotations

import re
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent
DYLINT_VERSION = "6.2.0"
DYLINT_NIGHTLY = "nightly-2026-08-15"
#: Every Dylint lint crate. Each carries its own nightly pin, lockfile and
#: linker config, so each must satisfy the same lockstep.
LINT_DIRS = sorted(p.parent for p in (ROOT / "dylints").glob("*/Cargo.toml"))
WORKFLOW = ROOT / ".github" / "workflows" / "_dylint.yml"
# One triple per OS family: the workspace gates by OS, not architecture.
DYLINT_CROSS_TARGETS = ("x86_64-pc-windows-msvc", "aarch64-apple-darwin")


def _toml(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def _workflow_text() -> str:
    return WORKFLOW.read_text(encoding="utf-8")


def test_dylint_stack_versions_stay_in_lockstep() -> None:
    assert [p.name for p in LINT_DIRS] == [
        "ban_dirs_home_dir",
        "ban_manual_slash_normalize",
        "enforce_platform_boundary",
    ]
    for lint_dir in LINT_DIRS:
        assert not (lint_dir / "rust-toolchain.toml").exists(), lint_dir
        assert (lint_dir / "rust-toolchain").exists(), lint_dir
        lint_manifest = _toml(lint_dir / "Cargo.toml")
        toolchain = _toml(lint_dir / "rust-toolchain")
        assert lint_manifest["dependencies"]["dylint_linting"] == f"={DYLINT_VERSION}", lint_dir
        assert toolchain["toolchain"]["channel"] == DYLINT_NIGHTLY, lint_dir
    workflow = _workflow_text()

    assert "env -u RUSTUP_TOOLCHAIN soldr dylint prepare" in workflow
    assert "env -u RUSTUP_TOOLCHAIN soldr dylint --all -- --workspace --all-targets" in workflow
    assert 'SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS: "1"' in workflow
    assert "SOLDR_DYLINT_TOOLCHAIN: nightly-2026-08-15" in workflow
    assert "dylint: true" in workflow
    assert "soldr rustup component add --toolchain nightly-2026-08-15 rustfmt" in workflow
    assert "soldr --no-cache cargo fmt" in workflow
    # Every nightly the workflow names must be the one the lint crate pins;
    # a stray second date means the driver and the lints disagree.
    assert set(re.findall(r"nightly-\d{4}-\d{2}-\d{2}", workflow)) == {DYLINT_NIGHTLY}

    # The README tells developers what to install locally. Leaving it out of
    # the lockstep is how you get a green suite that hands people a
    # mismatched cargo-dylint — the tool/library skew #911 was about.
    readme = (ROOT / "dylints" / "README.md").read_text(encoding="utf-8")
    assert f"Soldr's verified {DYLINT_VERSION}" in readme
    assert set(re.findall(r"nightly-\d{4}-\d{2}-\d{2}", readme)) == {DYLINT_NIGHTLY}


def test_full_local_lint_runs_the_custom_dylint() -> None:
    script = (ROOT / "lint").read_text(encoding="utf-8")
    assert "-m ci.lint" in script
    assert "env -u RUSTUP_TOOLCHAIN soldr dylint prepare" in script
    assert "export SOLDR_DYLINT_TOOLCHAIN=nightly-2026-08-15" in script
    assert "env -u RUSTUP_TOOLCHAIN soldr dylint --all -- --workspace --all-targets" in script
    assert "--static-only" in script


def test_dylint_lockfile_matches_the_pinned_version() -> None:
    """The lockfile is committed, so it can drift from Cargo.toml silently."""
    for lint_dir in LINT_DIRS:
        lock = (lint_dir / "Cargo.lock").read_text(encoding="utf-8")
        assert f'name = "dylint_linting"\nversion = "{DYLINT_VERSION}"' in lock, lint_dir


def test_dylint_workflow_runs_one_plain_invocation_per_target() -> None:
    """One pass per checked target; no retry, alias reconstruction, or hand-built driver (#911)."""
    workflow = _workflow_text()

    # Exactly one managed workspace pass per target — the recovery path ran the
    # host pass twice. A late lint only sees code compiled for the checked
    # target, so the cross passes are what reach cfg(windows)/macOS modules.
    target_suffixes = re.findall(
        r"(?m)^\s*(?:run:\s*)?env -u RUSTUP_TOOLCHAIN soldr dylint --all\b(.*)$",
        workflow,
    )
    expected = ["-- --workspace --all-targets"] + [
        f"-- --workspace --all-targets --target {target}" for target in DYLINT_CROSS_TARGETS
    ]
    assert sorted(suffix.strip() for suffix in target_suffixes) == sorted(expected)
    assert "cargo install cargo-dylint" not in workflow

    # Target std comes from the nightly's prebuilt rust-std, never a std build.
    for target in DYLINT_CROSS_TARGETS:
        assert f"env -u RUSTUP_TOOLCHAIN soldr dylint prepare --target {target}" in workflow
    assert "build-std" not in workflow
    assert "rustup target add" not in workflow

    # The specific scaffolding that was removed. Each of these appearing again
    # means someone restored the workaround instead of moving the version pin.
    for banned in (
        "build_dylint_driver",
        "Build published Dylint driver",
        "Set up Dylint Python environment",
        "VENV_PY",
        "missing-alias",
        "libban_manual_slash_normalize@",
        "set +e",
        "for attempt in",
    ):
        assert banned not in workflow, (
            f"{banned!r} is Dylint recovery scaffolding removed in #911; "
            "if Dylint regressed, move the version pin instead of restoring it"
        )


def test_dylint_dependency_checks_stay_cached() -> None:
    """Dylint re-checks the whole dependency graph per target unless zccache runs.

    Measured on this workflow: warm zccache took the host/Windows/macOS passes
    from 2m45s/2m41s/1m21s to 28s/49s/69s at a 100% hit rate. A kill-switch
    here makes every pass cold again while setup-soldr still reports the layer.
    """
    workflow = _workflow_text()
    assert "ZCCACHE_DISABLE" not in workflow
    assert "soldr --no-cache cargo dylint" not in workflow
    assert "cache: true" in workflow


def test_dylint_driver_builder_is_gone() -> None:
    """The hand-built-driver script was retired with the workaround."""
    assert not (ROOT / "ci" / "build_dylint_driver.py").exists()


def test_lint_crate_links_through_dylint_link() -> None:
    """The actual fix behind #911's removed scaffolding.

    `dylint-link` names the cdylib `lib<name>@<toolchain>.so`, which is the
    filename `cargo dylint` looks up. Without this config the build succeeds
    and emits a plain `lib<name>.so`, and Dylint fails with "Could not find
    ... despite successful build" — the failure CI used to paper over by
    copying the artifact and retrying. Verified against Dylint 6.0.3, so this
    is not something a version bump makes redundant.
    """
    for lint_dir in LINT_DIRS:
        config = lint_dir / ".cargo" / "config.toml"
        assert config.exists(), f"{lint_dir.name} must configure the dylint-link linker"
        # Parsed, not substring-matched: the file carries a long comment *about*
        # the directive, so `"linker=dylint-link" in text` would still pass with
        # the directive commented out — resurrecting the exact failure this
        # guards. `cfg(all())` rather than a concrete triple so it applies on
        # whatever host builds the lint.
        parsed = _toml(config)
        assert parsed["target"]["cfg(all())"]["rustflags"] == ["-C", "linker=dylint-link"]
