"""Unit tests for the CI target matrix.

The matrix is the contract between ci/ci_matrix.py and the `strategy: matrix:`
blocks in .github/workflows/ci.yml and auto-release.yml. A typo here is only
discoverable by pushing to CI, which is exactly the slow feedback loop the
redesign exists to eliminate -- so it is tested here instead.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

import pytest

from ci.ci_matrix import (
    SDIST_TARGET,
    SUITES,
    TARGETS,
    build_matrix,
    exec_matrix,
    release_matrix,
    resolve_tier,
    selected,
    static_musl_matrix,
)

CI_YML = Path(__file__).resolve().parent.parent / ".github" / "workflows" / "ci.yml"
BUILD_YML = CI_YML.with_name("_build-target.yml")


def test_final_gate_uses_shared_source_and_independent_skip_proofs() -> None:
    """#362: the adopter declares policy; ci-lint owns skip acceptance."""
    text = CI_YML.read_text(encoding="utf-8")
    gate = text.split("\n  ci-ok:\n", 1)[1]
    assert "ci-lint gate" in gate
    assert "--workflow-plan ci.yml" in gate
    assert "--attested-workflow" not in gate
    assert "--default-branch-reuse reuse-check.json" in gate
    assert '--event "$GITHUB_EVENT_PATH"' in gate
    assert "REUSE_EVIDENCE: ${{ needs.static.outputs.reuse_evidence }}" in gate
    assert "reuse_evidence: ${{ steps.reuse.outputs.evidence_json }}" in text
    assert "MINIMAL:" not in gate
    assert "for result in" not in gate


@pytest.mark.parametrize(
    "path",
    [
        BUILD_YML,
        CI_YML.with_name("rm-protection-docker.yml"),
        CI_YML.parent.parent.parent / "bosn.toml",
    ],
)
def test_builds_leave_compile_concurrency_to_soldr(path: Path) -> None:
    text = path.read_text(encoding="utf-8")
    assert not re.search(r"^\s*(?:SOLDR_JOBS|CARGO_BUILD_JOBS)\s*[:=]", text, re.MULTILINE)


def test_every_target_is_unique():
    triples = [target.triple for target in TARGETS]
    assert len(triples) == len(set(triples))


def test_core_tier_avoids_hosted_macos():
    """The ci-test tier may add Windows, but macOS is reserved for full CI."""
    core = selected("core")
    families = {triple.split("-")[2] for triple in (target.triple for target in core)}
    assert families == {"linux", "windows"}


def test_full_and_release_include_both_hosted_macos_architectures():
    full = {target.triple for target in selected("full")}
    assert {"aarch64-apple-darwin", "x86_64-apple-darwin"} <= full
    release = {entry["target"] for entry in release_matrix()["include"]}
    assert {"aarch64-apple-darwin", "x86_64-apple-darwin"} <= release
    workflow = CI_YML.read_text(encoding="utf-8")
    arm = workflow.split("\n  build-macos-arm:\n", 1)[1].split("\n  test-macos-arm:\n", 1)[0]
    assert "if: needs.static.outputs.mode == 'full'" in arm
    gate = workflow.split("\n  ci-ok:\n", 1)[1]
    assert "- test-macos-arm" in gate
    assert "- test-macos-x64" in gate


def test_release_keeps_six_wheels_and_builds_two_separate_static_linux_assets() -> None:
    wheels = release_matrix()["include"]
    musl = static_musl_matrix()["include"]
    assert len(wheels) == 6
    assert {row["target"] for row in musl} == {
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    }
    assert all(row["strategy"] == "soldr" for row in musl)
    assert all(row["artifact"].startswith("standalone-musl-") for row in musl)
    assert not any(row["artifact"].startswith("wheels-") for row in musl)
    assert all(row["runs-on"] == "ubuntu-24.04" for row in musl)


def test_full_tier_is_a_superset_of_core():
    core = {target.triple for target in selected("core")}
    full = {target.triple for target in selected("full")}
    assert core < full
    assert full == {target.triple for target in TARGETS}


