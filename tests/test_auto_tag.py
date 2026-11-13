"""#2764: a verified landed version bump hands off to the existing release."""

import base64
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlsplit

import pytest
import yaml

from ci import auto_tag

ROOT = Path(__file__).resolve().parents[1]
BEFORE = "a" * 40
LANDED = "b" * 40
ADVANCED = "c" * 40
VERSION = "2.8.66"


def workflow(name):
    return yaml.safe_load((ROOT / ".github/workflows" / name).read_text())


def test_2764_post_merge_ci_hands_off_to_release():
    jobs = workflow("ci.yml")["jobs"]
    tag = jobs["auto-tag"]
    assert tag["needs"] == ["static", "ci-ok"]
    assert "github.event_name == 'push'" in tag["if"]
    assert "github.ref == 'refs/heads/main'" in tag["if"]
    assert "!inputs.candidate_sha" in tag["if"]
    assert "needs.ci-ok.outputs.verified == 'true'" in tag["if"]
    assert tag["permissions"] == {"contents": "write", "actions": "write"}
    assert "steps.verify.outputs.verified" in jobs["ci-ok"]["outputs"]["verified"]
    verify = next(s for s in jobs["ci-ok"]["steps"] if s.get("id") == "verify")
    assert "steps.superseded.outputs.superseded != 'true'" in verify["if"]
    assert verify["run"].index("ci-lint gate") < verify["run"].index("verified=true")
    steps = tag["steps"]
    assert any("python -m ci.auto_tag" in s.get("run", "") for s in steps)


def test_release_caller_covers_even_skipped_nested_job_permissions():
    ci = workflow("ci.yml")
    release = workflow("auto-release.yml")
    envelope = release["jobs"]["full-ci-gate"]["permissions"]
    rank = {"none": 0, "read": 1, "write": 2}
    for job in ci["jobs"].values():
        for scope, permission in job.get("permissions", ci["permissions"]).items():
            assert rank[permission] <= rank[envelope.get(scope, "none")]
    assert ci["permissions"]["contents"] == "read"
    assert [
        name
        for name, job in ci["jobs"].items()
        if job.get("permissions", {}).get("contents") == "write"
    ] == ["auto-tag"]


def test_bursts_retain_main_runs_and_retry_without_blind_post_retries():
    ci = workflow("ci.yml")
    concurrency = ci["concurrency"]
    assert "github.event_name == 'push'" in concurrency["queue"]
    assert "github.ref == 'refs/heads/main'" in concurrency["queue"]
    assert "'max' || 'single'" in concurrency["queue"]
    assert "github.ref != 'refs/heads/main'" in concurrency["cancel-in-progress"]
    job = ci["jobs"]["auto-tag"]
    assert job["concurrency"] == {
        "group": "clud-auto-tag",
        "queue": "max",
        "cancel-in-progress": False,
    }
    first = next(s for s in job["steps"] if s.get("id") == "handoff")
    recovery = job["steps"][-1]
    assert first["continue-on-error"] is True
    assert recovery["if"] == "steps.handoff.outcome == 'failure'"
    assert recovery["run"] == first["run"]
    preflight = workflow("auto-release.yml")["jobs"]["preflight"]
    assert preflight["steps"][0]["with"]["ref"] == "${{ github.sha }}"


def cargo(version):
    return {"workspace": {"package": {"version": version}}}


