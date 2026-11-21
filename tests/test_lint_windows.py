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


def test_soldr_lint_runs_the_dylint_libraries() -> None:
    # Lint never runs under Bosn: `soldr lint` is the one local linter, so its
    # step list must include Dylint (each library pins its nightly in
    # dylints/*/rust-toolchain.toml).
    assert _soldr_metadata()["lint"]["steps"] == ["fmt", "clippy", "dylint"]


LINT_JOBS = {"static-checks", "dylint", "lint-linux-x64"}


def _gate() -> dict:
    with (ROOT / "local-gate.toml").open("rb") as fh:
        return tomllib.load(fh)["gate"]


def test_soldr_lint_extra_runs_the_static_checks() -> None:
    # A host `soldr lint` lane covers the Static checks job through this extra.
    assert _soldr_metadata()["lint"]["extra"] == [["bash", "lint", "--static-only"]]


def test_local_gate_lint_lanes_run_soldr_lint_on_the_host() -> None:
    gate = _gate()
    lanes = gate["lanes"]
    assert lanes["dylint"]["run"] == ["soldr", "lint", "rust", "--check", "--cross-dylint"]
    assert lanes["clippy"]["run"] == ["soldr", "lint", "rust", "--check", "--host-only"]
    assert lanes["doctest"]["run"] == ["soldr", "cargo", "test", "--workspace", "--doc"]
    for name, lane in lanes.items():
        if lane["run"][0] != "bosn":
            assert lane["run"][0] == "soldr", name
            assert "bosn" not in lane["tools"], name
    covered = gate["trust"]["covered-by"]
    assert covered["ci.yml:static-checks"] == ["clippy"]
    assert covered["ci.yml:dylint"] == ["dylint"]
    assert covered["ci.yml:lint-linux-x64"] == ["clippy", "doctest"]


def test_clippy_goes_through_soldr_lint_for_every_target() -> None:
    assert lint.clippy_argv("/x/soldr", host_only=False) == ["/x/soldr", "lint", "rust"]


def test_host_only_skips_the_cross_targets() -> None:
    assert lint.clippy_argv("/x/soldr", host_only=True) == [
        "/x/soldr",
        "lint",
        "rust",
        "--host-only",
    ]


def test_local_gate_never_replays_a_lint_job_under_bosn() -> None:
    # The bare `bosn ci run --trigger pr` plan includes the Static checks,
    # Dylint and Clippy jobs, and `--job` takes one job. The gate command
    # names the one Bosn job (the Linux unit suite), every Bosn lane is that
    # command exactly, and no replay selection or job names a lint job.
    gate = _gate()
    assert "full-run" not in gate
    run = gate["run"]
    assert run[0] == "bosn"
    assert run[-2:] == ["--job", "test-linux-x64-unit"]
    for name, lane in gate["lanes"].items():
        if lane["run"][0] == "bosn":
            assert lane["run"] == run, name
    replay = gate["replay"]
    assert [item["job"] for item in replay["selections"]] == ["test-linux-x64-unit"]
    for job in replay["jobs"]:
        assert job["source-job"].split(":")[1] not in LINT_JOBS, job
        assert job["source-job"] != "_dylint.yml:dylint", job
