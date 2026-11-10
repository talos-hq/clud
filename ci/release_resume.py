"""Resume acceptance of immutable public prerelease bytes after a harness repair."""

from __future__ import annotations

import argparse
import os
from pathlib import Path

from ci.public_release import api, asset_snapshot, load, release_by_tag, require_prior_public

REQUIRED = {
    "Exact SHA full CI / CI OK": 1,
    "Build wheels (": 6,
    "Build static musl clud (": 2,
    "Install-smoke release wheels (": 4,
    "Verify Authenticode (": 2,
    "Execute release static musl ARM64": 1,
    "Attach versioned public candidate catalog": 1,
    "Verify anonymous final bytes and prerelease asset identity": 1,
}


def require_run(run: dict, jobs: list[dict], tag: str, sha: str) -> None:
    """Require the original tag build and every prepublication proof."""
    if (
        run.get("path") != ".github/workflows/auto-release.yml"
        or run.get("event") != "push"
        or run.get("head_branch") != tag
        or run.get("head_sha") != sha
        or run.get("status") != "completed"
        or run.get("conclusion") != "failure"
    ):
        raise ValueError("resume requires a completed failed release run for the unchanged tag")
    for prefix, count in REQUIRED.items():
        rows = [job for job in jobs if job.get("name", "").startswith(prefix)]
        if len(rows) != count or any(job.get("conclusion") != "success" for job in rows):
            raise ValueError(f"original release proof is incomplete: {prefix}")


def validate_run(run_id: str, tag: str, sha: str) -> None:
    if (
        not run_id.isdecimal()
        or int(run_id) <= 0
        or os.environ.get("GITHUB_EVENT_NAME") != "workflow_dispatch"
        or os.environ.get("GITHUB_REF") != "refs/heads/main"
    ):
        raise ValueError("resume must be dispatched from main with a positive original run ID")
    run = api(f"actions/runs/{run_id}")
    jobs = []
    for page in range(1, 11):
        rows = api(f"actions/runs/{run_id}/jobs?filter=latest&per_page=100&page={page}")["jobs"]
        jobs.extend(rows)
        if len(rows) < 100:
            break
    else:
        raise ValueError("release job list exceeded the bounded proof limit")
    require_run(run, jobs, tag, sha)


def require_snapshot(release: dict, snapshot: dict, tag: str) -> None:
    if (
        release.get("tag_name") != tag
        or snapshot.get("tag") != tag
        or release.get("id") != snapshot.get("release_id")
        or release.get("draft")
        or release.get("prerelease") is not True
        or asset_snapshot(release) != snapshot.get("assets")
    ):
        raise ValueError("public prerelease identity or bytes changed since the original proof")


def require_harness_only(paths: list[str]) -> None:
    allowed = ("ci/", ".github/workflows/", ".github/actions/", "tests/", "docs/")
    if any(not path.startswith(allowed) for path in paths):
        raise ValueError("production inputs changed since the immutable release tag")


def validate_harness(source_sha: str, harness_sha: str) -> None:
    comparison = api(f"compare/{source_sha}...{harness_sha}")
    files = comparison.get("files")
    if (
        comparison.get("merge_base_commit", {}).get("sha") != source_sha
        or comparison.get("status") not in ("ahead", "identical")
        or not isinstance(files, list)
        or len(files) >= 300
    ):
        raise ValueError("harness ancestry or complete changed-file proof is unavailable")
    paths = []
    for row in files:
        paths.append(row["filename"])
        if old := row.get("previous_filename"):
            paths.append(old)
    require_harness_only(paths)


def require_same_proof(original: dict, current: dict) -> None:
    if any(original.get(key) != current.get(key) for key in ("release_id", "tag", "assets")):
        raise ValueError("fresh anonymous proof differs from the original asset identity")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("snapshot", type=Path)
    parser.add_argument("--current-snapshot", type=Path)
    args = parser.parse_args()
    snapshot = load(args.snapshot)
    if args.current_snapshot:
        require_same_proof(snapshot, load(args.current_snapshot))
    require_snapshot(release_by_tag(args.tag), snapshot, args.tag)
    require_prior_public(snapshot)


if __name__ == "__main__":
    main()
