"""Recovery retains source, asset and acceptance gates; it never republishes bytes."""

import os
import textwrap
from copy import deepcopy
from pathlib import Path

import pytest
from running_process import PIPE, RunningProcess

from ci import release_resume

TAG = "v2.8.65"
SHA = "a" * 40


def proof():
    run = {
        "path": ".github/workflows/auto-release.yml",
        "event": "push",
        "head_branch": TAG,
        "head_sha": SHA,
        "status": "completed",
        "conclusion": "failure",
    }
    jobs = [
        {"name": f"{prefix}{index}", "conclusion": "success"}
        for prefix, count in release_resume.REQUIRED.items()
        for index in range(count)
    ]
    return run, jobs


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("path", ".github/workflows/ci.yml"),
        ("event", "pull_request"),
        ("head_branch", "main"),
        ("head_sha", "b" * 40),
        ("status", "in_progress"),
        ("conclusion", "success"),
    ],
)
def test_wrong_original_run_is_rejected(field, value):
    run, jobs = proof()
    run[field] = value
    with pytest.raises(ValueError, match="unchanged tag"):
        release_resume.require_run(run, jobs, TAG, SHA)


@pytest.mark.parametrize("prefix", release_resume.REQUIRED)
@pytest.mark.parametrize("state", ["missing", "failure", "skipped", "cancelled", "duplicate"])
def test_every_original_build_and_smoke_proof_is_required(prefix, state):
    run, jobs = proof()
    selected = next(job for job in jobs if job["name"].startswith(prefix))
    if state == "missing":
        jobs.remove(selected)
    elif state == "duplicate":
        jobs.append(deepcopy(selected))
    else:
        selected["conclusion"] = state
    with pytest.raises(ValueError, match="incomplete"):
        release_resume.require_run(run, jobs, TAG, SHA)


@pytest.mark.parametrize("event", ["push", "workflow_dispatch"])
def test_complete_failed_original_run_is_accepted(event):
    run, jobs = proof()
    run["event"] = event
    release_resume.require_run(run, jobs, TAG, SHA)


def test_paginated_original_proof_and_dispatch_origin(monkeypatch):
    run, jobs = proof()
    monkeypatch.setenv("GITHUB_EVENT_NAME", "workflow_dispatch")
    monkeypatch.setenv("GITHUB_REF", "refs/heads/main")
    responses = iter(
        [run, {"jobs": [{"name": "other", "conclusion": "success"}] * 100}, {"jobs": jobs}]
    )
    monkeypatch.setattr(release_resume, "api", lambda _path: next(responses))
    release_resume.validate_run("123", TAG, SHA)
    monkeypatch.setenv("GITHUB_REF", "refs/heads/feature")
    with pytest.raises(ValueError, match="dispatched from main"):
        release_resume.validate_run("123", TAG, SHA)


def snapshot_fixture():
    release = {
        "id": 1,
        "tag_name": TAG,
        "draft": False,
        "prerelease": True,
        "assets": [
            {
                "name": "binary",
                "id": 2,
                "size": 100,
                "digest": "sha256:" + SHA + "a" * 24,
                "state": "uploaded",
            }
        ],
    }
    snapshot = {"release_id": 1, "tag": TAG, "assets": release_resume.asset_snapshot(release)}
    return release, snapshot


@pytest.mark.parametrize(
    "change", ["id", "tag", "stable", "draft", "asset-id", "size", "digest", "extra"]
)
def test_published_identity_cannot_change(change):
    release, snapshot = snapshot_fixture()
    if change in ("id", "tag"):
        release["id" if change == "id" else "tag_name"] = "changed"
    elif change == "stable":
        release["prerelease"] = False
    elif change == "draft":
        release["draft"] = True
    elif change == "extra":
        extra = deepcopy(release["assets"][0])
        extra["name"] = "extra"
        release["assets"].append(extra)
    else:
        key = "id" if change == "asset-id" else change
        release["assets"][0][key] = "sha256:" + "b" * 64 if key == "digest" else 999
    with pytest.raises(ValueError, match="identity or bytes"):
        release_resume.require_snapshot(release, snapshot, TAG)


def test_unchanged_public_prerelease_is_accepted():
    release_resume.require_snapshot(*snapshot_fixture(), TAG)


def test_workflow_resume_still_runs_full_ci_and_public_acceptance():
    text = Path(".github/workflows/auto-release.yml").read_text()
    assert "resume_run_id:" in text
    assert "release_resume.validate_run" in text
    assert "python -m ci.release_resume" in text
    assert "artifact_run: ${{ steps.meta.outputs.artifact_run }}" in text
    assert "candidate_sha: ${{ needs.preflight.outputs.candidate_sha }}" in text
    assert "mode: candidate" in text
    assert "mode: released" in text
    assert "needs.candidate-acceptance-gate.result == 'success'" in text
    assert "needs.verify-prior-public.result == 'success'" in text


@pytest.mark.parametrize(
    "path",
    [
        "Cargo.toml",
        "Cargo.lock",
        "crates/clud-bin/build.rs",
        "src/clud/__init__.py",
        "pyproject.toml",
        "build.sh",
    ],
)
def test_resume_refuses_changes_to_production_inputs(path):
    with pytest.raises(ValueError, match="production inputs"):
        release_resume.require_recovery_changes(["ci/installer_public_distro_guest.sh", path])


def test_resume_allows_only_acceptance_harness_repairs():
    release_resume.require_recovery_changes(
        [
            "ci/installer_public_distro_guest.sh",
            ".github/workflows/auto-release.yml",
            "tests/test_release_resume.py",
            "docs/architecture/ci.md",
        ]
    )