def test_minimal_and_extended_modes_are_strict_subsets():
    minimal = {target.triple for target in selected("minimal")}
    extended = {target.triple for target in selected("extended")}
    full = {target.triple for target in selected("full")}
    assert minimal == {"x86_64-unknown-linux-gnu"}
    assert minimal < extended < full


@pytest.mark.parametrize(
    ("event", "dispatch", "labels", "expected"),
    [
        ("pull_request", "", "", "minimal"),
        ("pull_request", "", "ci-test", "extended"),
        ("pull_request", "", "ci-full", "full"),
        ("pull_request", "", "ci-test,ci-full", "full"),
        ("pull_request", "", "ci:full", "full"),
        ("push", "", "", "minimal"),
        ("merge_group", "", "", "minimal"),
        ("schedule", "", "", "full"),
    ],
)
def test_resolve_tier(event, dispatch, labels, expected):
    assert resolve_tier(event, dispatch, labels) == expected


def test_full_dispatch_requires_exact_candidate_sha():
    assert resolve_tier("workflow_dispatch", "full", "", "a" * 40, "a" * 40, True) == "full"
    assert resolve_tier("workflow_dispatch", "full", "", "a" * 40, "b" * 40, True) == "full"
    with pytest.raises(ValueError, match="provenance"):
        resolve_tier("workflow_dispatch", "full", "", "a" * 40, "b" * 40, False)
    with pytest.raises(ValueError, match="candidate"):
        resolve_tier("workflow_dispatch", "full", "")


def test_pr_and_dispatch_source_ref_is_pinned_in_every_job():
    text = CI_YML.read_text(encoding="utf-8")
    assert "github.event.pull_request.head.sha" in text
    assert "git merge-base --is-ancestor" in text
    assert "git diff --quiet" in text
    for name in ("_build-target.yml", "_run-tests.yml", "_dylint.yml"):
        reusable = CI_YML.with_name(name).read_text(encoding="utf-8")
        assert "source_ref:" in reusable, name
        assert "ref: ${{ inputs.source_ref || github.sha }}" in reusable, name
    # 14 build/test/dylint jobs, the #1323 real-harness job, and the Linux x64
    # clippy job that runs beside the build.
    assert text.count("source_ref: ${{ needs.static.outputs.source_ref }}") == 16
    # The static-checks lint job checks out the same pinned SHA.
    static_checks = text.split("\n  static-checks:\n", 1)[1].split("\n\n  ", 1)[0]
    assert "ref: ${{ needs.static.outputs.source_ref }}" in static_checks


def test_harness_suite_runs_in_extended_and_full_mode_and_is_gated():
    text = CI_YML.read_text(encoding="utf-8")
    after = text.split("\n  test-linux-x64-harness:\n", 1)[1]
    block = after.split("\n  build-windows-x64:\n", 1)[0]
    assert "needs.static.outputs.mode == 'extended'" in block
    assert "needs.static.outputs.mode == 'full'" in block
    assert "suite: harness" in block
    gate = text.split("\n  ci-ok:\n", 1)[1]
    assert "- test-linux-x64-harness" in gate
    run_tests = CI_YML.with_name("_run-tests.yml").read_text(encoding="utf-8")
    assert "@anthropic-ai/claude-code@" in run_tests
    assert "if: inputs.suite == 'harness'" in run_tests


def test_pinned_claude_code_never_installs_into_the_node_toolcache():
    # zackees/ci.yml#404: a bare `npm install -g` writes the shared Node
    # toolcache, which non-root runners do not own (EACCES).
    run_tests = CI_YML.with_name("_run-tests.yml").read_text(encoding="utf-8")
    step = run_tests.split("- name: Install pinned Claude Code\n", 1)[1]
    step = step.split("\n      - name:", 1)[0]
    assert '--prefix "$prefix"' in step
    assert 'prefix="$RUNNER_TEMP/' in step
    assert '"$prefix/bin" >> "$GITHUB_PATH"' in step
    assert "ACT" not in step
    assert "RUNNER_ENVIRONMENT" not in step


