"""The scheduled refresh bots must test what they are about to commit (#1635).

Both bots land their refresh on `main` through a bot PR with auto-merge
(`main` requires the merge queue, #2518). 60f6087b showed that a refresh can
land a commit that turns `main` red, so each workflow runs the producer's own
tests against the regenerated file between the refresh and the commit (DD-127).
"""

from __future__ import annotations

from pathlib import Path

import pytest

WORKFLOWS = Path(__file__).resolve().parents[1] / ".github" / "workflows"

# workflow file -> (producer module, producer test file)
BOTS = {
    "refresh-model-contexts.yml": (
        "ci.refresh_model_contexts",
        "tests/test_refresh_model_contexts.py",
    ),
    "refresh-openrouter-catalog.yml": (
        "ci.refresh_openrouter_catalog",
        "tests/test_refresh_openrouter_catalog.py",
    ),
}


def _steps(text: str) -> list[str]:
    """Split a single-job workflow into its step blocks, in order."""
    _, _, body = text.partition("    steps:\n")
    assert body, "workflow has no steps block"
    blocks: list[str] = []
    for line in body.splitlines():
        if line.startswith("      - "):
            blocks.append(line)
        elif blocks:
            blocks[-1] += "\n" + line
    return blocks


def _index(steps: list[str], needle: str) -> int:
    hits = [i for i, step in enumerate(steps) if needle in step]
    assert len(hits) == 1, f"expected exactly one step containing {needle!r}, found {len(hits)}"
    return hits[0]


@pytest.mark.parametrize("workflow", sorted(BOTS))
def test_bot_runs_producer_tests_between_refresh_and_commit(workflow: str) -> None:
    module, test_file = BOTS[workflow]
    steps = _steps((WORKFLOWS / workflow).read_text(encoding="utf-8"))
    refresh = _index(steps, f"python -m {module}")
    gate = _index(steps, "python -m pytest")
    commit = _index(steps, "git commit")
    assert refresh < gate < commit, "the test gate must run after the refresh and before the commit"
    assert test_file in steps[gate]
    # A failing gate must fail the job, so nothing may swallow its exit code.
    assert "continue-on-error" not in steps[gate]
    assert "|| true" not in steps[gate]


@pytest.mark.parametrize("workflow", sorted(BOTS))
def test_bot_never_syncs_the_project(workflow: str) -> None:
    # `uv run` would trigger a full maturin build (CLAUDE.md, ci.md).
    lines = (WORKFLOWS / workflow).read_text(encoding="utf-8").splitlines()
    code = [line for line in lines if not line.lstrip().startswith("#")]
    assert not any("uv run" in line for line in code)


@pytest.mark.parametrize("workflow", sorted(BOTS))
def test_bot_lands_through_a_merge_queue_pr(workflow: str) -> None:
    """A direct push to `main` is always rejected, and a `GITHUB_TOKEN` PR
    triggers no `pull_request` workflows, so the commit step must push a branch
    and open the PR with a bot token (#2518)."""
    steps = _steps((WORKFLOWS / workflow).read_text(encoding="utf-8"))
    commit = steps[_index(steps, "git commit")]
    assert "REFRESH_BOT_TOKEN" in commit, "the commit step needs a bot token"
    assert "gh pr merge" in commit and "--auto" in commit, "the PR must auto-enqueue"
    assert "HEAD:main" not in commit, "main requires the merge queue; never push to it"
