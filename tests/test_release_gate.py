"""The release recomputes full CI on the exact candidate inside its own run."""

from __future__ import annotations

import re
from pathlib import Path

WORKFLOWS = Path(__file__).resolve().parent.parent / ".github" / "workflows"


def test_ci_is_callable_with_a_candidate_that_resolves_the_full_tier() -> None:
    text = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    call = text.split("  workflow_call:\n", 1)[1].split("\n\n", 1)[0]
    assert "candidate_sha:" in call
    assert "required: true" in call
    # A call arrives as the caller's tag push; the candidate makes it a
    # dispatch for provenance and mode resolution, which selects full.
    mapping = "EVENT_NAME: ${{ inputs.candidate_sha && 'workflow_dispatch' || github.event_name }}"
    assert text.count(mapping) == 2
    assert "github.event_name == 'workflow_dispatch'" not in text.split("\njobs:", 1)[1]


def test_a_release_run_consults_no_attestation_or_reuse() -> None:
    # #2202: release ignores attestations, always. Every step that can waive
    # a job keys on a PR or a main push, which a tag-push call never is.
    text = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    for step in (
        "Default-branch reuse (enforce)",
        "Select jobs to skip on a verified reuse",
    ):
        body = text.split(f"- name: {step}\n", 1)[1].split("\n      - ", 1)[0]
        condition = body.split("if: ", 1)[1].split("\n", 1)[0]
        assert condition in ("github.event_name == 'push' && github.ref == 'refs/heads/main'",), (
            step,
            condition,
        )


def test_release_workflow_gates_every_publish_path() -> None:
    text = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    assert "candidate_sha: ${{ steps.meta.outputs.candidate_sha }}" in text
    gate = text.split("  full-ci-gate:\n", 1)[1].split("\n\n", 1)[0]
    assert "uses: ./.github/workflows/ci.yml" in gate
    assert "candidate_sha: ${{ needs.preflight.outputs.candidate_sha }}" in gate
    assert "ci.release_gate" not in text
    assert "needs: [preflight, full-ci-gate]" in text
    assert "needs: [preflight, full-ci-gate, release-matrix]" in text
    publish_needs = (
        "needs: [preflight, full-ci-gate, snapshot-public, build, wheel-smoke, "
        "windows-signature-smoke, build-static-musl, verify-static-musl-arm64]"
    )
    assert publish_needs in text
    assert "name: Publish final bytes as public prerelease" in text
    assert "prerelease: true" in text
    assert "make_latest: false" in text
    pypi_needs = (
        "needs: [preflight, full-ci-gate, build, wheel-smoke, windows-signature-smoke, "
        "candidate-acceptance-gate, verify-prior-public]"
    )
    assert pypi_needs in text
    # #1545: the release wheel is pip-installed and run before either publish.
    smoke = text.split("  wheel-smoke:\n", 1)[1].split("\n\n", 1)[0]
    assert "needs: [preflight, release-matrix, build]" in smoke
    assert "uses: ./.github/workflows/_run-tests.yml" in smoke
    assert "suite: wheel-smoke" in smoke
    assert "needs: [preflight, publish-pypi, verify-prior-public]" in text
    assert "needs: [preflight, promote-release]" in text
    assert "mode: candidate" in text
    assert "mode: released" in text
    assert "rollback-public:" in text
    assert "python -m ci.public_release verify-rollback" in text
    assert "name: Execute release static musl ARM64" in text
    installer = (WORKFLOWS / "installer-check.yml").read_text(encoding="utf-8")
    assert "candidate-build:" in installer
    assert "candidate-host:" in installer
    assert "candidate-aggregate:" in installer
    assert "public-host:" in installer
    assert "public-nixos:" in installer
    assert "public-distros:" in installer
    assert "if: always() && github.event_name == 'pull_request' &&" in installer
    assert "contains(github.event.pull_request.labels.*.name, 'ci-full')" in installer
    assert "contains(github.event.pull_request.labels.*.name, 'ci:full')" in installer
    assert "build-installer:" not in text
    assert "clud-installer-ape" not in text
    assert "build-ape:" not in installer
    assert "ape-host:" not in installer
    assert "ape-core-tests:" not in installer
    assert "linux-distro:" not in installer
    assert "source_ref: ${{ needs.preflight.outputs.candidate_sha }}" in text
    assert "ref: ${{ needs.preflight.outputs.candidate_sha }}" in text
    assert "branches: [main]" not in text  # A main version bump cannot start a release.


def test_candidate_proof_is_bound_to_dispatch_title() -> None:
    text = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    assert "format('CI full {0}', inputs.candidate_sha)" in text


def test_tag_push_runs_public_reusable_jobs_and_fails_if_they_skip() -> None:
    installer = (WORKFLOWS / "installer-check.yml").read_text(encoding="utf-8")
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    # Reusable jobs inherit the caller's event (a tag push), so event_name
    # cannot identify a workflow_call inside the called workflow.
    assert "github.event_name == 'workflow_call'" not in installer
    assert "github.event_name != 'workflow_call'" not in installer
    assert "candidate-acceptance-gate:" in release
    assert "released-acceptance-gate:" in release
    assert "needs.verify-candidate-installer.result" in release
    assert "needs.verify-published-installer.result" in release
    for job in ("candidate-acceptance-gate", "released-acceptance-gate"):
        match = re.search(
            rf"(?ms)^  {job}:\n(.*?)(?=^  [a-z][a-z0-9-]*:\n|\Z)",
            release,
        )
        assert match is not None
        block = match.group(1)
        assert "if: always()" in block
        assert 'run: test "$RESULT" = success' in block


def test_public_guest_evidence_capture_uses_runner_tools() -> None:
    installer = (WORKFLOWS / "installer-check.yml").read_text(encoding="utf-8")
    # The Ubuntu Actions runners for these guests do not include ripgrep.
    assert "rg -m1 'PUBLIC_NIXOS_EVIDENCE '" not in installer
    assert "rg -m1 'PUBLIC_DISTRO_EVIDENCE '" not in installer
    assert (
        "grep -m1 'PUBLIC_NIXOS_EVIDENCE ' public-nixos.log "
        "> public-nixos-evidence.txt" in installer
    )
    assert (
        "grep -m1 'PUBLIC_DISTRO_EVIDENCE ' public-distro.log "
        "> public-distro-evidence.txt" in installer
    )


def test_public_host_evidence_is_required() -> None:
    installer = (WORKFLOWS / "installer-check.yml").read_text(encoding="utf-8")
    match = re.search(r"(?ms)^  public-host:\n(.*?)(?=^  public-nixos:\n)", installer)
    assert match is not None
    upload = match.group(1).split("- name: Retain public host evidence\n", 1)[1]
    assert "if-no-files-found: error" in upload