def test_each_target_executes_both_test_suites_and_gate_checks_every_target():
    text = CI_YML.read_text(encoding="utf-8")
    gate = text.split("\n  ci-ok:\n", 1)[1]
    for name in ("windows-x64", "macos-arm", "linux-arm", "windows-arm", "macos-x64"):
        match = re.search(
            rf"^  test-{name}:\n(.*?)(?=^  [a-z0-9-]+:|\Z)",
            text,
            re.MULTILINE | re.DOTALL,
        )
        assert match, name
        block = match.group(1)
        if name == "macos-x64":
            # Sharded integration; test_unit_shards.py pins the exact matrix.
            assert "suite: unit" in block
            assert "suite: integration" in block
        else:
            assert "suite: [unit, integration]" in block, name
        assert f"- test-{name}" in gate, name

    unit = text.split("\n  test-linux-x64-unit:\n", 1)[1].split(
        "\n  test-linux-x64-integration:\n", 1
    )[0]
    integration = text.split("\n  test-linux-x64-integration:\n", 1)[1].split(
        "\n  build-windows-x64:\n", 1
    )[0]
    assert "suite: unit" in unit
    assert "suite: integration" in integration
    assert "needs.static.outputs.mode == 'extended'" in integration
    assert "needs.static.outputs.mode == 'full'" in integration
    assert "- test-linux-x64-unit" in gate
    assert "- test-linux-x64-integration" in gate


def test_unknown_ci_label_fails_closed():
    with pytest.raises(ValueError, match="unknown"):
        resolve_tier("pull_request", "", "ci-ful")


def test_workflow_binds_dispatch_and_labels_to_mode():
    text = CI_YML.read_text(encoding="utf-8")
    assert "types: [opened, synchronize, reopened, labeled, unlabeled]" in text
    assert "candidate_sha:" in text
    assert "CANDIDATE_SHA: ${{ inputs.candidate_sha }}" in text
    assert "EVENT_SHA: ${{ github.sha }}" in text
    assert "run: python -m ci.ci_matrix" in text
    assert "mode: ${{ steps.mode.outputs.tier }}" in text
    assert "--workflow-plan ci.yml" in text


def test_every_build_waits_for_mode_and_full_is_complete():
    text = CI_YML.read_text(encoding="utf-8")
    dylint = text.split("\n  dylint:\n", 1)[1].split("\n  build-linux-x64:\n", 1)[0]
    assert "mode == 'full'" not in dylint
    assert "- dylint" in text.split("\n  ci-ok:\n", 1)[1]
    dylint_workflow = (CI_YML.parent / "_dylint.yml").read_text(encoding="utf-8")
    assert "runs-on: ubuntu-24.04" in dylint_workflow
    # A Linux-only matrix: the host and cross-check passes run in parallel
    # (#2061); #1512 removed native macOS/Windows Dylint runners.
    assert "pass: [host, x86_64-pc-windows-msvc, aarch64-apple-darwin]" in dylint_workflow
    assert "runs-on: macos" not in dylint_workflow
    assert "runs-on: windows" not in dylint_workflow
    for name in ("linux-x64", "windows-x64", "macos-arm", "linux-arm", "windows-arm", "macos-x64"):
        block = text.split(f"\n  build-{name}:\n", 1)[1].split("\n  test-", 1)[0]
        # Accept either `needs: static` or `needs: [static, ...]`: the
        # property under test is that every build WAITS FOR static's mode
        # output, not the exact scalar-vs-list spelling. `build-linux-x64`
        # legitimately lists `cache-maint` as a second dependency (the
        # pre-prune must precede its cache save), which a substring match on
        # the scalar form rejected while the behaviour was correct.
        assert "needs: static" in block or "needs: [static," in block, name
        # Same for the `if:` -- the property is that it consults static's
        # mode output. `always() && !cancelled() && ...` (needed to survive a
        # skipped `cache-maint`) no longer starts with `if: needs.static...`.
        assert "needs.static.outputs.mode" in block, name
    assert "--workflow-plan ci.yml" in text


def test_every_target_cross_compiles_on_linux():
    """No native-builder fallback survives: one build host for all six triples.

    That is the whole point of build-once/run-everywhere -- a single triple
    escaping to its own native runner reintroduces the cold C++ build the
    redesign exists to delete.
    """
    include = build_matrix(selected("full"))["include"]
    assert len(include) == len(TARGETS)
    assert all(entry["runs-on"] == "ubuntu-24.04" for entry in include)