@pytest.mark.parametrize("key", ["release_id", "tag", "assets"])
def test_fresh_proof_cannot_adopt_replaced_public_assets(key):
    _, original = snapshot_fixture()
    current = deepcopy(original)
    release_resume.require_same_proof(original, current)
    current[key] = "changed"
    with pytest.raises(ValueError, match="original asset identity"):
        release_resume.require_same_proof(original, current)


@pytest.fixture
def provenance_repo(tmp_path):
    env = os.environ.copy()
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull, GIT_TERMINAL_PROMPT="0")

    def git(*args):
        return RunningProcess.run(
            [
                "git",
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                *args,
            ],
            cwd=tmp_path,
            env=env,
            stdout=PIPE,
            stderr=PIPE,
            text=True,
            check=True,
        ).stdout.strip()

    git("init")
    (tmp_path / "ci").mkdir()
    (tmp_path / "ci/helper.py").write_text("original")
    (tmp_path / "app.rs").write_text("original")
    assets = tmp_path / "crates/clud-bin/assets"
    assets.mkdir(parents=True)
    for name in ("openrouter-catalog.json", "server-settings.json"):
        (assets / name).write_text("original model data")
    git("add", ".")
    git("commit", "-m", "source")
    source = git("rev-parse", "HEAD")
    (tmp_path / "ci/helper.py").write_text("repaired harness")
    git("add", ".")
    git("commit", "-m", "harness repair")
    harness = git("rev-parse", "HEAD")
    for name in ("openrouter-catalog.json", "server-settings.json"):
        (assets / name).write_text("independent model data refresh")
    git("add", ".")
    git("commit", "-m", "model data refresh")
    model_data = git("rev-parse", "HEAD")
    (tmp_path / "app.rs").write_text("production changed")
    git("add", ".")
    git("commit", "-m", "production change")
    production = git("rev-parse", "HEAD")
    git("checkout", "--detach", source)
    text = Path(".github/workflows/ci.yml").read_text()
    block = text.split("      - name: Verify source and workflow provenance", 1)[1].split(
        "      - name: Set up Python", 1
    )[0]
    script = textwrap.dedent(block.split("        run: |\n", 1)[1])
    env.update(
        SOURCE_REF=source,
        EVENT_SHA=harness,
        EVENT_NAME="workflow_dispatch",
        RECOVERY_RUN_ID="123",
        CALLER_WORKFLOW="Auto Release",
        CALLER_EVENT="workflow_dispatch",
        CALLER_REF="refs/heads/main",
        GITHUB_ENV=str(tmp_path / "github-env"),
        GITHUB_OUTPUT=str(tmp_path / "github-output"),
    )
    return tmp_path, env, script, {"production": production, "model-data": model_data}


@pytest.mark.skipif(os.name != "posix", reason="CI provenance executes on an Ubuntu runner")
@pytest.mark.parametrize(
    ("field", "value", "success"),
    [
        ("RECOVERY_RUN_ID", "123", True),
        ("RECOVERY_RUN_ID", "", False),
        ("RECOVERY_RUN_ID", "bad", False),
        ("CALLER_WORKFLOW", "CI", False),
        ("CALLER_EVENT", "pull_request", False),
        ("CALLER_REF", "refs/heads/feature", False),
        ("EVENT_SHA", "production", False),
        ("EVENT_SHA", "model-data", True),
    ],
)
def test_actual_ci_provenance_guard(provenance_repo, field, value, success):
    path, env, script, commits = provenance_repo
    env[field] = commits.get(value, value)
    result = RunningProcess.run(
        ["bash", "-c", script], cwd=path, env=env, stdout=PIPE, stderr=PIPE, text=True
    )
    assert (result.returncode == 0) is success, result.stdout + result.stderr


@pytest.mark.parametrize(
    "change", ["valid", "production", "rename", "truncated", "ancestry", "diverged"]
)
def test_github_harness_comparison_fails_closed(monkeypatch, change):
    comparison = {
        "merge_base_commit": {"sha": SHA},
        "status": "ahead",
        "files": [{"filename": "ci/helper.py"}],
    }
    if change == "production":
        comparison["files"] = [{"filename": "Cargo.toml"}]
    elif change == "rename":
        comparison["files"][0]["previous_filename"] = "Cargo.toml"
    elif change == "truncated":
        comparison["files"] *= 300
    elif change == "ancestry":
        comparison["merge_base_commit"]["sha"] = "b" * 40
    elif change == "diverged":
        comparison["status"] = "diverged"
    monkeypatch.setattr(release_resume, "api", lambda _path: comparison)
    if change == "valid":
        release_resume.validate_harness(SHA, "b" * 40)
    else:
        with pytest.raises(ValueError, match=r"harness|production"):
            release_resume.validate_harness(SHA, "b" * 40)


@pytest.mark.parametrize(
    "path",
    [
        "crates/clud-bin/assets/openrouter-catalog.json",
        "crates/clud-bin/assets/server-settings.json",
    ],
)
def test_original_release_can_recover_after_independent_model_data_refresh(path):
    release_resume.require_recovery_changes(["ci/release_resume.py", path])


@pytest.mark.parametrize(
    "path",
    [
        "crates/clud-bin/assets/openrouter-catalog.json.bak",
        "crates/clud-bin/assets/server-settings.json/escape",
        "crates/clud-bin/assets/other.json",
    ],
)
def test_model_data_exception_is_not_a_directory_or_prefix_escape(path):
    with pytest.raises(ValueError, match="production inputs"):
        release_resume.require_recovery_changes([path])
