"""DD-223: no release workflow depends on an external package-manager channel.

Auto Release, and every local reusable workflow or composite action it reaches,
must not publish to or install clud through Homebrew, Scoop, winget,
Chocolatey, an apt PPA, the AUR, nixpkgs, npm or any other external registry.
Those channels run in their own workflows after the stable release exists
(Homebrew: `homebrew-tap.yml`), so a channel failure never turns a completed
release red and can be retried alone. PyPI and GitHub Releases are the
release's primary artifacts, not downstream channels, and are not listed here.

Runner tooling installed with a system package manager (`apt-get install fish`,
`brew install fish`, distro test images, the Nix VM harness) does not publish
clud and is not matched: the patterns name distribution channels only.
"""

import re
from pathlib import Path

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github/workflows"
RELEASE_ENTRYPOINTS = ("auto-release.yml",)

# Channel name -> pattern matched (case-insensitively) against every string a
# job carries: names, `uses:`, `with:` values and `run:` scripts. YAML
# comments are not part of the parsed job, so prose about a channel is fine.
EXTERNAL_CHANNELS = {
    "homebrew": r"homebrew-tap|homebrew-core|homebrew_formula|Formula/clud\.rb"
    r"|\bbrew\s+(?:tap|bump\S*|audit|test-bot|pr-\S+|install\s+\S*clud)",
    "scoop": r"\bscoop\b",
    "winget": r"\bwinget|winget-pkgs",
    "chocolatey": r"\bchoco(?:latey)?\b",
    "apt-ppa": r"\bdput\b|launchpad\.net|\bppa:",
    "aur": r"aur\.archlinux\.org|\baur\b|\bmakepkg\b",
    "nixpkgs": r"NixOS/nixpkgs/pull|nixpkgs-review|nix-community|\bcachix\s+push\b",
    "npm": r"\bnpm\s+publish\b|registry\.npmjs\.org",
    "crates-io": r"\bcargo\s+publish\b",
    "snap-flatpak": r"snapcraft|flathub|\bflatpak\b",
    "conda": r"conda-forge|anaconda\s+upload",
    "container-registry": r"\bdocker\s+push\b|docker/build-push-action",
}

# The only way a channel may appear in a release workflow: a package manager
# the organization operates itself, admitted by an explicit policy decision.
# Key: (workflow file, job id, channel). Value: the reason, citing the DD that
# admits it. External package managers are never eligible. Empty today.
OWNED_CHANNEL_ALLOWLIST: dict[tuple[str, str, str], str] = {}


def load(path):
    return yaml.safe_load(path.read_text())


def strings(node):
    if isinstance(node, str):
        yield node
    elif isinstance(node, dict):
        for key, value in node.items():
            yield str(key)
            yield from strings(value)
    elif isinstance(node, list):
        for item in node:
            yield from strings(item)


def local_uses(node):
    for text in strings(node):
        if text.startswith("./.github/"):
            yield text.removeprefix("./")


def release_files():
    """Auto Release plus every local workflow and composite action it reaches."""
    seen, queue = set(), [f".github/workflows/{name}" for name in RELEASE_ENTRYPOINTS]
    while queue:
        rel = queue.pop()
        if rel in seen:
            continue
        seen.add(rel)
        path = ROOT / rel
        if path.is_dir():
            path = path / "action.yml"
        queue.extend(local_uses(load(path)))
    return seen


def units(rel):
    """(unit id, parsed body) for each job of a workflow, or the whole action."""
    path = ROOT / rel
    if path.is_dir():
        return [("action", load(path / "action.yml"))]
    return list(load(path)["jobs"].items())


def channel_hits(node):
    text = "\n".join(strings(node))
    return {
        name
        for name, pattern in EXTERNAL_CHANNELS.items()
        if re.search(pattern, text, re.IGNORECASE)
    }


def test_release_closure_covers_the_called_workflows():
    files = release_files()
    for rel in (
        ".github/workflows/auto-release.yml",
        ".github/workflows/ci.yml",
        ".github/workflows/_build-target.yml",
        ".github/workflows/_run-tests.yml",
        ".github/workflows/installer-check.yml",
    ):
        assert rel in files
    assert ".github/workflows/homebrew-tap.yml" not in files


