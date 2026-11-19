"""Forward a PR head's prooft attestations to its merge-queue entry (DD-227).

    python -m ci.attest_queue --base SHA --queue-head SHA --queue-ref REF \
        --pr-head SHA [--github-output]

`merge_group` CI runs this from the *base* commit's `ci/` package (no code
from the queued PR is imported), with the hash-pinned prooft release on the
path. It decides whether the queue entry may reduce Clippy and the Dylint
legs in place exactly as a same-repo PR run does (DD-225/DD-226), and writes
the same `skip_<job>=true|false` outputs `prooft plan` writes.

Forwarding is sound only when every condition holds; any other case, any
missing fact and any error runs everything hosted:

1. the base's `ci-attestations-queue.toml` sets `forward = "on"`
   (`"shadow"` computes and reports the decision but reduces nothing);
2. the queue ref names a same-repository PR whose live head is the commit
   CI fetched;
3. GitHub verified that head's commit signature, and the signer is listed in
   the base's `signers`: the trailers are in the signed message, so the
   signature binds who wrote them;
4. the entry is the first of its group (its single parent is the base) and
   its tree equals the PR head's tree, so the queue would test exactly the
   bytes the trailers prove (prooft binds tree *and* parents; the parents
   necessarily differ in a squash queue commit, so equality of trees is the
   whole proof);
5. `prooft queue` (base policy, trust surfaces, stamps, age) agrees.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.request import Request, urlopen

import tomllib

from ci.cancel_superseded import queue_pr

QUEUE_POLICY = "ci-attestations-queue.toml"
MODES = ("off", "shadow", "on")
SIGNATURE_QUERY = """query($owner:String!,$name:String!,$number:Int!,$head:GitObjectID!) {
  repository(owner:$owner,name:$name) {
    pullRequest(number:$number) {
      headRefOid headRepository { nameWithOwner } baseRepository { nameWithOwner }
    }
    object(oid:$head) { ... on Commit { signature { isValid state signer { login } } } }
  }
}"""

GraphQL = Callable[[str, Mapping[str, Any]], Mapping[str, Any]]


@dataclass(frozen=True)
class QueuePolicy:
    """The base's forwarding switch and signer allowlist."""

    mode: str
    signers: frozenset[str]

    @classmethod
    def parse(cls, text: str | None) -> QueuePolicy:
        if text is None:
            return cls("off", frozenset())
        raw = tomllib.loads(text)
        mode = raw.get("forward", "off")
        signers = raw.get("signers", [])
        if mode not in MODES:
            raise ValueError(f"{QUEUE_POLICY}: forward must be one of {', '.join(MODES)}")
        if not isinstance(signers, list) or not all(isinstance(s, str) and s for s in signers):
            raise ValueError(f"{QUEUE_POLICY}: signers must be a list of GitHub logins")
        return cls(mode, frozenset(s.lower() for s in signers))


@dataclass(frozen=True)
class Decision:
    """Which policy jobs the queue entry may reduce, and why."""

    mode: str
    reason: str
    skips: Mapping[str, bool]

    def outputs(self) -> dict[str, str]:
        """`skip_<job>` for every policy job; only `on` may say true."""
        live = self.mode == "on"
        return {
            "skip_" + job.split(":", 1)[1]: "true" if live and skip else "false"
            for job, skip in self.skips.items()
        }


def signer_problem(
    graphql: GraphQL, *, repository: str, number: int, pr_head: str, signers: frozenset[str]
) -> str | None:
    """None when `pr_head` is the live, same-repo head and an allowed signer signed it."""
    owner, name = repository.split("/", 1)
    data = graphql(
        SIGNATURE_QUERY, {"owner": owner, "name": name, "number": number, "head": pr_head}
    )
    if data.get("errors"):
        return "GitHub GraphQL lookup failed"
    repo = (data.get("data") or {}).get("repository") or {}
    pr = repo.get("pullRequest") or {}
    if pr.get("headRefOid") != pr_head:
        return f"PR #{number}'s live head is not the fetched {pr_head[:12]}"
    if (pr.get("headRepository") or {}).get("nameWithOwner") != repository:
        return "fork or unknown head repository"
    signature = (repo.get("object") or {}).get("signature") or {}
    login = ((signature.get("signer") or {}).get("login") or "").lower()
    if signature.get("isValid") is not True or signature.get("state") != "VALID" or not login:
        return "PR head has no GitHub-verified signature"
    if login not in signers:
        return f"signer {login} is not in the base's {QUEUE_POLICY} signers"
    return None