def test_darwin_and_windows_use_the_soldr_blessed_cross_path():
    """The legacy passthrough in soldr's docs/CROSS_COMPILE.md is
    `cargo xwin` / `cargo zigbuild`; (cross-lint: allow — named on purpose)
    `soldr build` is the blessed surface. Pinning this here keeps a future
    edit from silently regressing to a hand-installed cross wrapper."""
    include = build_matrix(selected("full"))["include"]
    crossed = [
        entry for entry in include if "apple" in entry["target"] or "windows" in entry["target"]
    ]
    assert len(crossed) == 4
    assert all(entry["strategy"] == "soldr" for entry in crossed)


def test_no_target_uses_a_hand_installed_cross_wrapper_for_msvc_or_darwin():
    for target in TARGETS:
        if "windows" in target.triple or "apple" in target.triple:
            assert target.strategy == "soldr", target.triple


def test_cross_argv_never_routes_apple_or_msvc_through_a_banned_tool():
    """The behavioural half of #637, and the half a text scan cannot do.

    `ci/banned_cross_tools.py` catches a literal command with a literal target.
    It cannot follow a target held in a variable — and `cargo_argv` takes the
    target as an argument, so every real invocation in this repo is exactly the
    shape the text scan is blind to.

    So ask the real function. For every triple in the matrix, with every
    strategy it could plausibly be given, assert the argv it produces names no
    banned wrapper when the target is Apple or MSVC. A future edit that routes
    a darwin build through zigbuild fails here even though no file contains the
    string `zigbuild --target aarch64-apple-darwin`.
    """
    from ci.xbuild import cargo_argv

    banned = ("xwin", "zigbuild", "zig", "cross", "osxcross")  # cross-lint: allow
    soldr_owned = [t for t in TARGETS if "apple" in t.triple or "windows" in t.triple]
    assert soldr_owned, "matrix has no crossed targets; this test would be vacuous"

    for target in soldr_owned:
        # Not just the strategy the matrix assigns today: a future edit could
        # change it, and the point is that no strategy may reach a banned tool
        # for these triples.
        for strategy in ("native", "zigbuild", "soldr"):
            for subcommand in (["build"], ["test", "--no-run"], ["clippy"]):
                if strategy == "zigbuild":
                    # Refused outright rather than silently producing a working
                    # zigbuild argv. Returning something safe would let the
                    # misconfiguration sit in the matrix unnoticed.
                    with pytest.raises(ValueError, match="soldr"):
                        cargo_argv(subcommand, target.triple, strategy)
                    continue
                argv = cargo_argv(subcommand, target.triple, strategy)
                assert not any(token in banned for token in argv), (
                    f"{target.triple} / {strategy} / {subcommand[0]} produced "
                    f"{argv}, which drives a banned cross tool"
                )


def test_gnu_linux_is_now_soldr_owned_and_refuses_zig():
    """soldr#2299 reversal: soldr 0.8.39's catalogue GNU toolchain replaced zig
    for *-unknown-linux-gnu, so routing a GNU/Linux build through zigbuild is
    refused, exactly as for Apple/MSVC. (Was the zig-stays-for-Linux counter-
    weight to #637.)"""
    from ci.xbuild import cargo_argv, is_soldr_owned

    assert is_soldr_owned("x86_64-unknown-linux-gnu")
    assert is_soldr_owned("aarch64-unknown-linux-gnu")
    with pytest.raises(ValueError, match="soldr"):
        cargo_argv(["build"], "aarch64-unknown-linux-gnu", "zigbuild")
    # The blessed surface is what a GNU/Linux build uses now.
    assert cargo_argv(["build"], "x86_64-unknown-linux-gnu", "soldr")[:2] == ["soldr", "build"]


def test_test_matrix_always_uses_native_runners():
    include = exec_matrix(selected("full"))["include"]
    assert len(include) == len(TARGETS) * len(SUITES)
    expected = {target.triple: target.exec_runs_on for target in TARGETS}
    for entry in include:
        assert entry["runs-on"] == expected[entry["target"]]
        assert entry["suite"] in SUITES


