"""Merge-queue forwarding of prooft attestations (DD-227).

Each case builds a real repository with clud's attestation policy, a PR head
stamped by prooft, and a squash-style queue commit, then runs the judge with
a fake GitHub GraphQL answer. Anything but a first-entry, same-tree,
allowlisted-signer, attested head must leave every job running hosted.
"""

from __future__ import annotations

import shutil
import sys
from pathlib import Path
from typing import Any

import pytest
from prooft import Repo

from ci import attest, attest_queue
from tests import process

pytestmark = pytest.mark.skipif(shutil.which("git") is None, reason="git not installed")

POLICY_TEXT = attest.POLICY.read_text(encoding="utf-8")
CLIPPY = "skip_lint-linux-x64"
PASS = [sys.executable, "-c", "pass"]
REPOSITORY = "talos-hq/clud"
NUMBER = 4242
SIGNERS = attest_queue.QueuePolicy("on", frozenset({"maintainer"}))


def _git(repo: Path, *args: str) -> str:
    return process.run(
        ["git", *args], cwd=repo, capture_output=True, text=True, check=True
    ).stdout.strip()


def _commit(repo: Path, path: str, text: str, message: str) -> str:
    target = repo / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")
    _git(repo, "add", path)
    _git(repo, "commit", "-q", "-m", message)
    return _git(repo, "rev-parse", "HEAD")


def _squash(repo: Path, tree_of: str, parent: str) -> str:
    """What the queue builds: one commit with `tree_of`'s tree on `parent`."""
    return _git(repo, "commit-tree", f"{tree_of}^{{tree}}", "-p", parent, "-m", "queue")


@pytest.fixture
def repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    config = tmp_path / "gitconfig"
    config.write_text(
        "[user]\n\tname = clud-test\n\temail = clud-test@example.invalid\n"
        "[commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("GIT_CONFIG_GLOBAL", str(config))
    monkeypatch.setenv("GIT_CONFIG_NOSYSTEM", "1")
    monkeypatch.setenv("GIT_CEILING_DIRECTORIES", str(tmp_path))
    root = tmp_path / "repo"
    root.mkdir()
    _git(root, "init", "-q")
    _commit(root, "ci-attestations.yml", POLICY_TEXT, "base: policy")
    _git(root, "branch", "base")
    _commit(root, "src/lib.rs", "pub fn f() {}\n", "feature")
    Repo(root).lane("clippy").run(PASS)
    return root


def _graphql(
    head: str, *, login: str = "maintainer", valid: bool = True, fork: bool = False
) -> attest_queue.GraphQL:
    def answer(_query: str, variables: Any) -> dict[str, Any]:
        assert variables["number"] == NUMBER
        return {
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": head,
                        "headRepository": {"nameWithOwner": "someone/clud" if fork else REPOSITORY},
                        "baseRepository": {"nameWithOwner": REPOSITORY},
                    },
                    "object": {
                        "signature": {
                            "isValid": valid,
                            "state": "VALID" if valid else "UNSIGNED",
                            "signer": {"login": login},
                        }
                    },
                }
            }
        }

    return answer


def _judge(
    root: Path,
    *,
    queue_head: str,
    policy: attest_queue.QueuePolicy = SIGNERS,
    graphql: attest_queue.GraphQL | None = None,
    base: str = "base",
) -> attest_queue.Decision:
    head = _git(root, "rev-parse", "HEAD")
    base_sha = _git(root, "rev-parse", base)
    return attest_queue.judge(
        attest_queue.open_repo(str(root)),
        policy,
        base=base_sha,
        queue_head=queue_head,
        queue_ref=f"refs/heads/gh-readonly-queue/main/pr-{NUMBER}-{base_sha}",
        pr_head=head,
        repository=REPOSITORY,
        graphql=graphql or _graphql(head),
    )


def test_first_entry_on_the_attested_tree_reduces_the_proven_job(repo: Path) -> None:
    queued = _squash(repo, "HEAD", _git(repo, "rev-parse", "base"))
    decision = _judge(repo, queue_head=queued)
    outputs = decision.outputs()
    assert outputs[CLIPPY] == "true", decision.reason
    # Only the lane that ran is credited; the Dylint legs still run hosted.
    assert {k for k, v in outputs.items() if v == "true"} == {CLIPPY}


