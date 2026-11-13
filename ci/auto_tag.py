"""Reconcile an immutable release tag and tag-ref dispatch after main CI (#2764)."""

from __future__ import annotations

import base64
import json
import os
import re
from datetime import datetime, timezone
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

import tomllib

WORKFLOW = "auto-release.yml"
SHA = re.compile(r"[0-9a-f]{40}")
VERSION = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)")


def api(path: str, *, body: dict | None = None) -> dict:
    """Use this run's repository/token; unlike public_release.api, allow 204."""
    request = Request(
        f"https://api.github.com/repos/{os.environ['GITHUB_REPOSITORY']}/{path}",
        data=json.dumps(body).encode() if body is not None else None,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "X-GitHub-Api-Version": "2022-11-28",
            "Content-Type": "application/json",
            "User-Agent": "clud-auto-tag",
        },
    )
    with urlopen(request, timeout=60) as response:
        return json.loads(response.read() or "{}")


def optional(path: str) -> dict | None:
    try:
        return api(path)
    except HTTPError as error:
        if error.code != 404:
            raise
        return None


def document(path: str, sha: str) -> dict:
    data = api(f"contents/{path}?ref={sha}")
    if data.get("encoding") != "base64":
        raise ValueError(f"Cannot read {path} at {sha}")
    return tomllib.loads(base64.b64decode(data["content"]).decode())


def changed_version(before: dict, after: dict, project: dict) -> str | None:
    previous = before["workspace"]["package"]["version"]
    version = after["workspace"]["package"]["version"]
    if previous == version:
        return None
    if version != project["project"]["version"]:
        raise ValueError("Version mismatch: Cargo.toml workspace and pyproject.toml project")
    if not VERSION.fullmatch(version):
        raise ValueError(f"Automatic release requires a stable version: {version!r}")
    return version


def tag_target(tag: str) -> str | None:
    ref = optional(f"git/ref/tags/{tag}")
    if ref is None:
        return None
    obj = ref["object"]
    for _depth in range(5):
        if obj["type"] == "commit":
            return obj["sha"]
        if obj["type"] != "tag":
            break
        obj = api(f"git/tags/{obj['sha']}")["object"]
    raise ValueError(f"Tag {tag} does not resolve to a commit")


def existing_tag(version: str, sha: str) -> str | None:
    found = []
    for tag in (version, f"v{version}"):
        target = tag_target(tag)
        if target is not None:
            if target != sha:
                raise ValueError(f"Conflicting tag {tag}: {target}; validated main SHA is {sha}")
            found.append(tag)
    return found[0] if found else None


def ensure_tag(version: str, sha: str) -> str:
    tag = existing_tag(version, sha)
    if tag:
        print(f"Tag {tag} already exists at {sha}; skipping tag creation")
        return tag
    annotation = api(
        "git/tags",
        body={
            "tag": version,
            "message": f"Release {version}",
            "object": sha,
            "type": "commit",
            "tagger": {
                "name": "github-actions[bot]",
                "email": "41898282+github-actions[bot]@users.noreply.github.com",
                "date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            },
        },
    )
    try:
        api("git/refs", body={"ref": f"refs/tags/{version}", "sha": annotation["sha"]})
    except HTTPError as error:
        # Another creator may have won. Re-read both spellings; never update a ref.
        if error.code != 422 or not existing_tag(version, sha):
            raise
    tag = existing_tag(version, sha)
    if not tag:
        raise ValueError("Created tag was not visible; refusing an unbound dispatch")
    return tag


def release_run(tag: str, sha: str) -> dict | None:
    for page in range(1, 11):
        query = urlencode({"head_sha": sha, "per_page": 100, "page": page})
        rows = api(f"actions/workflows/{WORKFLOW}/runs?{query}")["workflow_runs"]
        for run in rows:
            if (
                run["head_sha"] == sha
                and run["head_branch"] == tag
                and run["event"] in ("push", "workflow_dispatch")
            ):
                return run
        if len(rows) < 100:
            return None
    raise ValueError("Release run lookup exceeded 1000 runs; refusing duplicate dispatch")


def handoff(version: str, sha: str) -> dict | None:
    existing_tag(version, sha)
    # Check both aliases before creating anything, including an orphaned release.
    for alias in (version, f"v{version}"):
        if optional(f"releases/tags/{alias}") is not None:
            print(f"Release {alias} already exists; leaving it immutable")
            return None
    tag = ensure_tag(version, sha)
    run = release_run(tag, sha)
    if run:
        print(f"Auto Release already handed off: {run['html_url']}")
        return run
    # A default-token tag push cannot trigger Auto Release. Dispatch against the
    # immutable tag ref, so both workflow provenance and checkout pin this SHA.
    run = api(
        f"actions/workflows/{WORKFLOW}/dispatches",
        body={"ref": f"refs/tags/{tag}", "inputs": {"tag": tag}, "return_run_details": True},
    )
    if not run.get("workflow_run_id"):
        raise ValueError("Dispatch did not return a run ID; reconcile before retrying")
    print(f"Auto Release dispatched: {run['html_url']} (tag {tag}, SHA {sha})")
    return run


def reconcile(event: dict, sha: str) -> dict | None:
    if (
        os.environ.get("GITHUB_EVENT_NAME") != "push"
        or os.environ.get("GITHUB_REF") != "refs/heads/main"
        or os.environ.get("CI_VERIFIED") != "true"
        or os.environ.get("CANDIDATE_SHA")
    ):
        raise ValueError("Auto-tag requires executed, successful post-merge main CI")
    before = event.get("before", "")
    if (
        not SHA.fullmatch(sha)
        or event.get("after") != sha
        or event.get("ref") != "refs/heads/main"
        or not SHA.fullmatch(before)
        or before == "0" * 40
        or event.get("forced")
        or event.get("deleted")
    ):
        raise ValueError("Auto-tag requires an existing, non-forced main push range")
    version = changed_version(
        document("Cargo.toml", before),
        document("Cargo.toml", sha),
        document("pyproject.toml", sha),
    )
    if version is None:
        print("Workspace version unchanged; no release handoff")
        return None
    return handoff(version, sha)


def main() -> None:
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    reconcile(event, os.environ["GITHUB_SHA"])


if __name__ == "__main__":
    main()