def test_release_matrix_ships_exactly_one_sdist():
    include = release_matrix()["include"]
    sdists = [entry for entry in include if entry["include-sdist"]]
    assert len(sdists) == 1
    assert sdists[0]["target"] == SDIST_TARGET


def test_release_matrix_artifact_names_are_unique_and_complete():
    include = release_matrix()["include"]
    artifacts = [entry["artifact"] for entry in include]
    assert len(artifacts) == len(TARGETS) == len(set(artifacts))
    assert all(name.startswith("wheels-") for name in artifacts)


def test_release_matrix_uses_the_same_strategies_as_ci():
    """Release must ship what CI tested.

    If release built natively while CI only exercised cross-built binaries, CI
    would never have validated the artifact that ships.
    """

    def strategies(matrix):
        return {entry["target"]: entry["strategy"] for entry in matrix["include"]}

    assert strategies(build_matrix(selected("full"))) == strategies(release_matrix())


def test_doc_tests_run_on_a_live_matrix_triple():
    """#863: doc-tests were silently disabled when #859 retired the `native`
    strategy — the step's `if:` keyed on `strategy == 'native'`, which no lane
    passes, and it is the ONLY doc-test runner in CI (they produce no harness
    binary, so they cannot ride in the exec bundle).

    Pin the key to something the matrix can actually satisfy: the condition
    must name a target triple, that triple must exist in TARGETS, and no
    strategy comparison may guard the step.
    """
    build_target = CI_YML.parent / "_build-target.yml"
    text = build_target.read_text(encoding="utf-8")
    step = re.search(r"- name: Doc tests\n\s+if: \$\{\{ (.+?) \}\}", text, re.DOTALL)
    assert step, "_build-target.yml has no conditional Doc tests step"
    condition = step.group(1)

    assert "strategy" not in condition, (
        "doc-tests must not key on the strategy — that is how #863 silently "
        f"disabled them: {condition}"
    )
    assert "inputs.doctest" in condition, condition
    lint = CI_YML.read_text(encoding="utf-8").split("\n  lint-linux-x64:\n", 1)[1]
    assert "doctest: true" in lint.split("\n\n  ", 1)[0], "the Linux x64 Clippy job must run them"
    named = re.search(r"inputs\.target == '([^']+)'", condition)
    assert named, f"doc-test condition must pin a target triple: {condition}"
    live = {target.triple for target in TARGETS}
    assert named.group(1) in live, (
        f"doc-tests keyed on {named.group(1)}, which is not in the matrix — they would never run"
    )


def test_ci_yml_covers_exactly_the_targets_table():
    """ci.yml spells out one build/test job pair per triple; keep them in sync.

    The pairs cannot be a matrix: `needs:` on a matrix job is all-or-nothing in
    GitHub Actions, so a single test matrix would make the fast Linux lane wait
    on the slowest cross-build. The cost of that workaround is hand-written
    YAML, and the cost of hand-written YAML is drift -- which is what this test
    exists to prevent.
    """
    text = CI_YML.read_text(encoding="utf-8")
    declared = set(re.findall(r"^      target: (\S+)$", text, re.MULTILINE))
    assert declared == {target.triple for target in TARGETS}

    # Every build job must have a matching test job that depends on it, or the
    # triple gets compiled and then never exercised.
    build_jobs = set(re.findall(r"^  build-([a-z0-9-]+):$", text, re.MULTILINE))
    test_needs = set(re.findall(r"^    needs: \[static, build-([a-z0-9-]+)\]$", text, re.MULTILINE))
    assert build_jobs == test_needs
    assert len(build_jobs) == len(TARGETS)


