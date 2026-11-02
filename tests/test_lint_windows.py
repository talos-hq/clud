"""`bash lint` type-checks every declared cross target through `soldr lint` (#1441, #2370)."""

from __future__ import annotations

from pathlib import Path

import tomllib

from ci import lint

ROOT = Path(lint.__file__).resolve().parent.parent


def _soldr_metadata() -> dict:
    with (ROOT / "Cargo.toml").open("rb") as fh:
        return tomllib.load(fh)["workspace"]["metadata"]["soldr"]


def test_declared_targets_cover_the_native_lanes() -> None:
    targets = _soldr_metadata()["targets"]
    # The remote lanes build these; local lint must type-check the same set.
    for triple in ("x86_64-pc-windows-msvc", "aarch64-apple-darwin", "x86_64-apple-darwin"):
        assert triple in targets


def test_soldr_lint_steps_leave_dylint_to_the_pinned_nightly_pass() -> None:
    # `bash lint` runs Dylint itself with the pinned nightly; a bare
    # `soldr lint` step would run it without that pin and fail.
    assert _soldr_metadata()["lint"]["steps"] == ["fmt", "clippy"]


def test_clippy_goes_through_soldr_lint_for_every_target() -> None:
    assert lint.clippy_argv("/x/soldr", host_only=False) == ["/x/soldr", "lint", "rust"]


def test_host_only_skips_the_cross_targets() -> None:
    assert lint.clippy_argv("/x/soldr", host_only=True) == [
        "/x/soldr",
        "lint",
        "rust",
        "--host-only",
    ]