def test_a_tree_mismatch_runs_hosted(repo: Path) -> None:
    """main moved under the PR: the squash tree is not the attested tree."""
    _git(repo, "checkout", "-q", "base")
    moved = _commit(repo, "README.md", "main moved\n", "unrelated main commit")
    _git(repo, "checkout", "-q", "-")
    merged = _git(repo, "merge-tree", "--write-tree", moved, "HEAD")
    queued = _git(repo, "commit-tree", merged, "-p", moved, "-m", "queue")
    decision = _judge(repo, queue_head=queued, base=moved)
    assert decision.outputs()[CLIPPY] == "false"
    assert "tree differs" in decision.reason


def test_an_untrusted_signer_runs_hosted(repo: Path) -> None:
    queued = _squash(repo, "HEAD", _git(repo, "rev-parse", "base"))
    head = _git(repo, "rev-parse", "HEAD")
    decision = _judge(repo, queue_head=queued, graphql=_graphql(head, login="stranger"))
    assert decision.outputs()[CLIPPY] == "false"
    assert "not in the base's" in decision.reason


def test_an_unsigned_head_runs_hosted(repo: Path) -> None:
    queued = _squash(repo, "HEAD", _git(repo, "rev-parse", "base"))
    head = _git(repo, "rev-parse", "HEAD")
    decision = _judge(repo, queue_head=queued, graphql=_graphql(head, valid=False))
    assert decision.outputs()[CLIPPY] == "false"
    assert "no GitHub-verified signature" in decision.reason


def test_a_fork_or_a_moved_head_runs_hosted(repo: Path) -> None:
    queued = _squash(repo, "HEAD", _git(repo, "rev-parse", "base"))
    head = _git(repo, "rev-parse", "HEAD")
    fork = _judge(repo, queue_head=queued, graphql=_graphql(head, fork=True))
    moved = _judge(repo, queue_head=queued, graphql=_graphql("0" * 40))
    assert fork.outputs()[CLIPPY] == "false"
    assert moved.outputs()[CLIPPY] == "false"


def test_a_batched_entry_runs_hosted(repo: Path) -> None:
    """A later group entry sits on an earlier one, never directly on the base."""
    base = _git(repo, "rev-parse", "base")
    earlier = _git(repo, "commit-tree", f"{base}^{{tree}}", "-p", base, "-m", "entry 1")
    queued = _squash(repo, "HEAD", earlier)
    decision = _judge(repo, queue_head=queued)
    assert decision.outputs()[CLIPPY] == "false"
    assert "batched" in decision.reason


def test_shadow_reports_the_decision_but_reduces_nothing(repo: Path) -> None:
    queued = _squash(repo, "HEAD", _git(repo, "rev-parse", "base"))
    shadow = attest_queue.QueuePolicy("shadow", SIGNERS.signers)
    decision = _judge(repo, queue_head=queued, policy=shadow)
    assert "proves ci.yml:lint-linux-x64" in decision.reason
    assert set(decision.outputs().values()) == {"false"}


def test_a_head_that_edits_the_queue_switch_is_untrusted(repo: Path) -> None:
    _commit(repo, attest_queue.QUEUE_POLICY, 'forward = "on"\n', "turn it on")
    Repo(repo).lane("clippy").run(PASS)
    queued = _squash(repo, "HEAD", _git(repo, "rev-parse", "base"))
    decision = _judge(repo, queue_head=queued)
    assert decision.outputs()[CLIPPY] == "false"


def test_an_unattested_head_runs_hosted(repo: Path) -> None:
    _commit(repo, "src/lib.rs", "pub fn g() {}\n", "after the stamp")
    queued = _squash(repo, "HEAD", _git(repo, "rev-parse", "base"))
    assert _judge(repo, queue_head=queued).outputs()[CLIPPY] == "false"


def test_the_switch_file_parses_and_ships_off() -> None:
    shipped = attest_queue.QueuePolicy.parse(
        (attest.ROOT / attest_queue.QUEUE_POLICY).read_text(encoding="utf-8")
    )
    assert shipped.mode == "off"
    assert shipped.signers
    assert attest_queue.QueuePolicy.parse(None).mode == "off"
    with pytest.raises(ValueError, match="forward must be"):
        attest_queue.QueuePolicy.parse('forward = "yes"\n')