def test_ci_yml_job_configuration_matches_the_targets_table():
    """The hand-written jobs must preserve every registry field, not just names."""
    text = CI_YML.read_text(encoding="utf-8")
    expected = {target.triple: target for target in TARGETS}

    build_blocks = re.findall(
        r"^  build-[a-z0-9-]+:\n(.*?)(?=^  [a-z0-9-]+:|\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    declared_builds = {}
    for block in build_blocks:
        triple = re.search(r"^      target: (\S+)$", block, re.MULTILINE)
        runner = re.search(r"^      runs-on: (\S+)$", block, re.MULTILINE)
        strategy = re.search(r"^      strategy: (\S+)$", block, re.MULTILINE)
        assert triple
        assert runner
        assert strategy
        declared_builds[triple.group(1)] = (runner.group(1), strategy.group(1))

    assert declared_builds == {
        triple: (target.build_runs_on, target.strategy) for triple, target in expected.items()
    }

    test_blocks = re.findall(
        r"^  test-[a-z0-9-]+:\n(.*?)(?=^  [a-z0-9-]+:|\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    declared_exec = {}
    for block in test_blocks:
        triple = re.search(r"^      target: (\S+)$", block, re.MULTILINE)
        runner = re.search(r"^      runs-on: (\S+)$", block, re.MULTILINE)
        assert triple
        assert runner
        declared_exec[triple.group(1)] = runner.group(1)

    assert declared_exec == {triple: target.exec_runs_on for triple, target in expected.items()}


def test_ci_yml_never_requests_a_release_profile():
    """Requirement: only the release pipeline builds --release.

    _build-target.yml also guards this at run time on `github.workflow`, but a
    static check fails in `bash test` rather than after a runner has spun up.
    """
    assert "profile: release" not in CI_YML.read_text(encoding="utf-8")


def test_matrices_are_json_serializable_for_github_actions():
    """`fromJSON()` in the workflow needs valid, single-line JSON."""
    for matrix in (
        build_matrix(selected("full")),
        exec_matrix(selected("core")),
        release_matrix(),
        static_musl_matrix(),
    ):
        encoded = json.dumps(matrix, separators=(",", ":"))
        assert "\n" not in encoded
        assert json.loads(encoded) == matrix


def test_ci_windows_label_selects_linux_x64_and_windows_x64():
    """#1310 fast Windows iteration; #1652 keeps the routine Linux x64 lanes."""
    assert resolve_tier("pull_request", "", "ci-windows") == "windows"
    assert [t.triple for t in selected("windows")] == [
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ]
    # Merge-gating tiers win when both labels are present.
    assert resolve_tier("pull_request", "", "ci-windows,ci-test") == "extended"
    assert resolve_tier("pull_request", "", "ci-windows,ci-full") == "full"


def test_ci_macos_label_selects_linux_x64_and_both_darwin_triples():
    """Native macOS evidence without ci-full's Windows and Linux ARM lanes."""
    assert resolve_tier("pull_request", "", "ci-macos") == "macos"
    assert [t.triple for t in selected("macos")] == [
        "x86_64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
    ]
    assert resolve_tier("pull_request", "", "ci-macos,ci-test") == "extended"
    assert resolve_tier("pull_request", "", "ci-macos,ci-full") == "full"
    with pytest.raises(ValueError, match="ci-full"):
        resolve_tier("pull_request", "", "ci-macos,ci-windows")
    text = CI_YML.read_text(encoding="utf-8")
    for job in ("build-macos-arm", "build-macos-x64"):
        block = text.split(f"\n  {job}:\n", 1)[1].split("\n\n", 1)[0]
        assert "needs.static.outputs.mode == 'macos'" in block, job
    for job in ("build-windows-x64", "build-windows-arm", "build-linux-arm"):
        block = text.split(f"\n  {job}:\n", 1)[1].split("\n\n", 1)[0]
        assert "'macos'" not in block, job
    integration = text.split("\n  test-linux-x64-integration:\n", 1)[1].split("\n\n", 1)[0]
    assert "'macos'" not in integration


def test_ci_windows_mode_runs_and_gates_on_the_routine_linux_lanes():
    """#1652: with no merge queue, a green `CI OK` in windows mode must imply the
    routine (minimal) Linux lanes passed on the same run, hence the same SHA."""
    text = CI_YML.read_text(encoding="utf-8")
    for job in ("lint-linux-x64", "build-linux-x64", "test-linux-x64-unit"):
        block = text.split(f"\n  {job}:\n", 1)[1].split("\n\n", 1)[0]
        assert "mode != 'windows'" not in block, job
    integration = text.split("\n  test-linux-x64-integration:\n", 1)[1].split("\n\n", 1)[0]
    assert "needs.static.outputs.mode == 'extended'" in integration
    assert "needs.static.outputs.mode == 'full'" in integration
    assert "needs.static.outputs.mode == 'windows'" not in integration
    windows = text.split("\n  build-windows-x64:\n", 1)[1].split("\n\n", 1)[0]
    assert "needs.static.outputs.mode == 'windows'" in windows
    gate = text.split("\n  ci-ok:\n", 1)[1]
    for job in (
        "static",
        "dylint",
        "lint-linux-x64",
        "build-linux-x64",
        "test-linux-x64-unit",
        "build-windows-x64",
        "test-windows-x64",
    ):
        assert f"- {job}" in gate, job


def test_linux_x64_clippy_runs_beside_the_build_not_inside_it():
    """Clippy sat serially ahead of the compile in the job the unit lane waits on.

    It produces nothing the unit lane consumes, so on the PR critical path it was
    pure delay. It now has its own job, gated exactly like the build, required by
    `CI OK`, and sharing the build's cache namespace read-only so the two do not
    race to write one immutable key.
    """
    text = CI_YML.read_text(encoding="utf-8")
    lint = text.split("\n  lint-linux-x64:\n", 1)[1].split("\n  build-linux-x64:\n", 1)[0]
    build = text.split("\n  build-linux-x64:\n", 1)[1].split("\n  test-linux-x64-unit:\n", 1)[0]
    assert "needs: static" in lint
    assert "if: needs.static.outputs.mode != ''" in lint
    assert "clippy: true" in lint
    assert "compile: false" in lint
    assert "bundle: false" in lint
    assert 'save-cache: "false"' in lint
    assert "target: x86_64-unknown-linux-gnu" in lint
    assert "clippy: false" in build
    assert "clippy: true" not in build
    gate = text.split("\n  ci-ok:\n", 1)[1]
    assert "- lint-linux-x64" in gate
    reusable = (CI_YML.parent / "_build-target.yml").read_text(encoding="utf-8")
    assert "inputs.compile &&" in reusable
    setup = (CI_YML.parent.parent / "actions" / "setup-build" / "action.yml").read_text(
        encoding="utf-8"
    )
    # The caller's policy reaches setup-soldr unchanged: its `auto` already
    # skips every ref but the default branch (zackees/setup-soldr#587).
    assert "save-cache: ${{ inputs.save-cache }}" in setup


def test_workflows_use_the_local_runner_signal_only_under_an_exception() -> None:
    """zackees/ci.yml ACT-003: stock actions work under bosn ci -> act2 as
    written. `ACT=true` is the local-runner signal for tuning a cache (size,
    pruning, compression), and actions read it themselves. A workflow may
    branch on it only where an action has no such knob, under a filed
    zackees/ci.yml exception named in the comment just above (#1740)."""
    github = CI_YML.parent.parent
    offenders = []
    for path in sorted(github.rglob("*.y*ml")):
        lines = path.read_text(encoding="utf-8").splitlines()
        for index, line in enumerate(lines):
            if "env.ACT" not in line:
                continue
            context = "\n".join(lines[max(0, index - 8) : index])
            if "zackees/ci.yml#" not in context:
                offenders.append(f"{path.relative_to(github)}:{index + 1}")
    assert offenders == [], offenders


ROUTINE_PR_JOBS = (
    "static-checks",
    "dylint",
    "lint-linux-x64",
    "build-linux-x64",
    "test-linux-x64-unit",
)

# zackees/ci.yml GEN-021 (#162): a verified byte-identical merge may skip
# the VALIDATION lanes, but never the CACHE WRITERS. Dylint and
# Build linux-x64 refresh the default-branch caches every PR restores from,
# so they run on every main push regardless of the reuse decision. This
# split is the load-bearing part of the wiring and is asserted, not assumed.
REUSE_SKIP_JOBS = (
    "static-checks",
    "lint-linux-x64",
    "test-linux-x64-unit",
)
WRITER_JOBS = ("dylint", "build-linux-x64")


def test_hosted_pr_checks_do_not_require_local_bosn_receipts() -> None:
    """A stamp-free PR runs real hosted checks; local receipts cannot waive them."""
    root = CI_YML.parent.parent.parent
    text = CI_YML.read_text(encoding="utf-8")
    config = (root / "local-gate.toml").read_text(encoding="utf-8")
    static = text.split("\n  static:\n", 1)[1].split("\n  static-checks:\n", 1)[0]
    assert "local-gate verify" not in text
    assert "steps.gate.outputs" not in text
    for section in ("gate", "gate.trust"):
        body = config.split(f"[{section}]\n", 1)[1].split("\n[", 1)[0]
        assert re.findall(r'^mode\s*=\s*"([^"]+)"', body, re.MULTILINE) == ["shadow"]
    gate = text.split("\n  ci-ok:\n", 1)[1].split("\n  nightly-report:\n", 1)[0]
    assert "--attested-workflow" not in gate
    assert "--workflow-plan ci.yml --needs needs.json" in gate
    assert "continue-on-error" not in static + gate
    assert "|| true" not in static + gate
    for job in ROUTINE_PR_JOBS:
        if job in REUSE_SKIP_JOBS:
            output = f"skip_{job}: ${{{{ steps.reuse_skip.outputs.skip_{job} }}}}"
            assert output in static, job
        else:
            assert f"skip_{job}" not in text, job
        assert f"- {job}" in gate, job
        assert f"skip_{job}" not in gate, job


def test_default_branch_reuse_skips_validation_but_never_the_cache_writers() -> None:
    """zackees/ci.yml GEN-021 strategy (i).

    The whole point of keeping Dylint and Build linux-x64 out of the reuse
    skip list is that they are the default-branch cache writers: every main
    push refreshes the caches that PRs restore from. If reuse could skip
    them, the caches would go stale for every downstream PR and the saving
    on the push would be paid back many times over on the PRs.

    So this asserts the negative directly -- the writers appear nowhere in
    the reuse-skip step -- rather than trusting the comment above it."""

    text = CI_YML.read_text(encoding="utf-8")
    static = text.split("\n  static:\n", 1)[1].split("\n  static-checks:\n", 1)[0]
    step = static.split("- name: Select jobs to skip on a verified reuse", 1)[1]
    step = step.split("\n  # Everything platform-independent", 1)[0]

    for job in REUSE_SKIP_JOBS:
        assert job in step.split("for job in", 1)[1].split("; do", 1)[0], job
    for job in WRITER_JOBS:
        assert job not in step.split("for job in", 1)[1].split("; do", 1)[0], (
            f"{job} is a cache writer and must always run"
        )
        assert f"steps.reuse_skip.outputs.skip_{job}" not in static, job

    # And the step is fail-closed: it emits nothing unless the decision said so.
    assert 'if [ "$REUSE" != "true" ]; then' in step
    assert "if: github.event_name == 'push' && github.ref == 'refs/heads/main'" in step
    # Enforcement, not shadow, or the skips would never happen.
    assert "--mode enforce" in static
    assert "--mode shadow" not in static


def test_preprune_is_ordered_before_writers_and_only_deletes_on_main_pushes() -> None:
    text = CI_YML.read_text(encoding="utf-8")
    maintenance = text.split("\n  cache-maint:\n", 1)[1].split("\n  dylint:\n", 1)[0]
    assert "if: github.event_name == 'push' && github.ref == 'refs/heads/main'" in maintenance
    assert "actions: write" in maintenance
    assert "uses: ./.github/actions/setup-exec" in maintenance
    assert '"$VENV_PY" -m ci.cache_lockfile_changed' in maintenance
    assert "--lockfile-changed --max-deletes 50" in maintenance
    for job in ("dylint", "build-linux-x64"):
        block = text.split(f"\n  {job}:\n", 1)[1].split("\n  #", 1)[0]
        assert "needs: [static, cache-maint]" in block
        assert "always() && !cancelled()" in block
        assert "needs.static.result == 'success'" in block
        assert "needs.cache-maint.result == 'success'" in block
        assert "needs.cache-maint.result == 'skipped'" in block