class GitHub:
    """Remote fixture: main advances, immutable refs and dispatch state survive retries."""

    def __init__(self):
        self.tags = {}
        self.annotations = {}
        self.releases = set()
        self.runs = []
        self.writes = []
        self.fail_dispatch = False
        self.lose_dispatch_response = False
        self.race = None
        self.project_version = VERSION
        self.docs = {
            ("Cargo.toml", BEFORE): '[workspace.package]\nversion = "2.8.65"',
            ("Cargo.toml", LANDED): f'[workspace.package]\nversion = "{VERSION}"',
        }

    def __call__(self, path, *, body=None):
        if body is not None:
            self.writes.append((path, body))
            return self.write(path, body)
        return self.read(path)

    def read(self, path):
        if path.startswith("contents/"):
            parsed = urlsplit(path)
            name, sha = parsed.path.removeprefix("contents/"), parse_qs(parsed.query)["ref"][0]
            text = (
                f'[project]\nversion = "{self.project_version}"'
                if name == "pyproject.toml"
                else self.docs[name, sha]
            )
            return {"encoding": "base64", "content": base64.b64encode(text.encode()).decode()}
        if path.startswith("git/ref/tags/"):
            tag = path.removeprefix("git/ref/tags/")
            if tag in self.tags:
                return {"object": self.tags[tag]}
        elif path.startswith("git/tags/"):
            return self.annotations[path.removeprefix("git/tags/")]
        elif path.startswith("releases/tags/"):
            if path.removeprefix("releases/tags/") in self.releases:
                return {"published_at": "2026-11-01T00:00:00Z"}
        elif "/runs?" in path:
            return {"workflow_runs": self.runs}
        raise HTTPError(path, 404, "Not Found", {}, None)

    def write(self, path, body):
        if path == "git/tags":
            key = str(len(self.annotations))
            self.annotations[key] = {"object": {"type": "commit", "sha": body["object"]}}
            return {"sha": key}
        if path == "git/refs":
            tag = body["ref"].removeprefix("refs/tags/")
            if self.race:
                self.tags[tag] = {"type": "commit", "sha": self.race}
                raise HTTPError(path, 422, "Reference already exists", {}, None)
            assert tag not in self.tags, "Never overwrite an existing ref"
            self.tags[tag] = {"type": "tag", "sha": body["sha"]}
            return {}
        assert path == "actions/workflows/auto-release.yml/dispatches"
        if self.fail_dispatch:
            self.fail_dispatch = False
            raise HTTPError(path, 503, "Unavailable", {}, None)
        tag = body["inputs"]["tag"]
        assert body["ref"] == f"refs/tags/{tag}"
        target = self.tags[tag]
        if target["type"] == "tag":
            target = self.annotations[target["sha"]]["object"]
        run = {
            "head_sha": target["sha"],
            "head_branch": tag,
            "event": "workflow_dispatch",
            "html_url": "https://example.test/runs/1",
            "workflow_run_id": 1,
        }
        self.runs.append(run)
        if self.lose_dispatch_response:
            self.lose_dispatch_response = False
            raise TimeoutError("Dispatch accepted, response lost")
        return run


@pytest.fixture
def remote(monkeypatch):
    remote = GitHub()
    monkeypatch.setattr(auto_tag, "api", remote)
    monkeypatch.setenv("GITHUB_EVENT_NAME", "push")
    monkeypatch.setenv("GITHUB_REF", "refs/heads/main")
    monkeypatch.setenv("CI_VERIFIED", "true")
    monkeypatch.delenv("CANDIDATE_SHA", raising=False)
    return remote


def push_event(**changes):
    return {"ref": "refs/heads/main", "before": BEFORE, "after": LANDED, **changes}


def test_bump_creates_one_annotated_tag_and_dispatches_landed_sha(remote):
    # The API fixture has no moving-main lookup. It could advance to ADVANCED
    # at any time: all contents, annotation and dispatch use the landed SHA.
    auto_tag.reconcile(push_event(), LANDED)
    auto_tag.reconcile(push_event(), LANDED)
    assert list(remote.tags) == [VERSION]
    assert len(remote.annotations) == len(remote.runs) == 1
    assert remote.runs[0]["head_sha"] == LANDED
    assert remote.writes[0][1]["message"] == f"Release {VERSION}"
    assert remote.writes[0][1]["type"] == "commit"
    assert remote.writes[-1][1]["return_run_details"] is True


@pytest.mark.parametrize("kind", ["merge_commit", "multi_commit_push"])
def test_compare_entire_push_range_including_merge_parent(remote, kind):
    # The immediate parent/intermediate commit already carries the new version;
    # event.before still carries the old version. Only after has exact-SHA CI.
    remote.docs["Cargo.toml", ADVANCED] = remote.docs["Cargo.toml", LANDED]
    event = push_event(commits=[{"id": ADVANCED}, {"id": LANDED}], kind=kind)
    auto_tag.reconcile(event, LANDED)
    assert len(remote.runs) == 1
    assert remote.runs[0]["head_sha"] == LANDED


def test_unrelated_cargo_edit_does_not_release(remote):
    remote.docs["Cargo.toml", BEFORE] = remote.docs["Cargo.toml", LANDED] + "\nedition='2021'"
    auto_tag.reconcile(push_event(), LANDED)
    assert not remote.writes