def test_no_release_job_references_an_external_package_channel():
    found = []
    for rel in sorted(release_files()):
        for job_id, body in units(rel):
            for channel in sorted(channel_hits(body)):
                if (Path(rel).name, job_id, channel) not in OWNED_CHANNEL_ALLOWLIST:
                    found.append(f"{rel}:{job_id} -> {channel}")
    assert not found, (
        "release workflows must not depend on an external package manager (DD-223); "
        "move the channel to its own workflow triggered by the stable release:\n" + "\n".join(found)
    )


def test_allowlist_entries_are_explicit_and_still_needed():
    for (workflow, job_id, channel), reason in OWNED_CHANNEL_ALLOWLIST.items():
        assert channel in EXTERNAL_CHANNELS
        assert re.search(r"\bDD-\d+\b", reason), "an owned-channel exception must cite its decision"
        rel = next(r for r in release_files() if Path(r).name == workflow)
        assert channel in channel_hits(dict(units(rel))[job_id]), "stale allowlist entry"


@pytest.mark.parametrize(
    "script",
    [
        'git clone "https://x-access-token:${T}@github.com/talos-hq/homebrew-tap.git" tap',
        "python -m installer.homebrew_formula 2.8.68 SHA256SUMS > clud.rb",
        "brew bump-formula-pr --url x clud",
        "wingetcreate update clud",
        "scoop install clud",
        "choco push clud.nupkg",
        "npm publish --access public",
        "soldr cargo publish -p clud",
    ],
)
def test_detector_catches_a_publication_step(script):
    assert channel_hits({"steps": [{"run": script}]})


@pytest.mark.parametrize(
    "script",
    [
        "brew install fish",
        "sudo apt-get install -y fish zsh",
        "apk add curl",
        "npm ls -g --depth=0",
    ],
)
def test_detector_ignores_runner_tooling(script):
    assert not channel_hits({"steps": [{"run": script}]})


def test_auto_release_has_no_homebrew_job():
    release = load(WORKFLOWS / "auto-release.yml")
    assert "publish-homebrew" not in release["jobs"]
    for job_id, body in release["jobs"].items():
        needs = body.get("needs", [])
        needs = [needs] if isinstance(needs, str) else needs
        assert not any("homebrew" in need for need in needs), job_id


def test_homebrew_tap_triggers_only_after_a_stable_release_or_by_dispatch():
    tap = load(WORKFLOWS / "homebrew-tap.yml")
    triggers = tap[True]  # PyYAML reads the bare `on` key as boolean true.
    assert set(triggers) == {"workflow_run", "release", "workflow_dispatch"}
    # Auto Release promotes with GITHUB_TOKEN, which starts no `release` run,
    # so completion of the release run is the automatic trigger.
    assert triggers["workflow_run"] == {"workflows": ["Auto Release"], "types": ["completed"]}
    # `released` fires only for stable releases, never a prerelease.
    assert triggers["release"] == {"types": ["released"]}
    assert triggers["workflow_dispatch"]["inputs"]["tag"]["required"] is True
    assert "workflow_call" not in triggers
    assert tap["permissions"] == {"contents": "read"}


def tap_step(step_id):
    steps = load(WORKFLOWS / "homebrew-tap.yml")["jobs"]["update-tap"]["steps"]
    return next(s for s in steps if s.get("id") == step_id)


def test_homebrew_tap_publishes_only_a_stable_release():
    resolve = tap_step("tag")["run"]
    assert 'releases/tags/$tag"' in resolve
    assert '"false false"' in resolve, "drafts and prereleases must be refused"


