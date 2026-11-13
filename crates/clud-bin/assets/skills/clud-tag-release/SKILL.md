---
name: clud-tag-release
description: Inspect clud's automatic release handoff, recover a missing tag or dispatch, or push an annotated release tag in another tag-driven Rust/Python repository.
triggers:
  - When the user types "/clud-tag-release" with or without a version arg
  - When the user says "cut a release", "tag a release", "ship a version"
  - When the user wants to publish a new version of a Rust/Python crate
---
<!-- managed-by: clud -->

# /clud-tag-release

First identify the repository's release path. In `talos-hq/clud`, a version-bump
PR merged through the queue is the normal release action. Successful post-merge
main CI creates the annotated bare version tag and dispatches Auto Release on
that immutable tag. Observe that handoff; no manual tag push follows a normal
bump. The gates, retry behavior and operator recovery are owned by
[CI architecture](https://github.com/talos-hq/clud/blob/main/docs/architecture/ci.md#automatic-version-tags-2764).
Other repositories with tag-triggered releases can use the manual fallback below.

## Inspect clud's normal path

1. Read the workspace version and matching `pyproject.toml` project version.
   A requested version must match. Do not bump versions as part of this skill.
2. Locate the landed version-bump SHA and its original main-push CI run. Queue
   `merge_group` proof alone does not authorize a tag. `CI OK` must have actually
   executed its **Check results** verifier; a superseded shortcut is insufficient.
   Verified default-branch reuse accepted by that verifier is valid.
3. Check both `<version>` and `v<version>` remote tags, release records and Auto
   Release runs bound to the landed SHA. Existing refs must never move. If a
   matching release or run exists, report its URL/status instead of dispatching
   again. For a failed release, rerun that release or use its documented immutable
   acceptance recovery; do not create another tag or publication.
4. If main CI is queued/running, report its URL. If canceled (including queue
   overflow), rerun the original push run so its original event range and exact
   SHA receive verification. Do not substitute current main or a manual CI run.
5. A tag-created/dispatch-failed job retries reconciliation automatically. If both
   attempts failed, rerun the original main CI run/failed tagging job. It checks
   existing tag and run state before acting. A conflicting tag is a blocking
   error; report both SHAs without deletion, force updates or retagging.

## Explicit recovery and generic tag-driven repositories

This is a fallback for a missing automated handoff or a repository whose release
workflow expects a human tag. Inspect its workflow before changing remote state.
Do not create a GitHub release record directly: publication belongs to the release
workflow and its gates.

1. Detect the version from `Cargo.toml [workspace.package].version`, single-crate
   `[package].version`, or `pyproject.toml [project].version`. When both Cargo and
   Python versions exist, require them to match.
2. Find the existing workflow with version-tag push triggers and its optional
   dispatch inputs. Use the repository's established spelling; clud always creates
   bare `<version>` tags and treats `v<version>` as an existing equivalent.
3. Fetch origin. Require a clean, synchronized default-branch checkout, and verify
   the intended commit belongs to that branch and has the repository's required
   CI. For clud, use the exact landed SHA with successful executed post-merge CI,
   even if main has since advanced. Recheck version metadata at that SHA.
4. Check both tag spellings locally/remotely and both GitHub release records.
   Existing tags skip creation. A different target stops recovery. An existing
   release is immutable; use its original run's recovery path.
5. Before creating a missing tag, show the exact tag, SHA and workflow. Ask for
   confirmation only if the user's existing release instruction has not already
   authorized that action. Never proceed on silence when approval is required.
6. Create an annotated tag on the intended SHA:
   `git tag -a <tag> <sha> -m "Release <version>"`, then
   `git push origin refs/tags/<tag>`. Push exactly that ref, never `--tags`.
7. A human/App-token push can trigger the workflow. A default `GITHUB_TOKEN` push
   cannot. For clud recovery of an existing tag with no release run, dispatch the
   existing workflow on that exact tag:
   `gh workflow run auto-release.yml --ref refs/tags/<tag> -f tag=<tag>`.
   Do not dispatch against moving main. Check for a matching existing run first;
   a tag and a completed release handoff are distinct states.
8. Locate the run once using `gh run list --workflow <file> --limit 10 --json
   databaseId,event,headBranch,headSha,status,url`, matching tag and SHA. Report the
   tag target, run URL/status, and what the workflow publishes. If the run is not
   yet visible, report that fact instead of inventing success or redispatching.
   To wait when requested, use one `gh run watch <id> --exit-status` with the
   harness's live session/background completion mechanism. No sleep/poll loops.

## Scope and failure handling

Do not implement feature or code fixes inside this skill. Report a code blocker
and route it to the normal code-change workflow, following RED -> GREEN before
this release flow resumes. Do not create workflows, bypass
CI/review/merge queues, change published tags/assets, use `gh release create`,
or cut a release merely to test release automation. Keep unrelated repositories'
tag-driven paths supported; clud's automatic path does not depend on this skill.