@pytest.mark.parametrize(
    ("name", "value"),
    [
        ("GITHUB_EVENT_NAME", "pull_request"),
        ("GITHUB_EVENT_NAME", "merge_group"),
        ("GITHUB_EVENT_NAME", "schedule"),
        ("GITHUB_EVENT_NAME", "workflow_dispatch"),
        ("GITHUB_REF", "refs/heads/feature"),
        ("CANDIDATE_SHA", LANDED),
        ("CI_VERIFIED", ""),
        ("CI_VERIFIED", "false"),
        ("CI_VERIFIED", "failure"),
        ("CI_VERIFIED", "cancelled"),
        ("CI_VERIFIED", "skipped"),
    ],
)
def test_non_post_merge_or_unverified_ci_cannot_mutate(remote, monkeypatch, name, value):
    monkeypatch.setenv(name, value)
    with pytest.raises(ValueError, match="successful post-merge main CI"):
        auto_tag.reconcile(push_event(), LANDED)
    assert not remote.writes


@pytest.mark.parametrize(
    "changes",
    [
        {"before": "0" * 40},
        {"before": ""},
        {"forced": True},
        {"deleted": True},
        {"after": ADVANCED},
        {"ref": "refs/heads/feature"},
    ],
)
def test_invalid_push_range_cannot_mutate(remote, changes):
    with pytest.raises(ValueError, match="push range"):
        auto_tag.reconcile(push_event(**changes), LANDED)
    assert not remote.writes


def test_metadata_mismatch_fails_before_tagging(remote):
    remote.project_version = "2.8.67"
    with pytest.raises(ValueError, match="Version mismatch"):
        auto_tag.reconcile(push_event(), LANDED)
    assert not remote.writes


@pytest.mark.parametrize("version", ["v2.8.66", "2.8.66-rc1", "2.8.66/path"])
def test_unsafe_or_unsupported_versions_cannot_mutate(remote, version):
    with pytest.raises(ValueError, match="stable version"):
        auto_tag.changed_version(cargo("2.8.65"), cargo(version), {"project": {"version": version}})
    assert not remote.writes


@pytest.mark.parametrize("tag", [VERSION, f"v{VERSION}"])
def test_existing_tag_skips_creation_but_recovers_missing_handoff(remote, tag):
    remote.tags[tag] = {"type": "commit", "sha": LANDED}
    auto_tag.reconcile(push_event(), LANDED)
    assert len(remote.runs) == 1
    assert remote.runs[0]["head_branch"] == tag
    assert len(remote.writes) == 1


@pytest.mark.parametrize("tag", [VERSION, f"v{VERSION}"])
def test_conflicting_tag_is_never_moved(remote, tag):
    remote.tags[tag] = {"type": "commit", "sha": ADVANCED}
    with pytest.raises(ValueError, match=f"Conflicting tag {tag}"):
        auto_tag.reconcile(push_event(), LANDED)
    assert not remote.writes


@pytest.mark.parametrize("tag", [VERSION, f"v{VERSION}"])
def test_existing_release_is_immutable_even_if_tag_missing(remote, tag):
    remote.releases.add(tag)
    auto_tag.reconcile(push_event(), LANDED)
    assert not remote.writes


@pytest.mark.parametrize("failure", ["fail_dispatch", "lose_dispatch_response"])
def test_failed_dispatch_recovers_without_second_tag_or_run(remote, failure):
    setattr(remote, failure, True)
    with pytest.raises((HTTPError, TimeoutError)):
        auto_tag.reconcile(push_event(), LANDED)
    auto_tag.reconcile(push_event(), LANDED)
    assert len(remote.tags) == len(remote.annotations) == len(remote.runs) == 1


@pytest.mark.parametrize("winner", [LANDED, ADVANCED])
def test_tag_creation_race_reconciles_without_force(remote, winner):
    remote.race = winner
    if winner == LANDED:
        auto_tag.reconcile(push_event(), LANDED)
        assert len(remote.runs) == 1
    else:
        with pytest.raises(ValueError, match="Conflicting tag"):
            auto_tag.reconcile(push_event(), LANDED)
        assert not remote.runs


def test_authorization_errors_are_not_treated_as_absence(remote, monkeypatch):
    def forbidden(path, **_kwargs):
        raise HTTPError(path, 403, "Forbidden", {}, None)

    monkeypatch.setattr(auto_tag, "api", forbidden)
    with pytest.raises(HTTPError):
        auto_tag.handoff(VERSION, LANDED)
    assert not remote.writes