def judge(
    repo: Any,
    policy: QueuePolicy,
    *,
    base: str,
    queue_head: str,
    queue_ref: str,
    pr_head: str,
    repository: str,
    graphql: GraphQL,
    now: float | None = None,
) -> Decision:
    """Apply conditions 2-5 for `policy.mode`; the caller has applied 1."""
    jobs = tuple(repo.policy(base).jobs)
    hosted = dict.fromkeys(jobs, False)
    number = queue_pr(queue_ref)
    if number is None:
        return Decision(policy.mode, f"{queue_ref!r} is not a merge-queue ref", hosted)
    problem = signer_problem(
        graphql, repository=repository, number=number, pr_head=pr_head, signers=policy.signers
    )
    if problem:
        return Decision(policy.mode, problem, hosted)
    pr, queued = repo.commit(pr_head), repo.commit(queue_head)
    if queued.parents != (repo.commit(base).sha,):
        return Decision(policy.mode, "not the first entry of its group (batched)", hosted)
    if queued.tree != pr.tree:
        return Decision(
            policy.mode, "queue tree differs from the PR head (branch behind main)", hosted
        )
    plan = repo.queue(pr_head=pr.sha, queue_head=queued.sha, base=base, now=now)
    skips = {job.job: job.skip for job in plan.jobs}
    proven = ", ".join(job for job, skip in skips.items() if skip) or "nothing"
    return Decision(policy.mode, f"PR #{number} head {pr.sha[:12]}: proves {proven}", skips)


def open_repo(root: str) -> Any:
    """prooft's Repo with the queue switch added to the head trust surfaces."""
    from prooft.git import Git
    from prooft.repo import DEFAULT_SURFACES, Repo

    top = Git(Path(root)).run("rev-parse", "--show-toplevel").strip()
    return Repo(root=Path(top), surfaces=(*DEFAULT_SURFACES, QUEUE_POLICY))


def github_graphql(query: str, variables: Mapping[str, Any]) -> Mapping[str, Any]:
    request = Request(
        "https://api.github.com/graphql",
        data=json.dumps({"query": query, "variables": dict(variables)}).encode(),
        method="POST",
        headers={
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "Accept": "application/vnd.github+json",
        },
    )
    with urlopen(request, timeout=30) as response:
        return json.loads(response.read())


def _report(decision: Decision, github_output: bool) -> None:
    for key, value in decision.outputs().items():
        print(f"{'skip' if value == 'true' else 'run '} {key[5:]}")
    print(f"forwarding {decision.mode}: {decision.reason}")
    path = os.environ.get("GITHUB_OUTPUT")
    if github_output and path:
        with open(path, "a", encoding="utf-8") as handle:
            for key, value in decision.outputs().items():
                handle.write(f"{key}={value}\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    for flag in ("--base", "--queue-head", "--queue-ref", "--pr-head"):
        parser.add_argument(flag, required=True)
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", ""))
    parser.add_argument("-C", dest="root", default=".")
    parser.add_argument("--github-output", action="store_true")
    args = parser.parse_args(argv)
    repo = open_repo(args.root)
    policy = QueuePolicy.parse(repo.git.show(args.base, QUEUE_POLICY))
    if policy.mode == "off":
        print(f"forwarding off ({QUEUE_POLICY} at the base); the queue runs every lane")
        return 0
    decision = judge(
        repo,
        policy,
        base=args.base,
        queue_head=args.queue_head,
        queue_ref=args.queue_ref,
        pr_head=args.pr_head,
        repository=args.repository,
        graphql=github_graphql,
    )
    _report(decision, args.github_output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