def test_homebrew_tap_workflow_run_publishes_the_runs_own_tag():
    # Auto Release 44677702052 (2.8.69) has head_branch "2.8.69", but the tap
    # run it started read releases/latest a few minutes after promotion, got
    # 2.8.68 and pushed nothing. The tag comes from the run, never from latest.
    step = tap_step("tag")
    assert step["env"]["RUN_TAG"] == "${{ github.event.workflow_run.head_branch }}"
    assert step["env"]["RUN_CONCLUSION"] == "${{ github.event.workflow_run.conclusion }}"
    assert "releases/latest" not in step["run"]
    assert 'tag="$RUN_TAG"' in step["run"]
    assert '[ "$RUN_CONCLUSION" = success ] || skip' in step["run"], (
        "a failed or rolled-back release run must leave the tap alone"
    )
    assert 'tag="$EVENT_TAG"' in step["run"]
    assert 'tag="$DISPATCH_TAG"' in step["run"]


def formula(tag):
    return f'url "https://github.com/talos-hq/clud/releases/download/{tag}/clud-{tag}-x"\n'


def run_compare(tmp_path, monkeypatch, tag, published, event="workflow_run"):
    script = tap_step("diff")["run"]
    body = script.split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
    (tmp_path / "clud.rb").write_text(formula(tag))
    if published is not None:
        (tmp_path / "tap" / "Formula").mkdir(parents=True)
        (tmp_path / "tap" / "Formula" / "clud.rb").write_text(published)
    out = tmp_path / "out"
    out.write_text("")
    for key, value in {
        "RUNNER_TEMP": str(tmp_path),
        "RELEASE_TAG": tag,
        "EVENT": event,
        "GITHUB_OUTPUT": str(out),
    }.items():
        monkeypatch.setenv(key, value)
    exec(compile(body, "homebrew-tap compare", "exec"), {})
    return out.read_text()


@pytest.mark.parametrize(
    ("tag", "published", "changed"),
    [
        ("2.8.69", formula("2.8.68"), "true"),
        ("2.8.69", formula("2.8.69"), "false"),
        ("2.8.69", None, "true"),
        ("2.10.0", formula("2.9.9"), "true"),
        ("2.8.68", formula("2.8.69"), "false"),
        ("2.9.9", formula("2.10.0"), "false"),
    ],
)
def test_homebrew_tap_never_downgrades_the_formula(tmp_path, monkeypatch, tag, published, changed):
    assert run_compare(tmp_path, monkeypatch, tag, published) == f"changed={changed}\n"


def test_homebrew_tap_dispatched_downgrade_fails(tmp_path, monkeypatch):
    refused = r"refusing to downgrade the tap from 2\.8\.69 to 2\.8\.68"
    with pytest.raises(SystemExit, match=refused):
        run_compare(tmp_path, monkeypatch, "2.8.68", formula("2.8.69"), event="workflow_dispatch")


def test_homebrew_tap_renders_from_the_tags_own_sha256sums_and_generator():
    steps = load(WORKFLOWS / "homebrew-tap.yml")["jobs"]["update-tap"]["steps"]
    checkout = next(s for s in steps if s.get("uses") == "actions/checkout@v4")
    assert checkout["with"]["ref"] == "refs/tags/${{ steps.tag.outputs.tag }}"
    render = next(s for s in steps if s.get("name", "").startswith("Render Formula/clud.rb"))
    assert render["env"]["RELEASE_TAG"] == "${{ steps.tag.outputs.tag }}"
    assert 'gh release download "$RELEASE_TAG"' in render["run"]
    assert "--pattern SHA256SUMS" in render["run"]
    sums = '"$RUNNER_TEMP/release-sums/SHA256SUMS"'
    assert f'python -m installer.homebrew_formula "$RELEASE_TAG" {sums}' in render["run"]


def test_homebrew_tap_noop_needs_no_credential_and_missing_app_is_named():
    steps = load(WORKFLOWS / "homebrew-tap.yml")["jobs"]["update-tap"]["steps"]
    token = tap_step("tap-app")
    assert token["if"] == "steps.diff.outputs.changed == 'true'"
    assert token["continue-on-error"] is True
    explain = next(s for s in steps if s.get("name") == "Explain a missing tap credential")
    assert "Install the clud-refresh GitHub App on talos-hq/homebrew-tap" in explain["run"]
    assert "exit 1" in explain["run"]
