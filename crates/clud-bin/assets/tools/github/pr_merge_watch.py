#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "running-process==4.10.1",
# ]
# ///
# managed-by: clud
"""pr_merge_watch.py — fail-fast PR-check waiter for clud.

Polls a GitHub PR's checks; returns the moment a required check fails,
new review activity arrives, the PR closes/merges, or the timeout fires.
On non-success exits (and defensively on success), cancels still-running
workflow runs on the PR's head SHA so we stop burning matrix minutes on
results we've already decided to ignore.

Invoked via `"$CLUD_EXE" tool run github/pr_merge_watch.py …` so UV_CACHE_DIR
is pinned to ~/.clud/cache/uv per the three-layer enforcement (see
issue #408).

Immediately announces a durable JSONL log under
`~/.clud/logs/pr-merge-watch/<owner>__<repo>/`, never inside the watched
repository (#2042). Older versions wrote `<repo>/.clud/logs/pr-merge-watch/`;
that directory holds only these logs and is safe to delete. The first record is a UTC `START`; later
records use monotonic seconds relative to that start.

Exit codes:
  0  all required checks green AND mergeable=MERGEABLE (after the opt-in
     CodeRabbit wait below, when `--coderabbit-wait` is set)
  1  at least one required check failed (details on stdout)
  2  new review activity (unresolved coderabbit/human review)
  3  PR closed or merged out from under us
  4  timeout (configurable via --timeout, default 60min)
  5  approval required: a workflow run or check is `action_required` (a fork
     PR waiting for a maintainer); reported immediately, never pending
  6  never reported: a required check has no check run while every workflow
     run on the head commit has finished (path/branch filters, a
     `pull_request_target` run on the base commit, ...)
  7  stale: GitHub marked a required check `stale` (14 days incomplete); it
     never resolves on its own, a re-run is needed
  8  NO_CHECKS: no check will ever report (#1418): the base branch has no
     workflows, the head commit carries a skip marker (`[skip ci]`, ...), or
     the rollup stayed empty for the grace period (`--no-checks-grace`,
     default 60 s). The final event names the reason and `mergeStateStatus`;
     a `CLEAN` PR is mergeable as is
  9  CONFLICT: `mergeable=CONFLICTING` / `mergeStateStatus=DIRTY`, or
     `mergeable` stayed `UNKNOWN` for too many polls while checks were green
 10  GITHUB_UNREACHABLE: gh failed (auth, rate limit, network) for several
     polls in a row, a required failure could not be judged for lack of
     check-run data for several polls, or no repository could be resolved;
     the final event carries gh's stderr
 11  QUEUED: a run waited longer than `--max-queued` to start (off by default)
 12  NOT_ENQUEUED: a followed PR's auto-merge was withdrawn before the merge
     queue took it, so nothing will merge it
 13  EJECTED: the merge queue removed the PR (the reason is
     printed), or a required check failed on its merge-group commit (the job
     is named); the watch exits at once instead of waiting for the matrix
 14  UNMERGEABLE: the queue entry stayed UNMERGEABLE for consecutive polls
 64  USAGE: a bad flag or argument (argparse's own error). The caller's mistake,
     never a verdict: fix the command and run it again (#1331)
 124  the tool runner's watchdog stopped a resumable watch before the tool
      completed; re-invoke with the same args (`status: in-progress`)
 130/143  killed by SIGINT/SIGTERM: a final `EXIT` event with reason `killed`,
     and nothing is cancelled

A PR that is in a merge queue or set to auto-merge is followed to the merge
itself (#2035): green on its checks is not the end, and the watch exits 0
only once GitHub reports the PR `MERGED`. The gate snapshot already carries
the queue entry and auto-merge request, so a PR nothing else merges costs no
extra read. While it waits in the queue, the `merge_group` runs on the
queue's own commit are judged, not the PR head. A green PR that nothing will
merge exits 0 as before, so the caller can merge it. `--no-merge-wait` stops
at green on the PR's own checks and only reports that the merge is pending.
`--until merged` follows the PR directly until it merges, reporting queue
position and state changes and a persistent UNMERGEABLE entry distinctly. It
only observes the queue and never cancels workflow runs.

The tool runner allows requested waits up to 86400 seconds and reserves 60
seconds for the watcher to exit and finish cancellation. It rejects longer
requests. The watcher still clamps to the wrapper cap if necessary.
If the wrapper stops first, exit 124 means watch again; it is never green.

A watch only ever cancels runs on the head SHA it started on (#1418): when the
head moves it logs `head_moved`, keeps judging the new head, and skips any
cancellation. `--timeout` defaults to `$CLUD_PR_MERGE_WATCH_TIMEOUT` (else
3600 s); a caller whose tool call is capped must pass a lower value, or the
watch outlives it.

Supersession rule (#1330). Check runs are judged on the PR's *current* head
commit only, grouped by (workflow file, check name) -- never the display
`name:` -- and ordered by check-run id, not run id:
  - a `cancelled` check is replaced by any newer check in its group, even one
    still queued or in progress;
  - a completed result is replaced only by a newer check that actually ran
    (anything but `skipped`), so a skip never hides an older real failure;
  - a cancelled check with no replacement fails only when no other run of
    its workflow on the head commit replaces it: a newer run in any state
    (queued included), or any run that was not itself cancelled. Runs are
    ordered by `created_at`, then run number, never by run id: concurrency
    cancels whichever of two same-second runs entered its group first (#1710);
  - a failure in a cancelled run, or in a run a cancellation has reached, is
    replaced the same way: a superseded run's jobs are never the PR's result,
    and while a live sibling run exists such a failure is pending even if
    its run already reads `failure` (#2059);
  - `success`, `neutral` and `skipped` pass, but green needs at least one
    required check that actually ran on the head SHA, a protected check that
    skipped reads as pending unless the run that skipped it completed with
    `success` (a CI tier that skips a protected job, as GitHub itself
    accepts), and without branch protection every head run must have
    completed (#1639);
  - a cancellation-derived failure is acted on only after re-reading the
    PR's head: if the head moved, the verdict is dropped and the watch
    continues on the new commit;
  - on a failure in workflow X only the failing run and X's runs created
    strictly before it are cancelled; newer runs, same-second runs and other
    workflows are left alone. The failing run itself is cancelled only when
    none of its jobs is still in progress (GitHub cancels whole runs, so a
    cancel would discard sibling jobs' results); an unreadable job list
    keeps it too (obs-rust/obs-studio#13).
`merge_group` runs and check runs for any other commit are ignored; legacy
commit statuses keep GitHub's newest-per-context result.
The PR's check rollup never decides a failure on its own (#1742): it mixes
every run on the head, superseded ones included, with no run order. When a
poll has no REST check-run data, a required failure in the rollup is a
degraded poll, retried, and never a cancel; three in a row exit 10.

Without `--repo`, the watch targets the repo of `git remote get-url origin`
and passes it to every gh call (#1741): a bare gh call in a fork resolves to
the parent repository. gh's default repo is used only when origin is not a
github.com URL. The PR may be a number, a branch, a PR URL, or omitted for
the current branch; a branch is looked up on that same repo, and a URL names
its own.

CodeRabbit never stalls green. Nothing can reproduce CodeRabbit under a
local bosn -> act gate, and the fleet suppresses it by policy, so by default
GREEN is CI green plus mergeable and the watcher does not wait for CodeRabbit
at all. Green carries a note:
`coderabbit=<suppressed|absent|completed|rate_limited|skipped|pending|timeout>`.
- `suppressed`: the base branch's `.coderabbit.yaml` (or `.coderabbit.yml`)
  sets `reviews.auto_review.enabled: false`. It is read once per watch, only
  when CodeRabbit shows up at all, and no status is read after it.
- `absent`: CodeRabbit is not active here, or its `CodeRabbit` commit status
  is not on the head. That is final at once; there is no grace period.
- Otherwise the newest `CodeRabbit` status on the head SHA is reported as it
  stands (`pending` means it was not waited for).
`--coderabbit-wait N` opts in to waiting, for repos that still run CodeRabbit
(#1332): once required CI is green, a `pending` status is waited for at most N
seconds (never past `--timeout`), then green reads `coderabbit=timeout`. The
wait never fails and never cancels. `Review rate limited` and `Review skipped`
count as finished. New CodeRabbit threads still exit 2. The `CodeRabbit`
status itself is not a CI check unless branch protection requires it.

The exit code IS the result — do not pipe this through `tail`, `grep` or
`head`. A pipeline reports the *last* stage's status, so every one of the
codes above collapses to whatever the filter exited with, and `tail` buffers
to EOF so the per-poll progress lines vanish too. Redirect to a file and read
it, or run it bare.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import socket
import re
import signal
import sys
import time
import urllib.request
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime
from pathlib import Path
from typing import NoReturn, TextIO

from running_process import PIPE, RunningProcess, TimeoutExpired

EXIT_GREEN = 0
EXIT_REQUIRED_FAIL = 1
EXIT_REVIEW_ACTIVITY = 2
EXIT_PR_CLOSED = 3
EXIT_TIMEOUT = 4
EXIT_APPROVAL_REQUIRED = 5
EXIT_NEVER_REPORTED = 6
EXIT_STALE = 7
EXIT_NO_CHECKS = 8
EXIT_CONFLICT = 9
EXIT_GITHUB_UNREACHABLE = 10
EXIT_QUEUED = 11
# #2035: a followed PR lost its auto-merge before it was queued, so nothing
# will merge it.
EXIT_NOT_ENQUEUED = 12
# #2035: the merge queue removed the PR, or a required check failed on its
# merge-group commit.
EXIT_EJECTED = 13
EXIT_UNMERGEABLE = 14
# sysexits EX_USAGE. argparse exits 2 on a usage error, which is also this
# tool's "new review activity" verdict, so a bad flag read as a review (#1331).
EXIT_USAGE = 64

DEFAULT_TIMEOUT_SEC = 3600
DEFAULT_NO_CHECKS_GRACE_SEC = 60
# Every gh call is bounded (#1418): a stalled connection must not block the
# loop past --timeout, which is only checked between polls.
GH_CALL_TIMEOUT_SEC = 30.0
MAX_CONSECUTIVE_API_FAILURES = 3
MERGEABLE_UNKNOWN_MAX_POLLS = 6
# An immediate no-checks reason still waits this long: `pull_request_target`
# workflows and check apps can register seconds after a `[skip ci]` push.
NO_CHECKS_SETTLE_SEC = 5
SKIP_CI_MARKER = re.compile(r"\[(?:skip ci|ci skip|no ci|skip actions|actions skip)\]", re.I)
QUEUED_STATUSES = {"queued", "waiting", "pending", "requested"}

CANCEL_ON_CHOICES = {"fail", "review", "timeout", "closed", "always", "never"}
# A watcher timeout is not a CI failure: CI may be healthy, and the grind lander
# re-watches with `--timeout 540`, which would cancel CI every nine minutes (#1332).
# `timeout` stays a valid opt-in via `--cancel-on`.
CANCEL_ON_DEFAULTS = {"fail", "review", "closed"}
CANCEL_MODE_CHOICES = {"runs", "jobs", "none"}
# How often (in polls) a watch that found no CodeRabbit looks again (#1332).
CODERABBIT_RECHECK_POLLS = 3
# Once CI is green, how long to wait for CodeRabbit to finish the head commit
# (#1332). Zero by default: the wait is an explicit opt-in, so a repo without
# CodeRabbit (or a local bosn -> act gate that cannot run it) never stalls.
DEFAULT_CODERABBIT_WAIT_SEC = 0
CODERABBIT_CONFIG_FILES = (".coderabbit.yaml", ".coderabbit.yml")
CODERABBIT_STATUS_CONTEXT = "coderabbit"  # compared case-insensitively


def _utc_now() -> datetime:
    return datetime.now(UTC)


def _utc_text(value: datetime) -> str:
    return value.astimezone(UTC).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def _watch_root() -> Path:
    """The per-user clud home that holds watch logs (#2042).

    Never the watched repository: an untracked `.clud/logs/` there breaks
    every tool that needs a clean working tree.
    """
    return Path.home() / ".clud"


def _repo_slug(repo: str | None) -> str:
    """`owner__repo` for the log directory; `origin` when unresolved."""
    name = repo or _resolve_origin_repo() or "origin"
    return re.sub(r"[^A-Za-z0-9._-]", "_", name.replace("/", "__"))


@dataclass
class WatchLog:
    """Immediately announced JSONL event log under the per-user clud home."""

    path: Path
    display_path: str
    started_monotonic: float
    stream: TextIO
    closed: bool = False

    @classmethod
    def create(cls, pr: int, repo: str | None, *, root: Path | None = None) -> WatchLog:
        started_at = _utc_now()
        started_monotonic = time.monotonic()
        millis = started_at.microsecond // 1000
        stamp = f"{started_at:%Y%m%dT%H%M%S}.{millis:03d}Z-pr-{pr}"
        log_dir = (root or _watch_root()) / "logs" / "pr-merge-watch" / _repo_slug(repo)
        log_dir.mkdir(parents=True, exist_ok=True)
        path = log_dir / f"{stamp}.jsonl"
        suffix = 2
        while path.exists():
            path = log_dir / f"{stamp}-{suffix}.jsonl"
            suffix += 1
        display_path = str(path)
        stream = path.open("w", encoding="utf-8", newline="\n")
        log = cls(path, display_path, started_monotonic, stream)
        log._write(
            {
                "v": 1,
                "ts": _utc_text(started_at),
                "elapsed_sec": 0.0,
                "event": "START",
                "repo": repo or "origin",
                "pr": pr,
                "log_path": display_path,
            }
        )
        print(f"LOG {display_path}", file=sys.stderr, flush=True)
        return log

    def _write(self, record: dict) -> None:
        self.stream.write(json.dumps(record, separators=(",", ":")) + "\n")
        self.stream.flush()

    def emit(self, event: str, **fields: object) -> None:
        if self.closed:
            return
        record = {
            "v": 1,
            "elapsed_sec": round(max(0.0, time.monotonic() - self.started_monotonic), 2),
            "event": event,
        }
        if event == "cancel_item":
            # Which watcher cancelled what: with several watchers on one PR the
            # log must say whose cancel this was (#1332).
            record["watcher_pid"] = os.getpid()
            record["watcher_host"] = socket.gethostname()
        record.update(fields)
        self._write(record)

    def close(self) -> None:
        if self.closed:
            return
        self.stream.close()
        self.closed = True


# Every line of a GitHub Actions log carries a prefix the anchored patterns
# below must not see: the REST job-log endpoint emits
# `2026-09-03T19:45:30.9660131Z <line>`, and `gh run view --log-failed` emits
# `<job>\t<step>\t2026-…Z <line>`. Without stripping it, `^FAILED`,
# `^error` and `^thread .*? panicked` can never match a real log — which is
# how a pytest failure fell through to the transient-network pattern and was
# reported to the caller as "network/transient".
LOG_LINE_PREFIX = re.compile(
    r"^(?:[^\t]*\t[^\t]*\t)?\d{4}-\d{2}-\d{2}T[\d:.]+Z\s?"
)

# GitHub's workflow-command annotations. `##[error]` is how a step reports the
# thing that actually failed it -- `##[error]failing Rust harnesses:
# reaper-<hash>.exe (rc=-1073740791)` was the entire explanation for one red,
# and the anchored patterns below could not see it through the prefix.
GHA_ANNOTATION = re.compile(r"^##\[(?:error|warning)\]")

# The runner's own epilogue for any failed step. It names no cause, so as a
# first error it only sent the reader back to the raw log (#2445).
GENERIC_STEP_EXIT = re.compile(r"^##\[error\]Process completed with exit code \d+\.?$")

# Cargo's per-test result lines. These are NOT errors, and matching them is
# not hypothetical: `test tests::oversized_codes_are_truncated_not_panicked_on
# ... ok` contains the substring "panicked", so an unanchored search reported
# a PASSING test as the first error of a failed run. A first-error line that
# names the wrong thing is worse than none -- it sends the reader somewhere
# real and irrelevant.
CARGO_TEST_OUTCOME = re.compile(r"^test \S+ \.\.\. (ok|ignored)\b")

# A guard on pathological logs only. Real failure logs run to a few thousand
# lines; the summary that names the failing test is at the *end*, so this is
# deliberately not a head slice.
MAX_LOG_LINES = 100_000


def normalize_log(text: str) -> str:
    """Strip the runner's per-line prefix so anchored patterns mean what they say."""
    lines = text.splitlines()[:MAX_LOG_LINES]
    return "\n".join(LOG_LINE_PREFIX.sub("", line) for line in lines)


# First-error classifier patterns, applied to the normalized log.
# Order matters: first match wins.
CLASSIFIERS: list[tuple[re.Pattern[str], str]] = [
    (re.compile(r"^Diff in .*?:\d+:|run `cargo fmt", re.MULTILINE), "rustfmt drift"),
    (re.compile(r"^error: .*?clippy::|warning:.*?clippy::", re.MULTILINE), "clippy warning"),
    (re.compile(r"^error\[E\d+\]:|^error: could not compile", re.MULTILINE), "compile error"),
    (
        re.compile(
            r"^thread .*? panicked at"
            r"|FAILED \(\d+\)"
            r"|test result: FAILED"
            # A harness that died rather than reported: GitHub's step
            # annotation is all that survives when the process aborts.
            r"|^##\[error\]failing Rust harnesses:"
            # pytest: the summary line and the inline per-test marker. Without
            # these a timed-out pytest case matched "timed out" below and was
            # mislabelled transient.
            r"|^FAILED \S+::"
            r"|short test summary info"
            r"|^E\s+\w*(?:Error|Exception|AssertionError)",
            re.MULTILINE,
        ),
        "test failure",
    ),
    (
        re.compile(r"^ruff (check|format).*?(failed|error)", re.MULTILINE | re.IGNORECASE),
        "ruff violation",
    ),
    (
        re.compile(
            r"timed out|connection refused|temporary failure|EAI_AGAIN|503 Service",
            re.MULTILINE | re.IGNORECASE,
        ),
        "network/transient",
    ),
]


@dataclass
class GhResult:
    """Outcome of one gh CLI call."""

    exit_code: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.exit_code == 0


LAST_GH_ERROR = ""

# The clud session read broker prints this on stderr when, below the
# rate-limit reserve, it answers a read from its cache instead of
# refreshing it (#1743). A poll that read any such copy never decides a
# verdict: the gate waits for fresh data rather than act on a cached one.
BROKER_STALE_MARKER = "clud: gh read broker: rate-limit reserve"
STALE_READS = 0


def gh_error_note(text: str) -> None:
    """Remember the latest gh failure, for the final unreachable report."""
    global LAST_GH_ERROR
    if text.strip():
        LAST_GH_ERROR = text.strip()[-2000:]


# GitHub secondary (burst/concurrency) rate limit backoff (#2368). A 403/429
# with this wording is not an outage: the caller sleeps and retries within
# --timeout instead of counting it toward GITHUB_UNREACHABLE.
RATE_LIMIT_BASE_SEC = 30.0
RATE_LIMIT_MAX_SEC = 300.0
_RATE_LIMIT_RETRY_AFTER = re.compile(
    r"retry[- ]after[:=]?\s*(\d+)|try again in (\d+)\s*second", re.I
)
PENDING_RATE_LIMIT: str | None = None
RATE_LIMIT_STREAK = 0


def is_secondary_rate_limit(text: str) -> bool:
    low = text.lower()
    if "secondary rate limit" in low or "abuse detection" in low:
        return True
    return "rate limit" in low and ("http 403" in low or "http 429" in low)


def note_rate_limit(stderr: str) -> None:
    """Remember that the latest failed gh call was a secondary rate limit."""
    global PENDING_RATE_LIMIT
    if is_secondary_rate_limit(stderr):
        PENDING_RATE_LIMIT = stderr


def rate_limit_delay(text: str, streak: int) -> float:
    """`Retry-After` when gh's output carries it, else capped exponential."""
    match = _RATE_LIMIT_RETRY_AFTER.search(text)
    if match:
        seconds = float(next(g for g in match.groups() if g))
        return min(max(seconds, 1.0), RATE_LIMIT_MAX_SEC)
    return min(RATE_LIMIT_BASE_SEC * 2 ** max(streak - 1, 0), RATE_LIMIT_MAX_SEC)


def rate_limit_backoff(deadline: float, log: WatchLog | None) -> bool:
    """Sleep out a pending secondary rate limit; True when one was handled.

    The sleep never passes `deadline`, so the overall --timeout still rules.
    """
    global PENDING_RATE_LIMIT, RATE_LIMIT_STREAK
    text, PENDING_RATE_LIMIT = PENDING_RATE_LIMIT, None
    if text is None:
        RATE_LIMIT_STREAK = 0
        return False
    RATE_LIMIT_STREAK += 1
    delay = min(rate_limit_delay(text, RATE_LIMIT_STREAK), max(0.0, deadline - time.monotonic()))
    print(
        f"NOTE  GitHub secondary rate limit; retrying after {delay:.0f}s",
        file=sys.stderr,
        flush=True,
    )
    if log:
        log.emit("rate_limited", retry_after_sec=round(delay, 1), consecutive=RATE_LIMIT_STREAK)
    time.sleep(delay)
    return True


def gh(*args: str, check: bool = False, timeout: float | None = None) -> GhResult:
    """Run gh with the supplied args; return the captured outcome.

    `timeout` bounds the call (default `GH_CALL_TIMEOUT_SEC`): a call that
    would otherwise hang the loop is abandoned and reported as a failed call.
    """
    if timeout is None:
        timeout = GH_CALL_TIMEOUT_SEC
    if EXPLICIT_MERGE_WAIT and FOLLOW[1] > 0:
        timeout = min(timeout, max(0.001, FOLLOW[1] - time.monotonic()))
    try:
        # #1175: running-process merges stderr into stdout unless it is asked
        # for separately, and `capture_output=True` alone is not asking.
        # Without `stderr=PIPE` every `GhResult.stderr` was `None`, so the
        # first failed cancel crashed `_report_cancel` on `"HTTP 403" in None`.
        res = RunningProcess.run(
            ["gh", *args], capture_output=True, stderr=PIPE, text=True, timeout=timeout
        )
    except (TimeoutError, TimeoutExpired):
        # running-process raises its own `TimeoutExpired` (a
        # `subprocess.TimeoutExpired`, not a `TimeoutError`); catching only
        # the builtin let a hung probe escape as a crash (#1175 review).
        message = f"gh {' '.join(args)} timed out after {timeout}s"
        gh_error_note(message)
        return GhResult(124, "", message)
    stdout = res.stdout or ""
    stderr = res.stderr or ""
    if BROKER_STALE_MARKER in stderr:
        global STALE_READS
        STALE_READS += 1
    if res.returncode != 0:
        gh_error_note(stderr or f"gh {' '.join(args)} exited {res.returncode}")
        note_rate_limit(stderr)
    else:
        # A later success means the limit has lifted; drop a stale marker.
        global PENDING_RATE_LIMIT
        PENDING_RATE_LIMIT = None
    if check and res.returncode != 0:
        raise RuntimeError(f"gh {' '.join(args)} failed: {stderr.strip()}")
    return GhResult(res.returncode, stdout, stderr)


def broker_reads() -> bool:
    """Whether this process's `gh api` GETs go through the clud session
    read broker (#1743), which revalidates them by ETag for every reader."""
    return os.environ.get("CLUD_GH_READ_BROKER") == "1" and bool(
        os.environ.get("CLUD_GH_SHIM_TARGET")
    )


def ghd_reads() -> bool:
    """Whether this session routes `gh` through ghd (`gh.backend: "ghd"`,
    #2352), which shares REST reads across concurrent watchers (#2368). A
    per-process `If-None-Match`/`-i` read would defeat that sharing."""
    return os.environ.get("CLUD_GH_BACKEND") == "ghd" and bool(
        os.environ.get("CLUD_GH_SHIM_TARGET")
    )


def shared_reads() -> bool:
    """A plain `gh api <path>` is shared upstream: by the read broker or ghd."""
    return broker_reads() or ghd_reads()


# REST GETs this process sent outside a broker: path -> (ETag, parsed body).
_CONDITIONAL_CACHE: dict[str, tuple[str, object]] = {}


def _split_include(text: str) -> tuple[int, dict[str, str], str]:
    """`gh api -i` output -> (status, lower-case headers, body). Output with
    no status line (a fake, an older gh) reads as a plain 200 body."""
    if not text.startswith("HTTP/"):
        return 200, {}, text
    head, _, body = text.replace("\r\n", "\n").partition("\n\n")
    lines = head.split("\n")
    parts = lines[0].split()
    status = int(parts[1]) if len(parts) > 1 and parts[1].isdigit() else 0
    headers = {}
    for line in lines[1:]:
        name, sep, value = line.partition(":")
        if sep:
            headers[name.strip().lower()] = value.strip()
    return status, headers, body


def api_get(path: str) -> object | None:
    """One REST GET, parsed; None on any failure. It is always conditional:
    in a clud session the read broker, or ghd when gh routes through it,
    revalidates it with the ETag it holds (and shares the answer with every
    other reader); otherwise this process
    sends its own `If-None-Match`, and an unchanged resource is a `304`,
    which GitHub does not count against the rate limit."""
    if shared_reads():
        r = gh("api", path)  # ci-lint: allow GHAPI-001 brokered: the session read broker or ghd revalidates it by ETag
        return _parse_json(r)
    cached = _CONDITIONAL_CACHE.get(path)
    noted = LAST_GH_ERROR
    if cached is None:
        r = gh("api", "-i", path)
    else:
        r = gh("api", "-i", path, "-H", f"If-None-Match: {cached[0]}")
    status, headers, body = _split_include(r.stdout)
    if status == 304 and cached is not None:
        # gh exits 1 on a 304; it is not a failure to report.
        _restore_gh_error(noted)
        return cached[1]
    if not r.ok or status != 200:
        return None
    data = _parse_json(GhResult(0, body, ""))
    etag = headers.get("etag")
    if etag and data is not None:
        _CONDITIONAL_CACHE[path] = (etag, data)
    return data


def _restore_gh_error(text: str) -> None:
    global LAST_GH_ERROR
    LAST_GH_ERROR = text


def _parse_json(r: GhResult) -> object | None:
    if not r.ok or not r.stdout.strip():
        return None
    try:
        return json.loads(r.stdout)
    except json.JSONDecodeError:
        return None


def gh_json(*args: str) -> object | None:
    """Run gh and parse stdout as JSON; return None on any failure. A plain
    REST GET (`gh_json("api", path)`) is sent conditionally ([`api_get`])."""
    if len(args) == 2 and args[0] == "api" and not args[1].startswith("-") and (
        args[1] != "graphql"
    ):
        return api_get(args[1])
    return _parse_json(gh(*args))


@dataclass
class PRSnapshot:
    """One sample of the PR's gateable state."""

    number: int
    state: str  # OPEN | MERGED | CLOSED
    mergeable: str  # MERGEABLE | CONFLICTING | UNKNOWN
    head_sha: str
    base_ref: str
    head_ref: str = ""
    merge_state: str = "UNKNOWN"  # mergeStateStatus: CLEAN | DIRTY | BEHIND | BLOCKED | ...
    # In a merge queue or set to auto-merge (#2035): GitHub, not the caller,
    # will merge it, so green is followed to the merge itself.
    merge_pending: bool = False

    @classmethod
    def fetch(cls, pr: int, repo: str | None) -> PRSnapshot | None:
        args = ["pr", "view", str(pr)]
        if repo:
            args += ["--repo", repo]
        args += [
            "--json",
            "number,state,mergeable,headRefOid,baseRefName,headRefName,mergeStateStatus",
        ]
        data = gh_json(*args)
        if not isinstance(data, dict):
            return None
        return cls(
            number=int(data.get("number", pr)),
            state=str(data.get("state", "UNKNOWN")),
            mergeable=str(data.get("mergeable", "UNKNOWN")),
            head_sha=str(data.get("headRefOid", "")),
            base_ref=str(data.get("baseRefName", "main")),
            head_ref=str(data.get("headRefName") or ""),
            merge_state=str(data.get("mergeStateStatus") or "UNKNOWN"),
        )


@dataclass
class CheckRow:
    name: str
    bucket: str  # pass | fail | pending | skipping
    state: str
    link: str | None = None
    job_id: str | None = None  # populated lazily for failing checks


def check_counts(checks: list[CheckRow]) -> dict[str, int]:
    counts = {"total": len(checks), "pending": 0, "failed": 0, "succeeded": 0, "skipped": 0}
    for check in checks:
        bucket = check.bucket.lower()
        if bucket == "pass":
            counts["succeeded"] += 1
        elif bucket in {"fail", "cancel"}:
            counts["failed"] += 1
        elif bucket in {"skipping", "skip"}:
            counts["skipped"] += 1
        else:
            counts["pending"] += 1
    return counts


# ---------- head-commit check judgment (#1330) --------------------------------

PASSING_CONCLUSIONS = {"success", "neutral", "skipped"}
# A full-mode PR with re-runs passes 100 check runs; a runaway guard only.
MAX_PAGES = 50
PER_PAGE = 100


@dataclass(frozen=True)
class HeadChecks:
    """Raw REST data for one head commit, as GitHub returned it."""

    check_runs: list[dict]
    workflow_runs: list[dict]
    statuses: list[dict] = field(default_factory=list)


@dataclass
class CheckJudgment:
    """The judged state of one (workflow file, check name) group."""

    name: str
    workflow: str
    state: str  # pass | fail | pending | approval_required | stale | superseded
    conclusion: str
    check_run_id: int | None
    run_id: int | None
    link: str | None
    required: bool
    workflow_broken: bool = False  # startup_failure: the workflow, not the code
    cancelled: bool = False  # the failure is derived from a cancellation
    # The owning run's attempt and status when judged: a cancel re-checks them.
    run_attempt: int | None = None
    run_status: str = ""

    def as_check_row(self) -> CheckRow:
        bucket = {"pass": "pass", "superseded": "pass", "pending": "pending"}.get(
            self.state, "fail"
        )
        return CheckRow(self.name, bucket, self.conclusion.upper(), self.link)


@dataclass
class Verdict:
    """What the head commit's checks say, after the supersession rule."""

    state: str  # pending | pass | fail | approval_required | never_reported | stale
    judgments: list[CheckJudgment]
    failing: list[CheckJudgment]
    advisory_failing: list[CheckJudgment]
    # workflow key -> newest failing run id: the cancellation scope.
    failing_run_ids: dict[str, int]
    missing: list[str]
    notes: list[str]

    @property
    def cancellation_derived(self) -> bool:
        return any(j.cancelled for j in self.failing)


def _as_int(value: object) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def _lower(value: object) -> str:
    return str(value or "").lower()


def _workflow_key(run: dict) -> str:
    """The workflow's file path (or id), never its display `name:`."""
    path = run.get("path")
    if isinstance(path, str) and path:
        return path.split("@", 1)[0]
    workflow_id = _as_int(run.get("workflow_id"))
    if workflow_id is not None:
        return f"workflow:{workflow_id}"
    return f"run:{run.get('id')}"


def _is_cancelled(check: dict) -> bool:
    return _lower(check.get("status")) == "completed" and _lower(check.get("conclusion")) == (
        "cancelled"
    )


def _is_skipped(check: dict) -> bool:
    return _lower(check.get("status")) == "completed" and _lower(check.get("conclusion")) == (
        "skipped"
    )


def _run_succeeded(run: dict | None) -> bool:
    """The workflow run finished, and GitHub concluded it `success`."""
    return (
        run is not None
        and _lower(run.get("status")) == "completed"
        and _lower(run.get("conclusion")) == "success"
    )


def _run_order(run: dict) -> tuple[float, int, int]:
    """Creation order of a workflow run: `created_at`, then `run_number` (#1710).

    Never the run id alone: two runs created in one second (a PR opened with a
    label fires `opened` and `labeled`) get ids in no reliable order.
    """
    return (
        _parse_iso(run.get("created_at")) or 0.0,
        _as_int(run.get("run_number")) or 0,
        _as_int(run.get("id")) or 0,
    )


def _created_before(run: dict, anchor: dict | None) -> bool:
    """`run` was provably created before `anchor` (#1710).

    The same second, or a missing timestamp, is ambiguous: not before.
    """
    if anchor is None:
        return False
    mine = _parse_iso(run.get("created_at"))
    theirs = _parse_iso(anchor.get("created_at"))
    return mine is not None and theirs is not None and mine < theirs


def _from_older_attempt(check: dict, run: dict | None) -> bool:
    """The check finished before its run's current attempt started (#1449).

    `gh run rerun --failed` keeps the run id and head SHA, and the old
    attempt's failed check stays effective until the new attempt registers
    its own check run.
    """
    if run is None or (_as_int(run.get("run_attempt")) or 1) < 2:
        return False
    completed = _parse_iso(check.get("completed_at"))
    started = _parse_iso(run.get("run_started_at"))
    return completed is not None and started is not None and completed < started


def _pass_is_superseded(check: dict, run: dict | None, head_runs: list[dict]) -> bool:
    """A passing check that a still-unfinished run may overturn (#2338).

    True when a newer run of the same workflow on the head has not completed,
    or when the check's own run is being re-run (`gh run rerun`: attempt >= 2,
    not completed) and the pass is from the older attempt. Newer runs are
    ordered by `_run_order` (created_at, then run_number), so a same-second
    tie is broken correctly. An unfinished newer run (queued, waiting for
    approval, concurrency-queued) holds the pass at pending until it finishes
    or --timeout; this never yields a wrong GREEN.
    """
    if run is None:
        return False
    if _lower(run.get("status")) != "completed" and _from_older_attempt(check, run):
        return True
    key = _workflow_key(run)
    own = _run_order(run)
    return any(
        r["id"] != run["id"]
        and _workflow_key(r) == key
        and _run_order(r) > own
        and _lower(r.get("status")) != "completed"
        for r in head_runs
    )


def _effective(entries: list[tuple[dict, dict | None]]) -> tuple[dict, dict | None]:
    """The entry that stands for a group, in check-run id order.

    A cancelled check is replaced by anything newer. A newer cancellation
    never hides an older result, and a newer skip never hides an older result
    that actually ran.
    """
    ordered = sorted(entries, key=lambda entry: entry[0]["id"])
    current = ordered[0]
    for entry in ordered[1:]:
        check = entry[0]
        if _is_cancelled(current[0]):
            current = entry
        elif _is_cancelled(check):
            continue
        elif _is_skipped(check) and not _is_skipped(current[0]):
            continue
        else:
            current = entry
    return current


def judge_check_runs(  # noqa: C901
    check_runs: list[dict],
    workflow_runs: list[dict],
    head_sha: str,
    required: set[str] | None,
    *,
    statuses: list[dict] | None = None,
    head_branch: str | None = None,
    require_re: re.Pattern[str] | None = None,
    runs_grace_elapsed: bool = False,
) -> Verdict:
    """Judge the head commit's checks. Pure: no network, no clock.

    `check_runs` are `repos/{r}/commits/{sha}/check-runs` items and
    `workflow_runs` are `actions/runs?head_sha=` items, all pages. See the
    module docstring for the supersession rule this implements.

    `runs_grace_elapsed` says the head commit has had no workflow run for the
    whole grace period: a missing required check then never reports (#1418)
    instead of staying pending.
    """

    def is_required(name: str) -> bool:
        if require_re is not None:
            return bool(require_re.search(name))
        if not required:
            return True
        return name in required

    runs_by_id: dict[int, dict] = {}
    runs_by_suite: dict[int, dict] = {}
    for run in workflow_runs:
        if not isinstance(run, dict):
            continue
        run_id = _as_int(run.get("id"))
        if run_id is None:
            continue
        runs_by_id[run_id] = run
        suite = _as_int(run.get("check_suite_id"))
        if suite is not None:
            runs_by_suite[suite] = run

    def on_head(run: dict) -> bool:
        if run.get("head_sha") != head_sha or run.get("event") == "merge_group":
            return False
        branch = run.get("head_branch")
        return not (head_branch and branch and branch != head_branch)

    head_runs = [run for run in runs_by_id.values() if on_head(run)]

    def siblings(key: str, run_id: int) -> list[dict]:
        return [r for r in head_runs if _workflow_key(r) == key and r["id"] != run_id]

    def newer_runs(key: str, run_id: int) -> list[dict]:
        own = _run_order(runs_by_id[run_id])
        return [r for r in siblings(key, run_id) if _run_order(r) > own]

    def replacements(key: str, run_id: int) -> list[dict]:
        """The runs that stand in for a cancelled run (#1710): any newer run,
        and any sibling that was not itself cancelled. A concurrency group
        cancels whichever run entered it first, so neither the id nor the
        run number says which one survives; the survivor is the live one."""
        own = _run_order(runs_by_id[run_id])
        return [r for r in siblings(key, run_id) if not _is_cancelled(r) or _run_order(r) > own]

    def resolve_replaced(key: str, run_id: int) -> str | None:
        """pending/superseded when a replacement exists, else None."""
        found = replacements(key, run_id)
        if not found:
            return None
        return "pending" if any(_lower(r.get("status")) != "completed" for r in found) else (
            "superseded"
        )

    # A cancelled run with a live sibling was superseded by concurrency: its
    # checks never stand for a group the live run also reports (#1710).
    superseded_runs = {
        r["id"]
        for r in head_runs
        if _is_cancelled(r)
        and any(not _is_cancelled(s) for s in siblings(_workflow_key(r), r["id"]))
    }

    groups: dict[tuple[str, str], list[tuple[dict, dict | None]]] = {}
    for check in check_runs:
        if not isinstance(check, dict) or check.get("head_sha") != head_sha:
            continue
        name = check.get("name")
        if _as_int(check.get("id")) is None or not isinstance(name, str):
            continue
        run: dict | None = None
        linked = _extract_run_id_from_link(check.get("details_url") or check.get("html_url"))
        if linked:
            run = runs_by_id.get(int(linked))
        if run is None:
            suite = _as_int((check.get("check_suite") or {}).get("id"))
            if suite is not None:
                run = runs_by_suite.get(suite)
        if run is not None:
            if not on_head(run):
                continue
            key = _workflow_key(run)
        else:
            key = f"app:{(check.get('app') or {}).get('slug') or 'unknown'}"
        groups.setdefault((key, name), []).append((check, run))

    # Runs a cancellation has reached, even while they still read in_progress.
    runs_with_cancelled_check = {
        run["id"]
        for entries in groups.values()
        for check, run in entries
        if run is not None and _is_cancelled(check)
    }

    judgments: list[CheckJudgment] = []
    notes: list[str] = []
    for (key, name), entries in groups.items():
        events = {str(run.get("event")) for _check, run in entries if run is not None}
        if len(events) > 1:
            notes.append(
                f"{name} ({key}) reported by runs from events {sorted(events)}; "
                "the newest check counts"
            )
        live = [e for e in entries if e[1] is None or e[1]["id"] not in superseded_runs]
        check, run = _effective(live or entries)
        status = _lower(check.get("status"))
        conclusion = _lower(check.get("conclusion"))
        run_id = _as_int(run.get("id")) if run is not None else None
        cancelled = False
        if status != "completed":
            state = "pending"
        elif conclusion in PASSING_CONCLUSIONS:
            state = "pass"
        elif conclusion == "action_required":
            state = "approval_required"
        elif conclusion == "stale":
            state = "stale"
        elif conclusion == "cancelled":
            cancelled = True
            if run is not None and _lower(run.get("status")) != "completed":
                state = "pending"  # its own run is re-running
            else:
                replaced = resolve_replaced(key, run_id) if run_id is not None else None
                state = replaced or "fail"
        elif _from_older_attempt(check, run):
            state = "pending"  # a re-run attempt has not reported this check yet
        else:
            state = "fail"
            # A failure in a cancelled run (an `if: always()` gate, a job that
            # `needs:` a cancelled one) is the cancellation's, not the code's:
            # a replacement run supersedes it. So does one in a run that is
            # still being cancelled, while it reads in_progress (#1710).
            if run is not None and run_id is not None and (
                _lower(run.get("conclusion")) == "cancelled"
                or (
                    _lower(run.get("status")) != "completed"
                    and run_id in runs_with_cancelled_check
                )
            ):
                state = resolve_replaced(key, run_id) or state
            # A run's own conclusion can read `failure` before it settles on
            # `cancelled` (#2059): while a sibling of its workflow is still
            # live, a failure in a run a cancellation reached waits on it.
            elif run_id in runs_with_cancelled_check and any(
                not _is_cancelled(s) and _lower(s.get("status")) != "completed"
                for s in siblings(key, run_id)
            ):
                state = "pending"
        if state == "pass" and _pass_is_superseded(check, run, head_runs):
            state = "pending"
        judgments.append(
            CheckJudgment(
                name=name,
                workflow=key,
                state=state,
                conclusion=conclusion or status,
                check_run_id=_as_int(check.get("id")),
                run_id=run_id,
                link=check.get("details_url") or check.get("html_url") or None,
                required=is_required(name),
                workflow_broken=conclusion == "startup_failure",
                cancelled=cancelled,
                run_attempt=_as_int(run.get("run_attempt")) if run is not None else None,
                run_status=_lower(run.get("status")) if run is not None else "",
            )
        )

    # Run-level results that produce no check runs at all.
    for run in head_runs:
        if _lower(run.get("status")) != "completed":
            continue
        conclusion = _lower(run.get("conclusion"))
        if conclusion not in {"startup_failure", "action_required"}:
            continue
        key = _workflow_key(run)
        if newer_runs(key, run["id"]) or any(j.run_id == run["id"] for j in judgments):
            continue
        judgments.append(
            CheckJudgment(
                name=str(run.get("name") or key),
                workflow=key,
                state="fail" if conclusion == "startup_failure" else "approval_required",
                conclusion=conclusion,
                check_run_id=None,
                run_id=run["id"],
                link=run.get("html_url") or None,
                required=True,
                workflow_broken=conclusion == "startup_failure",
            )
        )

    # Legacy commit statuses: newest per context (highest id when present).
    seen_contexts: set[str] = set()
    for status_item in sorted(
        (s for s in statuses or [] if isinstance(s, dict)),
        key=lambda s: -(_as_int(s.get("id")) or 0),
    ):
        context = status_item.get("context")
        if not isinstance(context, str) or context in seen_contexts:
            continue
        seen_contexts.add(context)
        raw = _lower(status_item.get("state"))
        state = (
            "pass" if raw == "success" else "pending" if raw in {"pending", "expected"} else "fail"
        )
        judgments.append(
            CheckJudgment(
                name=context,
                workflow="status",
                state=state,
                conclusion=raw,
                check_run_id=None,
                run_id=None,
                link=status_item.get("target_url") or None,
                required=is_required(context),
            )
        )

    missing: list[str] = []
    if required and require_re is None:
        reported = {j.name for j in judgments}
        missing = sorted(required - reported)
    all_runs_done = bool(head_runs) and all(
        _lower(r.get("status")) == "completed" for r in head_runs
    )

    req = [j for j in judgments if j.required]
    real_failure_runs = {
        j.run_id for j in req if j.state == "fail" and not j.cancelled and j.run_id is not None
    }
    # Fail-fast siblings of a real failure in the same run are not the cause.
    failing = [
        j
        for j in req
        if j.state == "fail" and not (j.cancelled and j.run_id in real_failure_runs)
    ]
    advisory = [j for j in judgments if not j.required and j.state in {"fail", "stale"}]
    failing_run_ids: dict[str, int] = {}
    for j in failing:
        if j.run_id is None or j.workflow == "status":
            continue
        held = failing_run_ids.get(j.workflow)
        if held is None or _run_order(runs_by_id[j.run_id]) > _run_order(runs_by_id[held]):
            failing_run_ids[j.workflow] = j.run_id

    if any(j.state == "approval_required" for j in req):
        state = "approval_required"
    elif failing:
        state = "fail"
    elif any(j.state == "stale" for j in req):
        state = "stale"
    elif any(j.state == "pending" for j in req):
        state = "pending"
    elif missing:
        no_runs_ever = not head_runs and runs_grace_elapsed
        state = "never_reported" if all_runs_done or no_runs_ever else "pending"
    elif not judgments:
        state = "pass" if all_runs_done else "pending"
    elif not any(j.state == "pass" and j.conclusion != "skipped" for j in req):
        # Skips alone prove nothing ran on this head (#1639): the aggregate
        # may not exist yet because it `needs:` still-queued jobs.
        state = "pending"
    elif required and require_re is None and any(
        j.conclusion == "skipped" and not _run_succeeded(runs_by_id.get(j.run_id))
        for j in req
        if j.name in required
    ):
        # A protected gate (`CI OK`) that skipped is no verdict, unless the run
        # that skipped it finished green: a tier that skips a protected job
        # (bosn's `ci-test` skips `Full CI coverage`) is satisfied, as GitHub
        # treats it, or a ready PR is watched forever (zackees/bosn#360).
        state = "pending"
    elif not required and require_re is None and not all_runs_done and head_runs:
        # Without branch protection every check is required, including the
        # ones a queued run has not created yet.
        state = "pending"
    else:
        state = "pass"
    return Verdict(state, judgments, failing, advisory, failing_run_ids, missing, notes)


def paginate(path: str, key: str) -> list[dict] | None:
    """Every page of a REST list endpoint; None if any page is unreadable."""
    sep = "&" if "?" in path else "?"
    items: list[dict] = []
    for page in range(1, MAX_PAGES + 1):
        data = gh_json("api", f"{path}{sep}per_page={PER_PAGE}&page={page}")  # ci-lint: allow GHAPI-001 conditional: gh_json sends REST GETs through api_get (session broker ETag, else If-None-Match)
        if not isinstance(data, dict) or not isinstance(data.get(key), list):
            return None
        batch = [item for item in data[key] if isinstance(item, dict)]
        items.extend(batch)
        if len(data[key]) < PER_PAGE:
            break
    return items


def fetch_head_checks(
    repo: str, head_sha: str, statuses: list[dict] | None = None
) -> HeadChecks | None:
    """All check runs and workflow runs for one commit, every page."""
    if not head_sha:
        return None
    check_runs = paginate(f"repos/{repo}/commits/{head_sha}/check-runs?filter=all", "check_runs")
    if check_runs is None:
        return None
    runs = paginate(f"repos/{repo}/actions/runs?head_sha={head_sha}", "workflow_runs")
    if runs is None:
        return None
    return HeadChecks(check_runs, runs, list(statuses or []))


def fetch_checks(pr: int, repo: str | None) -> list[CheckRow] | None:
    args = ["pr", "checks", str(pr), "--json", "name,bucket,state,link"]
    if repo:
        args = ["pr", "checks", str(pr), "--repo", repo, "--json", "name,bucket,state,link"]
    # `gh pr checks` returns exit 1 when ANY check failed even with --json;
    # capture stdout regardless.
    res = gh(*args)
    if not res.stdout.strip():
        if re.search(r"no checks reported", res.stderr, re.IGNORECASE):
            return []
        return None
    try:
        rows = json.loads(res.stdout)
    except json.JSONDecodeError:
        return None
    if not isinstance(rows, list):
        return None
    return [
        CheckRow(
            name=str(r.get("name", "")),
            bucket=str(r.get("bucket", "pending")),
            state=str(r.get("state", "")),
            link=r.get("link") or None,
        )
        for r in rows
    ]


def fetch_required_check_names(repo: str, base_ref: str) -> set[str] | None:
    """Read the base branch's required-status-checks protection.

    Returns:
        - set of required check names if branch protection is configured;
        - empty set if protection exists but lists no checks;
        - None if the caller lacks permission OR no protection is set
          (callers should then fall back to --require allowlist).
    """
    data = gh_json("api", f"repos/{repo}/branches/{base_ref}/protection/required_status_checks")
    if not isinstance(data, dict):
        return None
    contexts = data.get("contexts")
    required = {str(context) for context in contexts} if isinstance(contexts, list) else set()
    checks = data.get("checks")
    if isinstance(checks, list):
        required.update(
            str(check["context"])
            for check in checks
            if isinstance(check, dict) and isinstance(check.get("context"), str)
        )
    return required


# One bounded probe. It runs on the fail-fast path, ahead of cancellation,
# so it must never be what delays either.
LOG_PROBE_TIMEOUT_SEC = 25.0
# #1631: a concluded job can briefly serve an empty log body while GitHub
# finishes uploading it. One short, bounded retry; never a loop.
LOG_EMPTY_RETRY_DELAY_SEC = 3.0


def _log_failure_reason(source: str, res: GhResult) -> str:
    """Why one log source produced nothing, in a few words."""
    if res.exit_code == 124:
        return f"{source} timed out after {LOG_PROBE_TIMEOUT_SEC:g}s"
    if res.ok:
        return f"{source} returned an empty log"
    detail = (res.stderr or res.stdout).strip()
    if "still in progress" in detail:
        return f"{source} refused while the run is still in progress"
    first = detail.splitlines()[0][:160] if detail else f"exit {res.exit_code}"
    return f"{source} failed ({first})"


def fetch_failure_log_explained(
    repo: str, run_id: str | None, job_id: str | None
) -> tuple[str, str]:
    """`(log, why_unavailable)`: exactly one of the two is non-empty.

    Same sources and order as `fetch_failure_log`; the second value names
    each source that produced nothing and why (#1631), so a missing first
    error line is explained rather than silent.
    """
    reasons: list[str] = []
    if job_id:
        args = (
            "api",
            f"repos/{repo}/actions/jobs/{job_id}/logs",
            "--allow-escape-sequences",
        )
        res = gh(*args, timeout=LOG_PROBE_TIMEOUT_SEC)
        if res.ok and not res.stdout.strip():
            # The job has concluded (it is being reported as failed), so an
            # empty body is an upload lag, not "no log". Retry once.
            time.sleep(LOG_EMPTY_RETRY_DELAY_SEC)
            res = gh(*args, timeout=LOG_PROBE_TIMEOUT_SEC)
        if res.ok and res.stdout.strip():
            return normalize_log(strip_ansi(res.stdout)), ""
        reasons.append(_log_failure_reason("job log", res))
    if run_id:
        res = gh(
            "run", "view", run_id, "--repo", repo, "--log-failed",
            timeout=LOG_PROBE_TIMEOUT_SEC,
        )
        if res.ok and res.stdout.strip():
            return normalize_log(strip_ansi(res.stdout)), ""
        reasons.append(_log_failure_reason("run log", res))
    return "", "; ".join(reasons) or "no job or run id"


def fetch_failure_log(repo: str, run_id: str | None, job_id: str | None) -> str:
    """The failing job's log, normalized, or `""` if it cannot be read now.

    Prefers the REST **job** endpoint. `gh run view --log-failed` resolves the
    whole *run*, and GitHub refuses that while any job is still going —
    `run … is still in progress; logs will be available when it is complete`.
    On the fail-fast path the run is in progress by definition, so the
    run-level probe returns nothing exactly when the caller needs it most.
    The job endpoint serves a finished job's log regardless of its siblings.
    """
    return fetch_failure_log_explained(repo, run_id, job_id)[0]


ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def strip_ansi(text: str) -> str:
    return ANSI_ESCAPE.sub("", text)


# Ordered most- to least-specific. `##[error]` first because when a step
# emits one it is by construction the reason the step failed; the rest are
# what a build or test harness prints on its way there.
FIRST_ERROR_PATTERNS: list[re.Pattern[str]] = [
    re.compile(r"^##\[error\]"),
    # A harness's own preflight refusal (`FATAL [pre-extract]: insufficient
    # free disk ...`) names the cause in plain text (#2457).
    re.compile(r"^FATAL\b"),
    re.compile(r"^FAILED \S"),
    re.compile(r"^test result: FAILED"),
    re.compile(r"^--- FAILED"),
    re.compile(r"^thread .*? panicked at"),
    re.compile(r"^error(\[E\d+\])?:"),
    re.compile(r"^Error:"),
    re.compile(r"^Diff in "),
]

# `thread '<name>' [(<tid>)] panicked at`: the test harness names the thread after the
# test, so a panic can be attributed to the test that printed it. Current Rust
# inserts the thread id (`'name' (12080) panicked`); older Rust omits it (#1623).
PANIC_THREAD = re.compile(r"^thread '([^']+)'(?: \(\d+\))? panicked at")
CARGO_TEST_OK = re.compile(r"^test (\S+) \.\.\. ok\b")


def first_error_line(sample: str) -> str:
    """The first line that names why the job failed, or `""`.

    Passing cargo test lines are skipped explicitly rather than filtered by
    pattern precision, because a test may legitimately be *named* after the
    failure mode it guards ("..._not_panicked_on") and no error pattern can
    tell that apart from a real panic by content alone.

    A panic printed by a test that then reports `... ok` (a deliberate,
    caught panic) is skipped too (#1616): it is not why the job failed.
    """
    lines = [raw.strip() for raw in sample.splitlines()]
    passed = {m.group(1) for line in lines if (m := CARGO_TEST_OK.match(line))}
    for line in lines:
        if not line or CARGO_TEST_OUTCOME.match(line) or GENERIC_STEP_EXIT.match(line):
            continue
        panic = PANIC_THREAD.match(line)
        if panic and panic.group(1) in passed:
            continue
        for pattern in FIRST_ERROR_PATTERNS:
            if pattern.search(line):
                return GHA_ANNOTATION.sub("", line).strip()
    return ""


def classify_failure(
    repo: str, run_id: str | None, job_id: str | None
) -> tuple[str, str | None]:
    """Read the failing job's log and classify the first error.

    Returns (first_error_line, classifier_label).
    """
    return classify_failure_explained(repo, run_id, job_id)[:2]


def classify_failure_explained(
    repo: str, run_id: str | None, job_id: str | None
) -> tuple[str, str | None, str]:
    """`classify_failure` plus why the log was unavailable (`""` if read)."""
    sample, unavailable = fetch_failure_log_explained(repo, run_id, job_id)  # ci-lint: allow GHAPI-001 on exit only: the failing job's log for the report
    if not sample:
        return "", None, unavailable
    first_err = first_error_line(sample)
    label = None
    for pattern, lbl in CLASSIFIERS:
        if pattern.search(sample):
            label = lbl
            break
    return first_err, label, ""


FIRST_ERROR_CAP_CHARS = 1000

# Failing non-required jobs diagnosed when the required failure is an
# aggregate gate with nothing to say (#2445). Each costs one log request.
ROOT_CAUSE_REPORTS = 3


def root_cause_reports(primary, advisory, build) -> list:
    """Reports for the jobs behind an uninformative required failure.

    A gate such as `CI OK` fails only because the jobs it needs failed, and its
    log holds nothing but the runner's exit line. Diagnose up to
    `ROOT_CAUSE_REPORTS` of the failing advisory jobs instead, so one watcher
    call ends with the real errors.
    """
    if primary.first_error or primary.classifier:
        return []
    return [build(item) for item in advisory[:ROOT_CAUSE_REPORTS]]


@dataclass
class FailureReport:
    check: CheckRow
    run_id: str | None
    first_error: str
    classifier: str | None
    # #1631: why no log could be read; rendered so the gap is not silent.
    log_unavailable: str = ""

    def render(self) -> str:
        lines = [
            f"FAIL  {self.check.name}",
            f"  conclusion: {self.check.state or self.check.bucket}",
        ]
        if self.check.link:
            lines.append(f"  link:       {self.check.link}")
        if self.run_id:
            lines.append(f"  log probe:  gh run view {self.run_id} --log-failed | tail -100")
        if self.first_error:
            first = self.first_error
            if len(first) > FIRST_ERROR_CAP_CHARS:
                # #1676: one minified/joined log line must not flood stdout.
                first = (
                    f"{first[:FIRST_ERROR_CAP_CHARS]} [TRUNCATED: showed "
                    f"{FIRST_ERROR_CAP_CHARS} of {len(self.first_error)} chars; "
                    "full log via the log probe above]"
                )
            lines.append(f"  first error: {first}")
        elif self.log_unavailable:
            lines.append(f"  log unavailable: {self.log_unavailable}")
        if self.classifier:
            lines.append(f"  classifier: {self.classifier}")
        return "\n".join(lines)


# ---------- review activity ---------------------------------------------------

CODERABBIT_LOGINS = {"coderabbitai", "coderabbitai[bot]", "coderabbit[bot]"}


def _is_coderabbit(login: object) -> bool:
    return isinstance(login, str) and login.lower() in CODERABBIT_LOGINS


@dataclass(frozen=True)
class CodeRabbitProbe:
    state: str  # detected | not_detected | degraded
    sampled_merged_prs: int


@dataclass(frozen=True)
class CodeRabbitObservation:
    state: str  # quiet | skipped | actionable | degraded
    reason: str | None = None
    actionable: bool = False
    unresolved_threads: int = 0
    ids: frozenset[int] = frozenset()


def probe_coderabbit(repo: str) -> CodeRabbitProbe:
    recent = gh_json(
        "pr", "list", "--repo", repo, "--state", "merged", "--limit", "5", "--json", "number"
    )
    if not isinstance(recent, list):
        return CodeRabbitProbe("degraded", 0)
    sampled = 0
    for item in recent:
        if not isinstance(item, dict) or not isinstance(item.get("number"), int):
            continue
        sampled += 1
        reviews = gh_json("api", f"repos/{repo}/pulls/{item['number']}/reviews?per_page=100")
        if not isinstance(reviews, list):
            return CodeRabbitProbe("degraded", sampled)
        if any(
            isinstance(review, dict) and _is_coderabbit((review.get("user") or {}).get("login"))
            for review in reviews
        ):
            return CodeRabbitProbe("detected", sampled)
    return CodeRabbitProbe("not_detected", sampled)


def classify_coderabbit(  # noqa: C901
    threads: list[dict],
    status_comments: list[dict],
) -> CodeRabbitObservation:
    ids: set[int] = set()
    unresolved = 0
    for thread in threads:
        if not isinstance(thread, dict) or thread.get("isResolved") is True:
            continue
        nodes = (thread.get("comments") or {}).get("nodes", [])
        if not isinstance(nodes, list):
            continue
        coderabbit_comments = [
            comment
            for comment in nodes
            if isinstance(comment, dict)
            and _is_coderabbit((comment.get("author") or {}).get("login"))
        ]
        if not coderabbit_comments:
            continue
        unresolved += 1
        for comment in coderabbit_comments:
            if isinstance(comment.get("databaseId"), int):
                ids.add(comment["databaseId"])
    if unresolved:
        return CodeRabbitObservation(
            "actionable",
            actionable=True,
            unresolved_threads=unresolved,
            ids=frozenset(ids),
        )

    for comment in reversed(status_comments):
        if not isinstance(comment, dict):
            continue
        if not _is_coderabbit((comment.get("user") or {}).get("login")):
            continue
        body = str(comment.get("body", ""))
        credit_subject = r"credits?|quota|usage\s+limit"
        unavailable = r"exhaust(?:ed|ion)?|out\s+of|deplet(?:ed|ion)?|paused|unavailable"
        credits = re.search(
            rf"(?:{credit_subject}).{{0,100}}(?:{unavailable})|"
            rf"(?:{unavailable}).{{0,100}}(?:{credit_subject})",
            body,
            re.IGNORECASE | re.DOTALL,
        )
        if credits:
            return CodeRabbitObservation("skipped", reason="credits_exhausted")
        if re.search(r"review\s+skipped", body, re.IGNORECASE):
            return CodeRabbitObservation("skipped", reason="review_skipped")
    return CodeRabbitObservation("quiet")


@dataclass(frozen=True)
class GateSnapshot:
    pr: PRSnapshot
    checks: list[CheckRow]
    human_review_ids: frozenset[int]
    coderabbit_probe: CodeRabbitProbe | None
    coderabbit: CodeRabbitObservation | None
    # REST check runs + workflow runs for the head commit (#1330); None when
    # unavailable, in which case the rollup rows above are judged instead.
    head_checks: HeadChecks | None = None


def _rollup_check(node: dict) -> CheckRow | None:
    kind = node.get("__typename")
    if kind == "CheckRun":
        name = str(node.get("name", ""))
        status = str(node.get("status", "")).upper()
        conclusion = str(node.get("conclusion") or "").upper()
        if status != "COMPLETED":
            bucket = "pending"
        elif conclusion in {"SUCCESS", "NEUTRAL"}:
            bucket = "pass"
        elif conclusion == "SKIPPED":
            bucket = "skipping"
        elif conclusion == "CANCELLED":
            bucket = "cancel"
        else:
            bucket = "fail"
        return CheckRow(name, bucket, conclusion or status, node.get("detailsUrl") or None)
    if kind == "StatusContext":
        name = str(node.get("context", ""))
        state = str(node.get("state", "")).upper()
        bucket = (
            "pass"
            if state == "SUCCESS"
            else "pending"
            if state in {"PENDING", "EXPECTED"}
            else "fail"
        )
        return CheckRow(name, bucket, state, node.get("targetUrl") or None)
    return None


def _connection_truncated(connection: object, *, from_end: bool = False) -> bool:
    if not isinstance(connection, dict):
        return True
    page_info = connection.get("pageInfo")
    if not isinstance(page_info, dict):
        return True
    key = "hasPreviousPage" if from_end else "hasNextPage"
    value = page_info.get(key)
    return not isinstance(value, bool) or value


_ROLLUP_PAGE_QUERY = """
query($owner:String!,$name:String!,$oid:GitObjectID!,$after:String!){
  repository(owner:$owner,name:$name){
    object(oid:$oid){... on Commit{statusCheckRollup{contexts(first:100,after:$after){nodes{
      __typename
      ... on CheckRun{name status conclusion detailsUrl}
      ... on StatusContext{context state targetUrl}
    } pageInfo{hasNextPage endCursor}}}}}
  }
}
"""

# Bounds the page walk: 50 pages is 5000 contexts, far beyond any real PR.
_MAX_ROLLUP_PAGES = 50


def _all_rollup_contexts(
    owner: str, name: str, head_sha: str, contexts: object
) -> list[dict] | None:
    """Return every rollup context, following `contexts` pagination (#1604).

    Later pages are pinned to `head_sha`, the commit the first page came
    from, so a push mid-walk cannot splice two commits' checks together.
    Any failed, malformed or unbounded page yields None: a partial rollup is
    never reported as the whole one.
    """
    collected: list[dict] = []
    for _ in range(_MAX_ROLLUP_PAGES):
        if not isinstance(contexts, dict):
            return None
        nodes = contexts.get("nodes")
        page_info = contexts.get("pageInfo")
        if not isinstance(nodes, list) or not isinstance(page_info, dict):
            return None
        collected.extend(node for node in nodes if isinstance(node, dict))
        has_next = page_info.get("hasNextPage")
        if has_next is False:
            return collected
        cursor = page_info.get("endCursor")
        if has_next is not True or not isinstance(cursor, str) or not cursor or not head_sha:
            return None
        data = gh_json(  # ci-lint: allow GHAPI-001 GraphQL (no ETag; own budget): one gate snapshot per poll, and polls wait on the broker subscription in a session
            "api",
            "graphql",
            "-f",
            f"query={_ROLLUP_PAGE_QUERY}",
            "-F",
            f"owner={owner}",
            "-F",
            f"name={name}",
            "-f",
            f"oid={head_sha}",
            "-f",
            f"after={cursor}",
        )
        repository = (
            ((data or {}).get("data") or {}).get("repository") if isinstance(data, dict) else None
        )
        commit = repository.get("object") if isinstance(repository, dict) else None
        rollup = commit.get("statusCheckRollup") if isinstance(commit, dict) else None
        contexts = rollup.get("contexts") if isinstance(rollup, dict) else None
    return None


_REVIEWS_PAGE_QUERY = """
query($id:ID!,$after:String!){
  node(id:$id){... on PullRequest{
    reviews(first:100,after:$after){
      nodes{databaseId state author{login}} pageInfo{hasNextPage endCursor}
    }
  }}
}
"""

_THREADS_PAGE_QUERY = """
query($id:ID!,$after:String!){
  node(id:$id){... on PullRequest{
    reviewThreads(first:100,after:$after){
      nodes{id isResolved comments(first:20){
        nodes{databaseId body author{login}} pageInfo{hasNextPage endCursor}
      }}
      pageInfo{hasNextPage endCursor}
    }
  }}
}
"""

_THREAD_COMMENTS_PAGE_QUERY = """
query($id:ID!,$after:String!){
  node(id:$id){... on PullRequestReviewThread{
    comments(first:100,after:$after){
      nodes{databaseId body author{login}} pageInfo{hasNextPage endCursor}
    }
  }}
}
"""


def _all_node_connection(
    node_id: object, field: str, query: str, first: object
) -> list[dict] | None:
    """Return every node of `field`, paging through `node(id:)` (#1617).

    Follow-up pages are pinned to `node_id` (the pull request or review
    thread the first page came from). Same fail-closed rules as
    `_all_rollup_contexts`: any failed, malformed or unbounded page yields
    None, so a partial list is never treated as the whole one.
    """
    collected: list[dict] = []
    connection = first
    for _ in range(_MAX_ROLLUP_PAGES):
        if not isinstance(connection, dict):
            return None
        nodes = connection.get("nodes")
        page_info = connection.get("pageInfo")
        if not isinstance(nodes, list) or not isinstance(page_info, dict):
            return None
        collected.extend(node for node in nodes if isinstance(node, dict))
        has_next = page_info.get("hasNextPage")
        if has_next is False:
            return collected
        cursor = page_info.get("endCursor")
        if (
            has_next is not True
            or not isinstance(cursor, str)
            or not cursor
            or not isinstance(node_id, str)
            or not node_id
        ):
            return None
        data = gh_json(  # ci-lint: allow GHAPI-001 GraphQL (no ETag; own budget): one gate snapshot per poll, and polls wait on the broker subscription in a session
            "api", "graphql", "-f", f"query={query}", "-f", f"id={node_id}", "-f", f"after={cursor}"
        )
        node = ((data or {}).get("data") or {}).get("node") if isinstance(data, dict) else None
        connection = node.get(field) if isinstance(node, dict) else None
    return None


LAST_SNAPSHOT_FAILURE = ""


def _snapshot_gap(reason: str) -> None:
    """Record why no snapshot was produced, so logs name the real cause."""
    global LAST_SNAPSHOT_FAILURE
    LAST_SNAPSHOT_FAILURE = reason
    return None


def fetch_gate_snapshot(repo: str, pr: int, *, include_coderabbit: bool) -> GateSnapshot | None:  # noqa: C901
    _snapshot_gap("GraphQL call failed or returned no pull request")
    owner, separator, name = repo.partition("/")
    if not separator or not owner or not name:
        return None
    query = """
query($owner:String!,$name:String!,$number:Int!,$includeCoderabbit:Boolean!){
  repository(owner:$owner,name:$name){
    pullRequest(number:$number){
      id number state mergeable mergeStateStatus headRefOid baseRefName headRefName
      autoMergeRequest{enabledAt} mergeQueueEntry{state}
      reviews(first:100){
        nodes{databaseId state author{login}} pageInfo{hasNextPage endCursor}
      }
      reviewThreads(first:100) @include(if:$includeCoderabbit){
        nodes{id isResolved comments(first:20){
          nodes{databaseId body author{login}} pageInfo{hasNextPage endCursor}
        }}
        pageInfo{hasNextPage endCursor}
      }
      comments(last:100) @include(if:$includeCoderabbit){
        nodes{body author{login}} pageInfo{hasPreviousPage}
      }
      commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){nodes{
        __typename
        ... on CheckRun{name status conclusion detailsUrl}
        ... on StatusContext{context state targetUrl}
      } pageInfo{hasNextPage endCursor}}}}}}
    }
    recent:pullRequests(first:5,states:MERGED,orderBy:{field:UPDATED_AT,direction:DESC})
      @include(if:$includeCoderabbit){
      nodes{number reviews(first:100){nodes{author{login}} pageInfo{hasNextPage}}}
    }
  }
}
"""
    data = gh_json(  # ci-lint: allow GHAPI-001 GraphQL (no ETag; own budget): one gate snapshot per poll, and polls wait on the broker subscription in a session
        "api",
        "graphql",
        "-f",
        f"query={query}",
        "-F",
        f"owner={owner}",
        "-F",
        f"name={name}",
        "-F",
        f"number={pr}",
        "-F",
        f"includeCoderabbit={'true' if include_coderabbit else 'false'}",
    )
    repository = (
        ((data or {}).get("data") or {}).get("repository") if isinstance(data, dict) else None
    )
    pull = repository.get("pullRequest") if isinstance(repository, dict) else None
    if not isinstance(pull, dict):
        return None

    pull_id = pull.get("id")
    review_nodes = _all_node_connection(
        pull_id, "reviews", _REVIEWS_PAGE_QUERY, pull.get("reviews")
    )
    if review_nodes is None:
        return _snapshot_gap("reviews pagination failed or was malformed")

    commit_nodes = (pull.get("commits") or {}).get("nodes") or []
    rollup_nodes: list[dict] = []
    if isinstance(commit_nodes, list) and commit_nodes and isinstance(commit_nodes[-1], dict):
        commit = commit_nodes[-1].get("commit")
        if not isinstance(commit, dict):
            return None
        rollup = commit.get("statusCheckRollup")
        if rollup is not None:
            if not isinstance(rollup, dict):
                return None
            paged = _all_rollup_contexts(
                owner, name, str(pull.get("headRefOid") or ""), rollup.get("contexts")
            )
            if paged is None:
                return _snapshot_gap("check-context pagination failed or was malformed")
            rollup_nodes = paged
    else:
        return None
    checks = [row for node in rollup_nodes if (row := _rollup_check(node)) is not None]
    # GitHub already keeps only the newest status per context in the rollup.
    statuses = [
        {
            "context": node.get("context"),
            "state": str(node.get("state", "")).lower(),
            "target_url": node.get("targetUrl"),
        }
        for node in rollup_nodes
        if node.get("__typename") == "StatusContext"
    ]

    human_ids = frozenset(
        review["databaseId"]
        for review in review_nodes
        if isinstance(review.get("databaseId"), int)
        and "[bot]" not in str((review.get("author") or {}).get("login", ""))
        and review.get("state") in {"CHANGES_REQUESTED", "COMMENTED"}
    )

    probe: CodeRabbitProbe | None = None
    observation: CodeRabbitObservation | None = None
    if include_coderabbit:
        comments_connection = pull.get("comments")
        if _connection_truncated(comments_connection, from_end=True):
            return _snapshot_gap("PR comments truncated (more than 100)")
        threads = _all_node_connection(
            pull_id, "reviewThreads", _THREADS_PAGE_QUERY, pull.get("reviewThreads")
        )
        if threads is None:
            return _snapshot_gap("review-thread pagination failed or was malformed")
        for thread in threads:
            thread_comments = _all_node_connection(
                thread.get("id"), "comments", _THREAD_COMMENTS_PAGE_QUERY, thread.get("comments")
            )
            if thread_comments is None:
                return _snapshot_gap("review-thread comment pagination failed or was malformed")
            thread["comments"] = {"nodes": thread_comments}
        recent = repository.get("recent")
        recent_nodes = (recent.get("nodes") or []) if isinstance(recent, dict) else None
        sampled = 0
        detected = False
        if isinstance(recent_nodes, list):
            for recent in recent_nodes:
                if not isinstance(recent, dict) or not isinstance(recent.get("number"), int):
                    continue
                sampled += 1
                recent_reviews = recent.get("reviews")
                if _connection_truncated(recent_reviews):
                    return _snapshot_gap("recent PR reviews truncated (more than 100)")
                reviews = recent_reviews.get("nodes") or []
                if isinstance(reviews, list) and any(
                    isinstance(review, dict)
                    and _is_coderabbit((review.get("author") or {}).get("login"))
                    for review in reviews
                ):
                    detected = True
                    break
            probe = CodeRabbitProbe("detected" if detected else "not_detected", sampled)
        else:
            probe = CodeRabbitProbe("degraded", 0)
        comments = comments_connection.get("nodes") or []
        if not isinstance(comments, list):
            observation = CodeRabbitObservation("degraded", reason="malformed_payload")
        else:
            normalized_comments = [
                {"user": comment.get("author") or {}, "body": comment.get("body", "")}
                for comment in comments
                if isinstance(comment, dict)
            ]
            observation = classify_coderabbit(threads, normalized_comments)

    return GateSnapshot(
        pr=PRSnapshot(
            number=int(pull.get("number", pr)),
            state=str(pull.get("state", "UNKNOWN")),
            mergeable=str(pull.get("mergeable", "UNKNOWN")),
            head_sha=str(pull.get("headRefOid", "")),
            base_ref=str(pull.get("baseRefName", "main")),
            head_ref=str(pull.get("headRefName") or ""),
            merge_state=str(pull.get("mergeStateStatus") or "UNKNOWN"),
            merge_pending=bool(pull.get("mergeQueueEntry") or pull.get("autoMergeRequest")),
        ),
        checks=checks,
        human_review_ids=human_ids,
        coderabbit_probe=probe,
        coderabbit=observation,
        head_checks=fetch_head_checks(repo, str(pull.get("headRefOid", "")), statuses),
    )


def fetch_coderabbit(repo: str, pr: int) -> CodeRabbitObservation:
    owner, separator, name = repo.partition("/")
    if not separator or not owner or not name:
        return CodeRabbitObservation("degraded", reason="invalid_repo")
    query = """
query($owner:String!,$name:String!,$number:Int!){
  repository(owner:$owner,name:$name){
    pullRequest(number:$number){
      reviewThreads(first:100){
        nodes{isResolved comments(first:20){nodes{databaseId body author{login}}}}
      }
    }
  }
}
"""
    thread_data = gh_json(  # ci-lint: allow GHAPI-001 GraphQL (no ETag; own budget): one gate snapshot per poll, and polls wait on the broker subscription in a session
        "api",
        "graphql",
        "-f",
        f"query={query}",
        "-F",
        f"owner={owner}",
        "-F",
        f"name={name}",
        "-F",
        f"number={pr}",
    )
    comments = gh_json("api", f"repos/{repo}/issues/{pr}/comments?per_page=100")  # ci-lint: allow GHAPI-001 conditional: gh_json sends REST GETs through api_get (session broker ETag, else If-None-Match)
    if not isinstance(thread_data, dict) or not isinstance(comments, list):
        return CodeRabbitObservation("degraded", reason="api_error")
    threads = (
        (((thread_data.get("data") or {}).get("repository") or {}).get("pullRequest") or {})
        .get("reviewThreads", {})
        .get("nodes", [])
    )
    if not isinstance(threads, list):
        return CodeRabbitObservation("degraded", reason="malformed_payload")
    return classify_coderabbit(threads, comments)


def _is_coderabbit_context(context: object) -> bool:
    return isinstance(context, str) and context.strip().lower() == CODERABBIT_STATUS_CONTEXT


def newest_coderabbit_status(statuses: list[dict]) -> dict | None:
    """The newest `CodeRabbit` commit status (highest id), or None."""
    found = [
        s for s in statuses if isinstance(s, dict) and _is_coderabbit_context(s.get("context"))
    ]
    if not found:
        return None
    # The REST list is newest first; the id orders it when present.
    return max(enumerate(found), key=lambda item: (_as_int(item[1].get("id")) or 0, -item[0]))[1]


def fetch_coderabbit_status(repo: str, sha: str) -> tuple[bool, dict | None]:
    """(read ok, newest `CodeRabbit` status) on one commit (#1332)."""
    data = gh_json("api", f"repos/{repo}/commits/{sha}/statuses?per_page=100")  # ci-lint: allow GHAPI-001 conditional: gh_json sends REST GETs through api_get (session broker ETag, else If-None-Match)
    if not isinstance(data, list):
        return False, None
    return True, newest_coderabbit_status(data)


def coderabbit_status_outcome(status: dict) -> str:
    """pending | completed | rate_limited | skipped | <other final state>."""
    state = _lower(status.get("state"))
    if state == "pending":
        return "pending"
    description = str(status.get("description") or "")
    if re.search(r"rate[\s-]*limit", description, re.IGNORECASE):
        return "rate_limited"
    if re.search(r"\bskip", description, re.IGNORECASE):
        return "skipped"
    return "completed" if state == "success" else state or "completed"


def coderabbit_gate_note(
    read_ok: bool, status: dict | None, waited: float, wait_cap: float
) -> str | None:
    """Pure: the note to go green with, or None to keep waiting (#1332).

    A head with no `CodeRabbit` status is `absent` at once: no grace period.
    Without an opt-in wait (`wait_cap <= 0`) the status is reported as it
    stands and nothing is waited for.
    """
    if read_ok and status is not None:
        outcome = coderabbit_status_outcome(status)
        if outcome != "pending":
            return outcome
    elif read_ok:
        return "absent"
    if wait_cap <= 0:
        return "pending" if status is not None else "unreadable"
    if waited >= wait_cap:
        return "timeout"
    return None


_YAML_FALSE = {"false", "no", "off"}


def coderabbit_config_disables_auto_review(text: str) -> bool:
    """Pure: `reviews.auto_review.enabled` is false in a `.coderabbit.yaml`.

    A small reader for exactly that key, block or flow style, so the script
    needs no YAML dependency. Anything it cannot read is "not disabled".
    """
    path: list[tuple[int, str]] = []
    for raw in text.splitlines():
        line = raw.split(" #", 1)[0].rstrip()
        if not line.strip() or line.lstrip().startswith(("#", "-")):
            continue
        indent = len(line) - len(line.lstrip())
        key, sep, value = line.strip().partition(":")
        if not sep:
            continue
        key = key.strip().strip("'\"")
        value = value.strip()
        while path and path[-1][0] >= indent:
            path.pop()
        keys = [k for _, k in path] + [key]
        if keys == ["reviews", "auto_review", "enabled"]:
            return value.strip("'\"").lower() in _YAML_FALSE
        if keys == ["reviews", "auto_review"] and value.startswith("{"):
            flow = re.search(r"\benabled\s*:\s*['\"]?(\w+)", value)
            return bool(flow) and flow.group(1).lower() in _YAML_FALSE
        if not value:
            path.append((indent, key))
    return False


def fetch_coderabbit_suppressed(repo: str, ref: str) -> bool:
    """The repo turns CodeRabbit's auto review off on `ref` (read once per watch).

    A missing file or a failed read is "not suppressed"; with the default
    no-wait policy that still never stalls green.
    """
    for name in CODERABBIT_CONFIG_FILES:
        data = gh_json("api", f"repos/{repo}/contents/{name}?ref={ref}")  # ci-lint: allow GHAPI-001 once per watch: the .coderabbit.yaml read is cached in coderabbit_suppressed
        if not isinstance(data, dict) or not isinstance(data.get("content"), str):
            continue
        try:
            text = base64.b64decode(data["content"]).decode("utf-8", "replace")
        except ValueError:
            continue
        return coderabbit_config_disables_auto_review(text)
    return False


@dataclass
class ReviewState:
    """Tracks new human reviews and actionable CodeRabbit threads."""

    coderabbit_enabled: bool = False
    # CodeRabbit said it skipped this PR: never re-enable it by re-checking.
    coderabbit_skipped: bool = False
    seen_review_ids: set[int] = field(default_factory=set)
    seen_coderabbit_ids: set[int] = field(default_factory=set)
    initialized: bool = False
    last_coderabbit_state: tuple[str, str | None] | None = None

    def update(self, pr: int, repo: str | None, log: WatchLog | None = None) -> bool:
        repo_arg = repo or _resolve_origin_repo()
        if not repo_arg:
            if log:
                log.emit("api_degraded", source="reviews", reason="repo_unresolved")
            return False
        reviews = gh_json("api", f"repos/{repo_arg}/pulls/{pr}/reviews?per_page=100")  # ci-lint: allow GHAPI-001 conditional: gh_json sends REST GETs through api_get (session broker ETag, else If-None-Match)
        human_ids: set[int] = set()
        if isinstance(reviews, list):
            for review in reviews:
                if not isinstance(review, dict):
                    continue
                user = str((review.get("user") or {}).get("login", ""))
                rid = review.get("id")
                if (
                    isinstance(rid, int)
                    and "[bot]" not in user
                    and review.get("state") in {"CHANGES_REQUESTED", "COMMENTED"}
                ):
                    human_ids.add(rid)
        elif log:
            log.emit("api_degraded", source="reviews", reason="api_error")

        observation = fetch_coderabbit(repo_arg, pr) if self.coderabbit_enabled else None
        return self.update_prefetched(frozenset(human_ids), observation, log)

    def update_prefetched(
        self,
        human_ids: frozenset[int],
        observation: CodeRabbitObservation | None,
        log: WatchLog | None = None,
    ) -> bool:
        coderabbit_actionable = False
        if self.coderabbit_enabled and observation is not None:
            signature = (observation.state, observation.reason)
            if signature != self.last_coderabbit_state and log:
                payload: dict[str, object] = {"state": observation.state}
                if observation.reason:
                    payload["reason"] = observation.reason
                if observation.unresolved_threads:
                    payload["unresolved_threads"] = observation.unresolved_threads
                log.emit("coderabbit", coderabbit=payload)
            self.last_coderabbit_state = signature
            if observation.state == "skipped":
                self.coderabbit_enabled = False
                self.coderabbit_skipped = True
            if observation.actionable:
                new_ids = set(observation.ids) - self.seen_coderabbit_ids
                coderabbit_actionable = bool(new_ids) or not self.initialized
                self.seen_coderabbit_ids.update(observation.ids)

        if not self.initialized:
            self.seen_review_ids = set(human_ids)
            self.initialized = True
            return coderabbit_actionable
        new_human = set(human_ids) - self.seen_review_ids
        self.seen_review_ids = set(human_ids)
        return bool(new_human) or coderabbit_actionable


# ---------- cancellation ------------------------------------------------------


@dataclass
class CancelOptions:
    on: set[str]
    mode: str  # runs | jobs | none
    timeout: int
    require: bool
    dry_run: bool
    ignore_permission_errors: bool
    no_retry: bool
    # The head SHA the watch started on (#1418); runs on any other SHA are
    # never cancelled, so an orphaned watch cannot kill a fix's fresh CI.
    pinned_sha: str | None = None


def cancel_pr_runs(  # noqa: C901
    pr: int,
    repo: str | None,
    head_sha: str,
    opts: CancelOptions,
    log: WatchLog | None = None,
    *,
    scope: dict[str, int] | None = None,
) -> int:
    """Cancel non-completed workflow runs on the PR's head SHA.

    `scope` (workflow key -> failing run id) limits cancellation to the
    failing run itself and runs of its workflow provably created before it:
    never a newer run, never another workflow (#1330), and never a run whose
    order is ambiguous, such as one created in the same second (#1710).

    Returns the number of cancel attempts. Failures are surfaced as
    `CANCEL <id> status=…` lines on stdout.
    """
    if opts.mode == "none":
        return 0
    if not head_sha:
        if log:
            log.emit("cancel_item", status="skipped", reason="head_sha_missing")
        return 0
    repo_arg = repo if repo else _resolve_origin_repo()
    if not repo_arg:
        print(f"CANCEL  skipped: could not resolve repo for PR #{pr}")
        if log:
            log.emit("cancel_item", status="skipped", reason="repo_unresolved")
        return 0
    runs = paginate(f"repos/{repo_arg}/actions/runs?head_sha={head_sha}", "workflow_runs")
    if runs is None:
        if log:
            log.emit("api_degraded", source="cancel_runs", reason="fetch_failed")
        return 0
    by_id = {r["id"]: r for r in runs if isinstance(r, dict) and isinstance(r.get("id"), int)}
    attempts = 0
    for r in runs:
        if not isinstance(r, dict):
            continue
        rid = r.get("id")
        status = r.get("status", "")
        if status in {"completed", "cancelled"} or not isinstance(rid, int):
            continue
        run_head_sha = r.get("head_sha")
        if not isinstance(run_head_sha, str) or run_head_sha != head_sha:
            if log:
                log.emit(
                    "cancel_item",
                    mode="runs",
                    run_id=rid,
                    status="skipped",
                    reason=("head_sha_mismatch" if run_head_sha else "head_sha_missing"),
                )
            continue
        if scope is not None:
            limit = scope.get(_workflow_key(r))
            if limit is None or not (rid == limit or _created_before(r, by_id.get(limit))):
                if log:
                    log.emit(
                        "cancel_item",
                        mode=opts.mode,
                        run_id=rid,
                        status="skipped",
                        reason="out_of_scope",
                    )
                continue
            if rid == limit and _keep_failing_run(repo_arg, rid, log, opts.mode):
                continue
        if opts.mode == "runs":
            attempts += 1
            if opts.dry_run:
                print(f"CANCEL  workflow_run={rid} status=DRY-RUN")
                if log:
                    log.emit("cancel_item", mode="runs", run_id=rid, status="dry_run")
                continue
            cancel = gh("api", "-X", "POST", f"repos/{repo_arg}/actions/runs/{rid}/cancel")
            _report_cancel(rid, cancel, opts, log, "runs")
        elif opts.mode == "jobs":
            jobs_resp = gh_json("api", f"repos/{repo_arg}/actions/runs/{rid}/jobs?per_page=100")  # ci-lint: allow GHAPI-001 on exit only: the jobs of a run being cancelled
            if not isinstance(jobs_resp, dict):
                if log:
                    log.emit(
                        "api_degraded",
                        source="cancel_jobs",
                        reason="fetch_failed",
                        run_id=rid,
                    )
                continue
            jobs = jobs_resp.get("jobs", [])
            if not isinstance(jobs, list):
                if log:
                    log.emit(
                        "api_degraded",
                        source="cancel_jobs",
                        reason="malformed_payload",
                        run_id=rid,
                    )
                continue
            for j in jobs:
                if not isinstance(j, dict) or j.get("status") == "completed":
                    continue
                jid = j.get("id")
                if not isinstance(jid, int):
                    continue
                attempts += 1
                if opts.dry_run:
                    print(f"CANCEL  job={jid} status=DRY-RUN")
                    if log:
                        log.emit("cancel_item", mode="jobs", item_id=jid, status="dry_run")
                    continue
                cancel = gh("api", "-X", "POST", f"repos/{repo_arg}/actions/jobs/{jid}/cancel")
                _report_cancel(jid, cancel, opts, log, "jobs")
    return attempts


def _keep_failing_run(repo: str, run_id: int, log: WatchLog | None, mode: str) -> bool:
    """True when the failing run must not be cancelled (obs-rust/obs-studio#13).

    GitHub cancels whole runs only; it has no per-job cancel. Cancelling the
    failing run would throw away every sibling job still in progress (a full
    build an hour in) along with its timing and result. So the failing run is
    cancelled only when nothing in it is running any more: its remaining jobs
    are all queued or waiting, which costs nothing to drop. When its jobs
    cannot be read, it is kept: a lost result is not recoverable, CI minutes
    are.
    """
    jobs = paginate(f"repos/{repo}/actions/runs/{run_id}/jobs", "jobs")
    if jobs is None:
        reason = "jobs_unreadable"
    elif any(_lower(j.get("status")) == "in_progress" for j in jobs):
        reason = "jobs_in_progress"
    else:
        return False
    print(f"CANCEL  workflow_run={run_id} status=skipped  ({reason}: in-flight results kept)")
    if log:
        log.emit("cancel_item", mode=mode, run_id=run_id, status="skipped", reason=reason)
    return True


def _report_cancel(
    item_id: int,
    res: GhResult,
    opts: CancelOptions,
    log: WatchLog | None,
    mode: str,
) -> None:
    if res.ok:
        print(f"CANCEL  id={item_id} status=cancelled")
        if log:
            log.emit("cancel_item", mode=mode, item_id=item_id, status="cancelled")
        return
    stderr = res.stderr or ""
    err = stderr.strip().splitlines()
    err_first = err[0] if err else "unknown"
    if "HTTP 403" in stderr or "Resource not accessible" in stderr:
        print(f"CANCEL  id={item_id} status=permission_denied  ({err_first})")
        if log:
            log.emit(
                "cancel_item",
                mode=mode,
                item_id=item_id,
                status="permission_denied",
                required=opts.require,
            )
    elif "HTTP 404" in stderr or "HTTP 409" in stderr or "HTTP 422" in stderr:
        # 409: another watcher on the same PR already cancelled it (#1332).
        print(f"CANCEL  id={item_id} status=already_completed")
        if log:
            log.emit("cancel_item", mode=mode, item_id=item_id, status="already_completed")
    else:
        print(f"CANCEL  id={item_id} status=error  ({err_first})")
        if log:
            log.emit(
                "cancel_item",
                mode=mode,
                item_id=item_id,
                status="error",
                required=opts.require,
            )


_GITHUB_REMOTE = re.compile(
    r"^(?:[a-z][a-z0-9+.-]*://)?(?:[^@/\s]+@)?github\.com(?::\d+)?[:/]"
    r"([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+?)(?:\.git)?/?$",
    re.IGNORECASE,
)


def github_repo_from_url(url: str) -> str | None:
    """`owner/name` of a github.com remote URL (https, ssh, scp-style), or None."""
    match = _GITHUB_REMOTE.match(url.strip())
    return f"{match.group(1)}/{match.group(2)}" if match else None


_GITHUB_PR_URL = re.compile(
    r"^https?://github\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)/pull/(\d+)(?:[/?#].*)?$",
    re.IGNORECASE,
)


def github_pr_from_url(url: str) -> tuple[str, int] | None:
    """(`owner/name`, number) of a github.com pull-request URL, or None."""
    match = _GITHUB_PR_URL.match(url.strip())
    return (f"{match.group(1)}/{match.group(2)}", int(match.group(3))) if match else None


def _git_origin_url() -> str | None:
    return _git_line("remote", "get-url", "origin")


def _git_line(*args: str) -> str | None:
    """The trimmed stdout of a git command, or None when it fails."""
    try:
        res = RunningProcess.run(
            ["git", *args],
            capture_output=True,
            stderr=PIPE,
            text=True,
            timeout=GH_CALL_TIMEOUT_SEC,
        )
    except (OSError, TimeoutError, TimeoutExpired):
        return None
    if res.returncode != 0:
        return None
    return (res.stdout or "").strip() or None


def _resolve_origin_repo() -> str | None:
    """The repository a watch without `--repo` targets (#1741).

    `origin` wins: a bare `gh pr view N` in a fork without `gh repo
    set-default` resolves to the *parent*, which watched nektos/act#1 instead
    of zackees/act2#1. gh's default repo is asked only when origin is not a
    github.com URL.
    """
    origin = _git_origin_url()
    repo = github_repo_from_url(origin) if origin else None
    if repo:
        return repo
    res = gh("repo", "view", "--json", "nameWithOwner")
    if not res.ok:
        return None
    try:
        return json.loads(res.stdout).get("nameWithOwner")
    except json.JSONDecodeError:
        return None


# ---------- main poll loop ----------------------------------------------------


def resolve_pr_selector(
    selector: str | None, repo: str | None
) -> tuple[str, int] | tuple[int, str]:
    """(`owner/name`, PR number) for a PR selector, or (exit code, why).

    The selector is what `gh pr checks` accepts: a number, a branch, a PR URL,
    or nothing for the current branch. A URL names its own repository. Every
    other selector resolves on `--repo`, else on origin (#1741): a bare
    `gh pr view <branch>` in a fork looks the branch up on the parent.
    """
    if selector:
        from_url = github_pr_from_url(selector)
        if from_url:
            return from_url
    repo = repo or _resolve_origin_repo()
    if not repo:
        return EXIT_GITHUB_UNREACHABLE, "no repository: pass --repo or add an `origin` remote"
    if selector and selector.isdigit():
        return repo, int(selector)
    branch = selector or _git_line("rev-parse", "--abbrev-ref", "HEAD")
    if not branch or branch == "HEAD":
        return EXIT_USAGE, "no PR selector and no current branch (detached HEAD?)"
    res = gh("pr", "view", branch, "--repo", repo, "--json", "number", "--jq", ".number")
    number = res.stdout.strip()
    if not res.ok or not number.isdigit():
        detail = res.stderr.strip() or f"gh returned {number[:80]!r}"
        return EXIT_GITHUB_UNREACHABLE, f"no PR for {branch!r} in {repo}: {detail}"
    return repo, int(number)


def _exit_after_cancel(
    code: int,
    on_label: str,
    pr: int,
    repo: str | None,
    head_sha: str,
    opts: CancelOptions,
    log: WatchLog | None = None,
) -> None:
    """Cancel (if enabled) then exit `code`."""
    _cancel_for_exit(on_label, pr, repo, head_sha, opts, log)
    _finish_exit(code, on_label, log)


def _cancel_for_exit(
    on_label: str,
    pr: int,
    repo: str | None,
    head_sha: str,
    opts: CancelOptions,
    log: WatchLog | None = None,
    scope: dict[str, int] | None = None,
) -> None:
    if on_label in opts.on or "always" in opts.on:
        if opts.pinned_sha and head_sha != opts.pinned_sha:
            print(
                f"NOTE  head moved from {opts.pinned_sha[:7]} to {head_sha[:7]}; "
                "not cancelling runs this watch did not start on",
                file=sys.stderr,
            )
            if log:
                log.emit(
                    "cancel_skipped", reason="head_moved", pinned=opts.pinned_sha, head=head_sha
                )
            return
        if scope is None:
            attempts = cancel_pr_runs(pr, repo, head_sha, opts, log)
        else:
            attempts = cancel_pr_runs(pr, repo, head_sha, opts, log, scope=scope)
        if log:
            log.emit("cancel", trigger=on_label, attempts=attempts, mode=opts.mode)


def _finish_exit(code: int, reason: str, log: WatchLog | None = None, **fields: object) -> None:
    if log:
        log.emit("EXIT", code=code, reason=reason, **fields)
        log.close()
    sys.exit(code)


def _exit_unreachable(why: str, log: WatchLog | None) -> None:
    detail = LAST_GH_ERROR or "(no gh stderr captured)"
    print(f"GITHUB-UNREACHABLE  {why}: {detail}", file=sys.stderr)
    _finish_exit(EXIT_GITHUB_UNREACHABLE, "github_unreachable", log, why=why, stderr=detail)


class WatchKilled(Exception):
    """SIGINT/SIGTERM arrived; `main` writes the final `EXIT` event."""

    def __init__(self, signum: int) -> None:
        super().__init__(f"signal {signum}")
        self.signum = signum
        self.code = 128 + signum


def install_kill_handlers() -> None:
    """On SIGINT/SIGTERM unwind to `main`, which writes a final `EXIT` event
    and cancels nothing: the caller that killed the watch did not ask for
    cancellation (#1418)."""

    def on_signal(signum: int, _frame: object) -> None:
        raise WatchKilled(int(signum))

    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, on_signal)


def fetch_workflow_presence(repo: str, ref: str) -> bool | None:
    """True/False when the base branch does/doesn't have workflow files;
    None when GitHub could not be asked."""
    res = gh("api", f"repos/{repo}/contents/.github/workflows?ref={ref}")  # ci-lint: allow GHAPI-001 once per head commit: memoized in no_checks_reasons
    if not res.ok:
        return False if "HTTP 404" in res.stderr else None
    try:
        entries = json.loads(res.stdout)
    except json.JSONDecodeError:
        return None
    if not isinstance(entries, list):
        return None
    return any(
        isinstance(e, dict) and str(e.get("name", "")).endswith((".yml", ".yaml"))
        for e in entries
    )


def fetch_head_commit_message(repo: str, sha: str) -> str | None:
    data = gh_json("api", f"repos/{repo}/commits/{sha}")  # ci-lint: allow GHAPI-001 once per head commit: memoized in no_checks_reasons
    if not isinstance(data, dict):
        return None
    message = (data.get("commit") or {}).get("message")
    return message if isinstance(message, str) else None


PR_TRIGGER_EVENTS = ("pull_request", "pull_request_target")


def _branch_glob(pattern: str) -> re.Pattern[str]:
    """GitHub's branch filter glob: `**` crosses `/`, `*` does not."""
    out = []
    i = 0
    while i < len(pattern):
        if pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif pattern[i] == "*":
            out.append("[^/]*")
            i += 1
        elif pattern[i] == "?":
            out.append(".?")
            i += 1
        else:
            out.append(re.escape(pattern[i]))
            i += 1
    return re.compile("".join(out) + r"\Z")


def branch_filter_matches(
    branches: list[str] | None, ignore: list[str] | None, base: str
) -> bool:
    """Does a `pull_request` filter admit a PR into `base`?

    `branches` patterns apply in order, a `!` pattern excluding what earlier
    ones matched; `branches-ignore` excludes any match."""
    if ignore is not None:
        return not any(_branch_glob(p).match(base) for p in ignore)
    if branches is None:
        return True
    admitted = False
    for pattern in branches:
        if pattern.startswith("!"):
            if _branch_glob(pattern[1:]).match(base):
                admitted = False
        elif _branch_glob(pattern).match(base):
            admitted = True
    return admitted


def _yaml_scalar(text: str) -> str:
    return text.strip().strip("'\"")


def _yaml_list(rest: str, lines: list[str], start: int, indent: int) -> list[str]:
    """An inline `[a, b]` list, or the `- item` lines indented under a key."""
    rest = rest.split(" #", 1)[0].strip()
    if rest.startswith("["):
        return [_yaml_scalar(item) for item in rest.strip("[]").split(",") if item.strip()]
    if rest:
        return [_yaml_scalar(rest)]
    items = []
    for line in lines[start:]:
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if len(line) - len(line.lstrip()) <= indent:
            break
        if stripped.startswith("- "):
            items.append(_yaml_scalar(stripped[2:]))
    return items


def _block_children(lines: list[str], start: int, indent: int) -> list[tuple[int, str, str]]:
    """`(index, key, rest)` for the keys one level under the line at `start`."""
    children = []
    child_indent = None
    for index in range(start + 1, len(lines)):
        line = lines[index]
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        current = len(line) - len(line.lstrip())
        if current <= indent:
            break
        if child_indent is None:
            child_indent = current
        if current == child_indent and ":" in stripped and not stripped.startswith("- "):
            key, _, rest = stripped.partition(":")
            children.append((index, _yaml_scalar(key), rest))
    return children


def workflow_triggers(text: str) -> dict[str, dict[str, list[str] | None]] | None:
    """`{event: {"branches": [...]|None, "branches-ignore": [...]|None}}`.

    A line scanner for the common `on:` shapes; `None` when the shape is not
    recognized, which callers treat as "may trigger" so an odd workflow can
    never produce a false `no_trigger_for_base`."""
    lines = text.splitlines()
    for index, line in enumerate(lines):
        match = re.match(r"""^(['"]?)(on|true)\1\s*:(.*)$""", line)
        if not match:
            continue
        rest = match.group(3).split(" #", 1)[0].strip()
        if rest:
            names = _yaml_list(rest, lines, index + 1, 0)
            return {name: {"branches": None, "branches-ignore": None} for name in names}
        events: dict[str, dict[str, list[str] | None]] = {}
        for child, event, _rest in _block_children(lines, index, 0):
            child_indent = len(lines[child]) - len(lines[child].lstrip())
            spec: dict[str, list[str] | None] = {"branches": None, "branches-ignore": None}
            for grand, key, grand_rest in _block_children(lines, child, child_indent):
                if key in spec:
                    grand_indent = len(lines[grand]) - len(lines[grand].lstrip())
                    spec[key] = _yaml_list(grand_rest, lines, grand + 1, grand_indent)
            events[event] = spec
        return events or None
    return None


def triggers_for_base(workflows: dict[str, str], base: str) -> dict[str, list[str]]:
    """Which workflows a PR into `base` triggers, and which can be dispatched."""
    pr_workflows: list[str] = []
    dispatchable: list[str] = []
    for name, text in sorted(workflows.items()):
        events = workflow_triggers(text)
        if events is None:
            pr_workflows.append(name)
            continue
        if "workflow_dispatch" in events:
            dispatchable.append(name)
        for event in PR_TRIGGER_EVENTS:
            spec = events.get(event)
            if spec is not None and branch_filter_matches(
                spec["branches"], spec["branches-ignore"], base
            ):
                pr_workflows.append(name)
                break
    return {"pr_workflows": pr_workflows, "dispatchable": dispatchable}


def fetch_workflow_texts(repo: str, ref: str) -> dict[str, str] | None:
    """`{file name: YAML text}` for `.github/workflows` at `ref`; None if unreadable."""
    res = gh("api", f"repos/{repo}/contents/.github/workflows?ref={ref}")  # ci-lint: allow GHAPI-001 once per head commit: memoized in no_checks_reasons
    if not res.ok:
        return None
    try:
        entries = json.loads(res.stdout)
    except json.JSONDecodeError:
        return None
    if not isinstance(entries, list):
        return None
    texts: dict[str, str] = {}
    for entry in entries:
        name = str(entry.get("name", "")) if isinstance(entry, dict) else ""
        if not name.endswith((".yml", ".yaml")):
            continue
        data = gh_json("api", f"repos/{repo}/contents/.github/workflows/{name}?ref={ref}")  # ci-lint: allow GHAPI-001 once per head commit: memoized in no_checks_reasons
        content = data.get("content") if isinstance(data, dict) else None
        if not isinstance(content, str):
            return None
        texts[name] = base64.b64decode(content).decode("utf-8", errors="replace")
    return texts


def immediate_no_checks_reason(repo: str, snapshot: PRSnapshot) -> str | None:
    """Why no check can ever report on this head, known without waiting."""
    # A PR adding the repo's first workflow runs it from its own ref, so the
    # repo has no workflows only when neither side does.
    if (
        fetch_workflow_presence(repo, snapshot.base_ref) is False
        and fetch_workflow_presence(repo, snapshot.head_sha) is False
    ):
        return "no_workflows"
    # #1893: workflows exist, but none triggers on a PR into this base (a
    # `branches:` filter). A re-push cannot help, so say so at once.
    texts = fetch_workflow_texts(repo, snapshot.head_sha)
    if texts and not triggers_for_base(texts, snapshot.base_ref)["pr_workflows"]:
        return "no_trigger_for_base"
    message = fetch_head_commit_message(repo, snapshot.head_sha)
    if message and SKIP_CI_MARKER.search(message):
        return "skip_ci_marker"
    return None


def fetch_queued_jobs(repo: str, run_id: int) -> list[dict]:
    data = gh_json("api", f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100")  # ci-lint: allow GHAPI-001 conditional: gh_json sends REST GETs through api_get (session broker ETag, else If-None-Match)
    jobs = data.get("jobs") if isinstance(data, dict) else None
    return [
        {"name": str(j.get("name", "?")), "labels": list(j.get("labels") or [])}
        for j in jobs or []
        if isinstance(j, dict) and _lower(j.get("status")) in QUEUED_STATUSES
    ]


QUEUE_WARN_SEC = 600  # 10 min queued = surface a warning (#440)
STEP_WARN_SEC = 300  # 5 min on the same step = possible stuck (#440)


def _now_epoch_ms() -> int:
    return int(time.time() * 1000)


def _parse_iso(ts: str | None) -> float | None:
    """Parse an RFC3339 / ISO-8601 timestamp into epoch seconds. Returns
    None on any malformed input."""
    if not ts:
        return None
    try:
        # Strip trailing Z; datetime.fromisoformat handles offsets but
        # not the 'Z' suffix until Python 3.11.
        ts = ts.replace("Z", "+00:00")
        from datetime import datetime

        return datetime.fromisoformat(ts).timestamp()
    except (ValueError, TypeError):
        return None


def fetch_run_jobs(run_id: str, repo: str | None) -> dict | None:
    """One run and its jobs, in the shape `gh run view --json
    jobs,status,conclusion,createdAt,updatedAt,workflowName` prints, read
    through two REST calls the session broker revalidates by ETag (the
    porcelain view is never brokered). None on any error."""
    if not repo:
        return None
    run = gh_json("api", f"repos/{repo}/actions/runs/{run_id}")  # ci-lint: allow GHAPI-001 conditional: gh_json sends REST GETs through api_get (session broker ETag, else If-None-Match)
    jobs = paginate(f"repos/{repo}/actions/runs/{run_id}/jobs", "jobs")
    if not isinstance(run, dict) or jobs is None:
        return None

    def step(raw: dict) -> dict:
        return {
            "name": raw.get("name"),
            "number": raw.get("number"),
            "status": raw.get("status"),
            "conclusion": raw.get("conclusion"),
            "startedAt": raw.get("started_at"),
            "completedAt": raw.get("completed_at"),
        }

    return {
        "status": run.get("status"),
        "conclusion": run.get("conclusion") or "",
        "createdAt": run.get("created_at"),
        "updatedAt": run.get("updated_at"),
        "workflowName": run.get("name"),
        "jobs": [
            {
                "name": job.get("name"),
                "status": job.get("status"),
                "conclusion": job.get("conclusion") or "",
                "startedAt": job.get("started_at"),
                "completedAt": job.get("completed_at"),
                "steps": [step(raw) for raw in job.get("steps") or [] if isinstance(raw, dict)],
            }
            for job in jobs
            if isinstance(job, dict)
        ],
    }


def aggregate_jobs(run_info: dict) -> dict:
    """Compute per-poll aggregate stats + per-job rows from a
    `gh run view --json jobs` payload."""
    jobs = run_info.get("jobs", []) or []
    counts = {"total": len(jobs), "queued": 0, "in_progress": 0, "completed": 0, "failed": 0}
    current: list[dict] = []
    warnings: list[str] = []
    now = time.time()
    for j in jobs:
        status = j.get("status") or ""
        conclusion = j.get("conclusion") or ""
        if status == "queued":
            counts["queued"] += 1
            started = _parse_iso(j.get("startedAt") or j.get("createdAt"))
            if started and (now - started) > QUEUE_WARN_SEC:
                warnings.append(
                    f"{j.get('name', '?')} has been queued for "
                    f"{int(now - started)}s (> {QUEUE_WARN_SEC}s threshold)"
                )
        elif status == "in_progress":
            counts["in_progress"] += 1
            current.append(_summarize_job(j, now, warnings))
        elif status == "completed":
            counts["completed"] += 1
            if conclusion in {"failure", "cancelled", "timed_out"}:
                counts["failed"] += 1
    total = counts["total"] or 1  # avoid div-by-zero
    percent = round((counts["completed"] / total) * 100)
    return {
        "counts": counts,
        "percent_complete": percent,
        "current_jobs": current,
        "warnings": warnings,
    }


def _summarize_job(j: dict, now: float, warnings: list[str]) -> dict:
    name = j.get("name", "?")
    started = _parse_iso(j.get("startedAt"))
    elapsed_sec = int(now - started) if started else None
    current_step = None
    for step in j.get("steps", []) or []:
        if step.get("status") == "in_progress":
            step_started = _parse_iso(step.get("startedAt"))
            step_elapsed = int(now - step_started) if step_started else None
            current_step = {
                "name": step.get("name", "?"),
                "number": step.get("number"),
                "started_at": step.get("startedAt"),
                "elapsed_sec": step_elapsed,
            }
            if step_elapsed and step_elapsed > STEP_WARN_SEC:
                warnings.append(
                    f"{name}: step '{step.get('name', '?')}' has held "
                    f"for {step_elapsed}s (> {STEP_WARN_SEC}s threshold)"
                )
            break
    return {
        "name": name,
        "started_at": j.get("startedAt"),
        "elapsed_sec": elapsed_sec,
        "current_step": current_step,
    }


# `--quiet` (#2368): no per-poll progress lines, only the final verdict.
QUIET = False


def emit_progress_report(
    pr: int, repo: str | None, snapshot: PRSnapshot, checks: list[CheckRow]
) -> None:
    """Emit a per-poll progress report covering active workflow runs
    associated with the PR's head SHA.

    JSONL line on stdout (schema-versioned), human-readable per-job
    rows on stderr. `--quiet` suppresses it (#2368)."""
    if QUIET:
        return
    seen_runs: set[str] = set()
    for c in checks:
        if c.bucket != "pending":
            continue
        run_id = _extract_run_id_from_link(c.link)
        if not run_id or run_id in seen_runs:
            continue
        seen_runs.add(run_id)
        info = fetch_run_jobs(run_id, repo)
        if not info:
            continue
        agg = aggregate_jobs(info)
        report = {
            "v": 1,
            "ts_ms": _now_epoch_ms(),
            "pr": pr,
            "run_id": run_id,
            "sha": snapshot.head_sha[:7] if snapshot.head_sha else None,
            "workflow": info.get("workflowName"),
            "status": info.get("status"),
            "conclusion": info.get("conclusion") or None,
            "jobs": agg["counts"],
            "percent_complete": agg["percent_complete"],
            "current_jobs": agg["current_jobs"],
            "warnings": agg["warnings"],
        }
        print(json.dumps(report))
        # Human-readable rows on stderr so stdout stays machine-parseable.
        c1 = agg["counts"]
        print(
            f"  {info.get('workflowName', '?')} run {run_id} "
            f"sha={(snapshot.head_sha or '?')[:7]} "
            f"status={info.get('status')} "
            f"{c1['completed']}/{c1['total']} jobs done "
            f"({agg['percent_complete']}%)",
            file=sys.stderr,
        )
        for j in agg["current_jobs"]:
            step = j.get("current_step") or {}
            step_str = step.get("name", "—") if step else "—"
            print(
                f"    {j['name']:<24} in_progress  "
                f"{step_str:<22} "
                f"{j.get('elapsed_sec', '—')}s "
                f"step elapsed {step.get('elapsed_sec', '—')}s",
                file=sys.stderr,
            )
        for w in agg["warnings"]:
            print(f"    WARN: {w}", file=sys.stderr)


# --- Session read-broker subscription (#1743 phase 3) ----------------------
#
# Inside a clud session whose gh reads go through the daemon's read broker,
# the watch blocks on `POST /gh/watch` between polls instead of sleeping: the
# broker re-reads the PR's REST state at its own cadence (its TTL, shared by
# every reader, free `304`s while nothing changed) and returns the moment a
# body changes. The poll that follows judges exactly as before. Outside a
# session, or when the daemon does not answer, the watch polls as it always
# did.

# Mirrors `gh_broker::FORWARDED_ENV` (crates/clud-bin/src/gh_broker/mod.rs):
# the identity, and so the cache keys, of this process's brokered reads.
# tests/test_pr_merge_watch_subscription.py fails if the two drift.
BROKER_FORWARDED_ENV = (
    "GH_HOST",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_CONFIG_DIR",
    "XDG_CONFIG_HOME",
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
)
BROKER_WATCH_PATH = "/gh/watch"
# One daemon call blocks at most this long (the daemon caps it at 55 s).
BROKER_WAIT_SEC = 50
# Even with nothing changed, a full poll runs at least every this many
# intervals: GraphQL-only state (review threads, mergeability) has no REST
# key to watch.
SUBSCRIPTION_HEARTBEAT_POLLS = 6


def broker_watch_keys(repo: str, pr: int, head_sha: str) -> list[str]:
    """The REST reads whose change should wake the watch, kept to the ones
    that carry verdicts, since every key costs a (free) revalidation per
    TTL: the PR (state, head, mergeability, and its `updated_at` moves with
    review and comment activity), the head commit's check runs (at the URL
    `paginate` reads first, so its first baseline is of the very body the
    poll judged) and its commit statuses. Anything else a poll reads only
    through GraphQL (review threads, CodeRabbit's comments) is caught at the
    heartbeat. Later waits keep the digests of the last wake, so a change
    during a poll wakes the next wait at once."""
    return [
        f"repos/{repo}/pulls/{pr}",
        f"repos/{repo}/commits/{head_sha}/check-runs?filter=all&per_page={PER_PAGE}&page=1",
        f"repos/{repo}/commits/{head_sha}/statuses?per_page={PER_PAGE}",
    ]


@dataclass
class BrokerSubscription:
    """A connection to the session daemon's `/gh/watch` route."""

    url: str
    token: str
    gh: str
    env: list[list[str]]
    session_id: str | None
    keys: list[str] | None = None
    seen: list[str | None] | None = None
    deferred_until: int | None = None

    @classmethod
    def from_env(cls, environ: dict[str, str] | None = None) -> BrokerSubscription | None:
        """The session's broker, or None outside a session that uses one."""
        environ = dict(os.environ if environ is None else environ)
        gh_target = environ.get("CLUD_GH_SHIM_TARGET", "")
        if environ.get("CLUD_GH_READ_BROKER") != "1" or not gh_target:
            return None
        state = environ.get("CLUD_DAEMON_STATE_DIR") or str(Path.home() / ".clud" / "state")
        try:
            info = json.loads((Path(state) / "daemon.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
        port = info.get("dashboard_port") if isinstance(info, dict) else None
        token = info.get("dashboard_token") if isinstance(info, dict) else None
        if not isinstance(port, int) or not isinstance(token, str) or not token:
            return None
        return cls(
            url=f"http://127.0.0.1:{port}{BROKER_WATCH_PATH}",
            token=token,
            gh=gh_target,
            env=[[key, environ[key]] for key in BROKER_FORWARDED_ENV if key in environ],
            session_id=environ.get("CLUD_SESSION_ID") or None,
        )

    def _call(self, wait_sec: float) -> dict | None:
        body = {
            "gh": self.gh,
            "hostname": None,
            "env": self.env,
            "session_id": self.session_id,
            "endpoints": self.keys,
            "seen": self.seen or [],
            "wait_ms": int(max(0.0, wait_sec) * 1000),
        }
        request = urllib.request.Request(
            self.url,
            data=json.dumps(body).encode(),
            headers={
                "Content-Type": "application/json",
                "Cookie": f"clud_dashboard_token={self.token}",
            },
            method="POST",
        )
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        try:
            with opener.open(request, timeout=wait_sec + 15) as response:
                reply = json.loads(response.read())
        except (OSError, ValueError):
            return None
        if not isinstance(reply, dict) or not isinstance(reply.get("digests"), list):
            return None
        return reply

    def wait(self, keys: list[str], wait_sec: float) -> str:
        """Block until a watched read changes or `wait_sec` runs out.

        Returns `changed`, `quiet` (nothing changed), or `unavailable` (the
        daemon did not answer; the caller sleeps as it would without a
        broker). A new key set (the head moved) first takes a baseline.
        """
        if keys != self.keys:
            self.keys, self.seen = list(keys), None
            baseline = self._call(0)
            if baseline is None:
                return "unavailable"
            self.seen = baseline["digests"]
        deadline = time.monotonic() + wait_sec
        while (left := deadline - time.monotonic()) > 0:
            reply = self._call(min(left, BROKER_WAIT_SEC))
            if reply is None:
                return "unavailable"
            self.seen = reply["digests"]
            self.deferred_until = reply.get("deferred_until_s")
            if reply.get("changed"):
                return "changed"
        return "quiet"


def _await_next_poll(
    subscription: BrokerSubscription | None,
    keys: list[str],
    poll_started: float,
    interval: int,
    deadline: float,
    log: WatchLog | None,
) -> None:
    """Between two steady-state polls: block on the broker until a watched
    read changes (at most a heartbeat, never past the deadline), else sleep
    the rest of the interval as before."""
    if subscription is not None:
        wait = min(interval * SUBSCRIPTION_HEARTBEAT_POLLS, deadline - time.monotonic())
        outcome = subscription.wait(keys, max(0.0, wait))
        if log:
            log.emit(
                "subscription",
                outcome=outcome,
                deferred_until_s=subscription.deferred_until,
            )
        if outcome != "unavailable":
            return
    _sleep_remaining_interval(poll_started, interval)


def _sleep_remaining_interval(poll_started: float, interval: int) -> None:
    remaining = max(0.0, interval - (time.monotonic() - poll_started))
    if remaining:
        time.sleep(remaining)


def watch(  # noqa: C901
    pr: int,
    repo: str | None,
    interval: int,
    timeout: int,
    require_pattern: str | None,
    opts: CancelOptions,
    log: WatchLog | None = None,
    *,
    no_checks_grace: int = DEFAULT_NO_CHECKS_GRACE_SEC,
    max_queued: int | None = None,
    coderabbit_wait: int = DEFAULT_CODERABBIT_WAIT_SEC,
    broker_wait: bool = True,
) -> int:
    deadline = time.monotonic() + timeout
    snapshot: PRSnapshot | None = None
    required_names: set[str] | None = None
    # Resolve the repository once, up front, and pass it to every gh call: a
    # bare call in a fork resolves to the parent repository (#1741). Without
    # a repository every poll would fail, so that is fatal (#1418).
    repo = repo or _resolve_origin_repo()
    if not repo:
        _exit_unreachable("no repository: pass --repo or add an `origin` remote", log)
        return EXIT_GITHUB_UNREACHABLE
    # Initial snapshot — bail fast if the PR isn't open. A failed fetch is
    # GitHub being unreachable, never "closed" (#1418).
    for attempt in range(1, MAX_CONSECUTIVE_API_FAILURES + 1):  # ci-lint: allow GHAPI-001 bounded retry: at most MAX_CONSECUTIVE_API_FAILURES (3) tries for the first snapshot
        snapshot = PRSnapshot.fetch(pr, repo)
        if snapshot is not None:
            break
        print(f"ERROR  could not fetch PR #{pr} (attempt {attempt})", file=sys.stderr)
        if log:
            log.emit("api_degraded", source="pr_snapshot", reason="fetch_failed")
        if attempt < MAX_CONSECUTIVE_API_FAILURES:
            time.sleep(interval)
    if snapshot is None:
        _exit_unreachable(f"could not fetch PR #{pr}", log)
        return EXIT_GITHUB_UNREACHABLE
    if snapshot.state in {"MERGED", "CLOSED"}:
        print(f"PR-STATE  #{pr} state={snapshot.state}")
        if log:
            log.emit("pr_state", state=snapshot.state, head_sha=snapshot.head_sha)
        code = EXIT_GREEN if snapshot.state == "MERGED" else EXIT_PR_CLOSED
        label = "merged" if snapshot.state == "MERGED" else "closed"
        _exit_after_cancel(code, label, pr, repo, snapshot.head_sha, opts, log)

    # Resolve required check names (best-effort).
    repo_for_protection = repo
    required_names = fetch_required_check_names(repo_for_protection, snapshot.base_ref)
    opts = replace(opts, pinned_sha=snapshot.head_sha)
    seen_head = snapshot.head_sha
    # Empty-rollup bookkeeping (#1418), reset whenever the head moves.
    idle_since: float | None = None
    no_checks_reasons: dict[str, str | None] = {}
    api_failures = 0
    # Consecutive polls whose rollup showed a required failure but no REST
    # check-run data to judge it by (#1742).
    unjudged_polls = 0
    unknown_polls = 0
    queue_announced = False

    review_state = ReviewState()
    coderabbit_probe_complete = False
    poll_no = 0
    # CodeRabbit gate (#1332): when required CI went green on this head, and
    # the green that is only waiting for CodeRabbit to finish the head commit.
    green_since: float | None = None
    waiting_green: tuple[list[CheckRow], dict[str, int]] | None = None
    last_wait_state: str | None = None
    # `.coderabbit.yaml` turns auto review off: read once, only if CodeRabbit
    # shows up at all (None = not read yet).
    coderabbit_suppressed: bool | None = None
    keep_coderabbit_status = bool(
        required_names and any(_is_coderabbit_context(name) for name in required_names)
    )

    require_re = re.compile(require_pattern) if require_pattern else None
    # In a clud session with the read broker, wait on it between polls
    # instead of sleeping (#1743 phase 3).
    subscription = BrokerSubscription.from_env() if broker_wait else None
    if log:
        log.emit("subscription", outcome="available" if subscription else "polling")

    while True:
        if time.monotonic() >= deadline:
            if waiting_green is not None:
                # CI is green and only CodeRabbit is outstanding: the wait
                # never becomes a timeout verdict (#1332).
                _exit_green(
                    pr, repo, snapshot.head_sha, opts, log, *waiting_green,
                    coderabbit="timeout", merge_pending=snapshot.merge_pending,
                )
            print(f"TIMEOUT  after {timeout}s")
            if log:
                log.emit("timeout", timeout_sec=timeout)
            _exit_after_cancel(EXIT_TIMEOUT, "timeout", pr, repo, snapshot.head_sha, opts, log)
        poll_started = time.monotonic()
        poll_no += 1
        stale_before = STALE_READS
        # A probe that found no CodeRabbit is not final (#1332): its comments
        # can arrive minutes after the first poll, and a probe that never runs
        # again would ignore them and let a merge through. Every few polls the
        # current PR is looked at again for direct CodeRabbit output.
        recheck_coderabbit = (
            coderabbit_probe_complete
            and not review_state.coderabbit_enabled
            and not review_state.coderabbit_skipped
            and poll_no % CODERABBIT_RECHECK_POLLS == 0
        )

        # One current GraphQL snapshot owns PR state, check rollup, human
        # reviews, and optional CodeRabbit fields. This prevents bot APIs from
        # becoming a serial post-green gate.
        gate = fetch_gate_snapshot(
            repo_for_protection,
            pr,
            include_coderabbit=(
                not coderabbit_probe_complete
                or review_state.coderabbit_enabled
                or recheck_coderabbit
            ),
        )
        if gate is None:
            if rate_limit_backoff(deadline, log):
                continue
            api_failures += 1
            print(
                f"NOTE  gate snapshot unavailable ({LAST_SNAPSHOT_FAILURE}); retrying",
                file=sys.stderr,
                flush=True,
            )
            if log:
                log.emit(
                    "api_degraded",
                    source="gate_snapshot",
                    reason="fetch_failed",
                    detail=LAST_SNAPSHOT_FAILURE,
                    consecutive=api_failures,
                )
            if api_failures >= MAX_CONSECUTIVE_API_FAILURES:
                _exit_unreachable(
                    f"{api_failures} consecutive failed polls ({LAST_SNAPSHOT_FAILURE})", log
                )
            _sleep_remaining_interval(poll_started, interval)
            continue
        api_failures = 0
        snapshot = gate.pr
        if snapshot.merge_pending and not queue_announced:
            # #1973: the first stderr line names queue mode.
            print("PR merge queue detected", file=sys.stderr)
            queue_announced = True
        if snapshot.head_sha != seen_head:
            print(
                f"NOTE  head moved {seen_head[:7]} -> {snapshot.head_sha[:7]}",
                file=sys.stderr,
            )
            if log:
                log.emit("head_moved", old=seen_head, new=snapshot.head_sha)
            seen_head = snapshot.head_sha
            idle_since = None
            unknown_polls = 0
            green_since = None
            last_wait_state = None
        merge_fields = {"mergeable": snapshot.mergeable, "merge_state_status": snapshot.merge_state}
        if snapshot.state != "OPEN":
            print(f"PR-STATE  #{pr} state={snapshot.state}")
            if log:
                log.emit("pr_state", state=snapshot.state, head_sha=snapshot.head_sha)
            code = EXIT_GREEN if snapshot.state == "MERGED" else EXIT_PR_CLOSED
            label = "merged" if snapshot.state == "MERGED" else "closed"
            _exit_after_cancel(code, label, pr, repo, snapshot.head_sha, opts, log)
        if STALE_READS != stale_before:
            # Below the rate-limit reserve the session broker answered from
            # its cache: never judge (pass or fail) on data that old.
            print(
                "NOTE  check data is a cached copy (rate-limit reserve); waiting for fresh data",
                file=sys.stderr,
                flush=True,
            )
            if log:
                log.emit("api_degraded", source="broker", reason="stale_reads")
            _sleep_remaining_interval(poll_started, interval)
            continue

        # 1. Judge the head commit's checks. The REST check runs (every page)
        # are judged by the supersession rule; the rollup rows are the
        # fallback when that data is unavailable.
        verdict: Verdict | None = None
        waiting_green = None
        # The `CodeRabbit` status is the review gate below, not a CI check,
        # unless branch protection makes it one or it is the only thing that
        # reports on the head (#1332).
        coderabbit_in_rollup = any(_is_coderabbit_context(c.name) for c in gate.checks) or (
            gate.head_checks is not None
            and newest_coderabbit_status(gate.head_checks.statuses) is not None
        )
        checks = gate.checks
        ci_statuses = gate.head_checks.statuses if gate.head_checks is not None else []
        if not keep_coderabbit_status:
            other_checks = [c for c in checks if not _is_coderabbit_context(c.name)]
            other_statuses = [
                s
                for s in ci_statuses
                if not (isinstance(s, dict) and _is_coderabbit_context(s.get("context")))
            ]
            has_ci = bool(other_checks) or bool(other_statuses) or (
                gate.head_checks is not None
                and bool(gate.head_checks.check_runs or gate.head_checks.workflow_runs)
            )
            if has_ci:
                checks, ci_statuses = other_checks, other_statuses
        # Idle: nothing has registered on the head. Idle for the grace period
        # means nothing will.
        # Anything in the rollup (an external CI's status, a check app) means
        # checks do report here, so only a fully empty head is idle.
        idle = not gate.checks
        if gate.head_checks is not None:
            idle = idle and not gate.head_checks.statuses and not any(
                isinstance(r, dict) and r.get("head_sha") == snapshot.head_sha
                for r in gate.head_checks.workflow_runs
            )
        if not idle:
            idle_since = None
        elif idle_since is None:
            idle_since = poll_started
        grace_elapsed = idle_since is not None and poll_started - idle_since >= no_checks_grace
        if gate.head_checks is not None:
            if max_queued is not None:
                _exit_if_queued_too_long(gate.head_checks, snapshot, repo_for_protection,
                                         max_queued, log)
            verdict = judge_check_runs(
                gate.head_checks.check_runs,
                gate.head_checks.workflow_runs,
                snapshot.head_sha,
                required_names,
                statuses=ci_statuses,
                head_branch=snapshot.head_ref or None,
                require_re=require_re,
                runs_grace_elapsed=grace_elapsed,
            )
            checks = [j.as_check_row() for j in verdict.judgments]
        pending = [c for c in checks if c.bucket == "pending"]
        failing = [c for c in checks if c.bucket in {"fail", "cancel"}]
        counts = check_counts(checks)
        if counts["total"] == 0 and (verdict is None or verdict.state == "pending"):
            # A fresh push registers no checks for the first few seconds; an
            # empty rollup must read as "no data yet", never as green.
            if log:
                log.emit("checks", checks=counts, note="rollup_empty", **merge_fields)
            # ...unless nothing will ever register (#1418).
            if snapshot.head_sha not in no_checks_reasons:
                no_checks_reasons[snapshot.head_sha] = immediate_no_checks_reason(
                    repo_for_protection, snapshot
                )
            if snapshot.mergeable == "CONFLICTING" or snapshot.merge_state == "DIRTY":
                _exit_conflict(pr, snapshot, log, merge_fields)
            immediate = no_checks_reasons[snapshot.head_sha]
            idle_for = poll_started - idle_since if idle_since is not None else 0.0
            if immediate and idle_for < NO_CHECKS_SETTLE_SEC:
                time.sleep(NO_CHECKS_SETTLE_SEC - idle_for)
                continue
            reason = immediate or ("empty_after_grace" if grace_elapsed else None)
            if reason:
                _exit_no_checks(reason, snapshot, required_names, require_re, log)
            _sleep_remaining_interval(poll_started, interval)
            continue
        if log:
            log.emit("checks", checks=counts, **merge_fields)
            elapsed = round(max(0.0, time.monotonic() - log.started_monotonic), 2)
            print(
                f"{elapsed:.2f} {counts['succeeded']} succeeded, "
                f"{counts['failed']} failed, {counts['pending']} pending",
                file=sys.stderr,
                flush=True,
            )

        if verdict is not None:
            if _act_on_verdict(verdict, pr, repo, repo_for_protection, snapshot, opts, log):
                # The head moved under a cancellation-derived verdict: drop
                # it and judge the new head on the next poll.
                _sleep_remaining_interval(poll_started, interval)
                continue
            failing = [j.as_check_row() for j in verdict.advisory_failing]
        else:
            # Rollup fallback: no REST check-run data this poll. The rollup
            # carries every run's checks on the head, including runs that
            # concurrency superseded, and names no run order, so it cannot
            # tell a stale aggregate's `failure` from a live one. A failure
            # verdict (and any cancel) comes only from `judge_check_runs`
            # (#1742): an unjudgeable required failure is a degraded poll.
            unjudged = [c for c in failing if _is_required(c, required_names, require_re)]
            if unjudged:
                unjudged_polls += 1
                why = "check runs unavailable; a rollup failure is never judged without them"
                print(
                    f"NOTE  {unjudged[0].name} reads {unjudged[0].state or 'failed'} in the "
                    f"rollup, but {why}; retrying",
                    file=sys.stderr,
                    flush=True,
                )
                if log:
                    log.emit(
                        "api_degraded",
                        source="head_checks",
                        reason="rollup_failure_unjudged",
                        check=unjudged[0].name,
                        consecutive=unjudged_polls,
                    )
                if unjudged_polls >= MAX_CONSECUTIVE_API_FAILURES:
                    _exit_unreachable(f"{unjudged_polls} consecutive polls: {why}", log)
                _sleep_remaining_interval(poll_started, interval)
                continue
        unjudged_polls = 0

        # A conflict never resolves by waiting (#1418).
        if snapshot.mergeable == "CONFLICTING" or snapshot.merge_state == "DIRTY":
            _exit_conflict(pr, snapshot, log, merge_fields)

        if not coderabbit_probe_complete or recheck_coderabbit:
            probe = gate.coderabbit_probe or CodeRabbitProbe("degraded", 0)
            if (
                probe.state != "detected"
                and gate.coderabbit is not None
                and (gate.coderabbit.actionable or gate.coderabbit.state == "skipped")
            ):
                # Current-PR bot output is direct presence evidence even if
                # the historical five-PR sample degraded.
                probe = CodeRabbitProbe("detected", probe.sampled_merged_prs)
            if log:
                log.emit(
                    "coderabbit",
                    coderabbit={
                        "state": probe.state,
                        "sampled_merged_prs": probe.sampled_merged_prs,
                    },
                )
            if probe.state != "degraded":
                coderabbit_probe_complete = True
                review_state.coderabbit_enabled = probe.state == "detected"

        observation = gate.coderabbit if review_state.coderabbit_enabled else None
        if review_state.update_prefetched(gate.human_review_ids, observation, log):
            print("REVIEW  new review activity")
            if log:
                log.emit("review_activity", state="actionable")
            _exit_after_cancel(
                EXIT_REVIEW_ACTIVITY, "review", pr, repo, snapshot.head_sha, opts, log
            )

        # CI and review state came from the same GraphQL response. Only when
        # CodeRabbit is active does green make one more request: the head
        # commit's `CodeRabbit` status (#1332).
        checks_green = verdict.state == "pass" if verdict is not None else not pending
        # #1973: GitHub reports `mergeable: UNKNOWN` for every queued PR, so
        # the bounded-UNKNOWN conflict rule does not apply to one; the queue
        # decides, and `_exit_green` follows it to the merge or ejection.
        queued = snapshot.merge_pending
        unknown_polls = (
            unknown_polls + 1
            if checks_green and snapshot.mergeable == "UNKNOWN" and not queued
            else 0
        )
        if unknown_polls >= MERGEABLE_UNKNOWN_MAX_POLLS:
            print(
                f"CONFLICT  #{pr} checks green but mergeable stayed UNKNOWN for "
                f"{unknown_polls} polls (mergeStateStatus={snapshot.merge_state})"
            )
            _finish_exit(EXIT_CONFLICT, "mergeable_unknown", log, **merge_fields)
        if not checks_green:
            green_since = None
            last_wait_state = None
        if checks_green and (snapshot.mergeable == "MERGEABLE" or queued):
            coderabbit_note: str | None = "absent"
            if (review_state.coderabbit_enabled or coderabbit_in_rollup) and (
                coderabbit_suppressed is None
            ):
                coderabbit_suppressed = fetch_coderabbit_suppressed(
                    repo_for_protection, snapshot.base_ref
                )
            if coderabbit_suppressed:
                coderabbit_note = "suppressed"
            elif review_state.coderabbit_enabled or coderabbit_in_rollup:
                # CodeRabbit gate (#1332): report the head commit's review, and
                # wait for it only under an explicit `--coderabbit-wait`.
                if green_since is None:
                    green_since = poll_started
                waited = poll_started - green_since
                # Never wait past --timeout: the wait always ends green.
                wait_cap = min(
                    float(max(0, coderabbit_wait)), max(0.0, deadline - interval - green_since)
                )
                read_ok, status = fetch_coderabbit_status(repo_for_protection, snapshot.head_sha)
                coderabbit_note = coderabbit_gate_note(read_ok, status, waited, wait_cap)
                if coderabbit_note is None:
                    wait_state = "pending" if status is not None else (
                        "absent" if read_ok else "unreadable"
                    )
                    if wait_state != last_wait_state:
                        print(
                            f"WAIT  CI green; CodeRabbit {wait_state} on "
                            f"{snapshot.head_sha[:7]} (up to {int(wait_cap)}s)",
                            file=sys.stderr,
                            flush=True,
                        )
                        if log:
                            log.emit(
                                "coderabbit_wait",
                                state=wait_state,
                                head_sha=snapshot.head_sha,
                                waited_sec=round(waited, 2),
                                wait_cap_sec=round(wait_cap, 2),
                            )
                        last_wait_state = wait_state
                    waiting_green = (failing, counts)
            if STALE_READS != stale_before:
                # The CodeRabbit reads above came from the broker's cache:
                # green waits for a poll that read fresh data.
                if log:
                    log.emit("api_degraded", source="broker", reason="stale_reads")
                _sleep_remaining_interval(poll_started, interval)
                continue
            if waiting_green is None:
                _exit_green(
                    pr,
                    repo,
                    snapshot.head_sha,
                    opts,
                    log,
                    failing,
                    counts,
                    coderabbit=coderabbit_note,
                    merge_pending=snapshot.merge_pending,
                )

        try:
            emit_progress_report(pr, repo, snapshot, checks)
        except Exception as exc:
            print(f"NOTE  progress report failed: {exc}", file=sys.stderr)

        # A grace period, a mergeability count, a CodeRabbit wait or a queue
        # limit is counted in polls or judged at poll time: keep polling at
        # the interval. Otherwise block on the broker until something
        # changes.
        armed = (
            idle_since is not None
            or unknown_polls > 0
            or waiting_green is not None
            or max_queued is not None
        )
        _await_next_poll(
            None if armed else subscription,
            broker_watch_keys(repo_for_protection, pr, snapshot.head_sha),
            poll_started,
            interval,
            deadline,
            log,
        )


def _exit_green(
    pr: int,
    repo: str | None,
    head_sha: str,
    opts: CancelOptions,
    log: WatchLog | None,
    advisory_failing: list[CheckRow],
    counts: dict[str, int],
    *,
    coderabbit: str | None = None,
    merge_pending: bool = False,
) -> None:
    for c in advisory_failing:
        print(f"ADVISORY-FAIL  {c.name} (not in required set)")
    suffix = f" (coderabbit={coderabbit})" if coderabbit else ""
    print(f"GREEN  #{pr} all required checks passed{suffix}")
    if merge_pending and repo:
        if not MERGE_WAIT:
            print(f"NOT-MERGED-YET  #{pr} is queued or set to auto-merge (--no-merge-wait)")
        else:
            print(f"FOLLOWING  #{pr} green; waiting for the merge")
            follow_merge(pr, repo, log)
    if log:
        fields: dict[str, object] = {"mergeable": "MERGEABLE", "checks": counts}
        if coderabbit:
            fields["coderabbit"] = coderabbit
        log.emit("green", **fields)
    _exit_after_cancel(
        EXIT_GREEN,
        "always" if "always" in opts.on else "never",
        pr,
        repo,
        head_sha,
        opts,
        log,
    )


def _exit_conflict(
    pr: int, snapshot: PRSnapshot, log: WatchLog | None, merge_fields: dict[str, str]
) -> None:
    print(
        f"CONFLICT  #{pr} mergeable={snapshot.mergeable} "
        f"mergeStateStatus={snapshot.merge_state}: rebase or merge the base branch"
    )
    _finish_exit(EXIT_CONFLICT, "conflict", log, **merge_fields)


def _exit_no_checks(
    reason: str,
    snapshot: PRSnapshot,
    required: set[str] | None,
    require_re: re.Pattern[str] | None,
    log: WatchLog | None,
) -> None:
    """Terminal empty rollup: a required check can then never report (exit
    6); otherwise nothing gates the merge but `mergeStateStatus` (exit 8)."""
    if required and require_re is None:
        missing = sorted(required)
        print(f"NEVER-REPORTED  {', '.join(missing)}: required, but no check runs ({reason})")
        if log:
            log.emit("never_reported", checks=missing, reason=reason)
        _finish_exit(EXIT_NEVER_REPORTED, "never_reported", log)
    print(
        f"NO-CHECKS  #{snapshot.number} reason={reason} "
        f"mergeStateStatus={snapshot.merge_state}"
    )
    if log:
        log.emit(
            "no_checks",
            reason=reason,
            merge_state_status=snapshot.merge_state,
            mergeable=snapshot.mergeable,
            head_sha=snapshot.head_sha,
        )
    _finish_exit(EXIT_NO_CHECKS, "no_checks", log, merge_state_status=snapshot.merge_state)


def _exit_if_queued_too_long(
    head: HeadChecks,
    snapshot: PRSnapshot,
    repo: str,
    max_queued: int,
    log: WatchLog | None,
) -> None:
    now = time.time()
    stuck = [
        run
        for run in head.workflow_runs
        if isinstance(run, dict)
        and run.get("head_sha") == snapshot.head_sha
        and _lower(run.get("status")) in QUEUED_STATUSES
        and (created := _parse_iso(run.get("created_at"))) is not None
        and now - created > max_queued
    ]
    if not stuck:
        return
    jobs: list[dict] = []
    for run in stuck:
        run_id = _as_int(run.get("id"))
        if run_id is None:
            continue
        found = fetch_queued_jobs(repo, run_id)
        if not found:
            found = [{"name": str(run.get("name") or run_id), "labels": []}]
        jobs += [{**job, "run_id": run_id} for job in found]
    for job in jobs:
        labels = ", ".join(job["labels"]) or "?"
        print(f"QUEUED  {job['name']} (run {job['run_id']}) waiting for a runner: {labels}")
    if log:
        log.emit("queued_too_long", max_queued_sec=max_queued, jobs=jobs)
    _finish_exit(EXIT_QUEUED, "queued", log)


def _act_on_verdict(  # noqa: C901
    verdict: Verdict,
    pr: int,
    repo: str | None,
    repo_for_reports: str | None,
    snapshot: PRSnapshot,
    opts: CancelOptions,
    log: WatchLog | None = None,
) -> bool:
    """Exit on a terminal verdict. Returns True when the verdict was dropped
    because the PR's head moved; False when it is pending or passing."""
    for note in verdict.notes:
        print(f"NOTE  {note}", file=sys.stderr)
        if log:
            log.emit("check_note", note=note)
    if verdict.state == "approval_required":
        names = [j.name for j in verdict.judgments if j.state == "approval_required"]
        print(f"APPROVAL-REQUIRED  {', '.join(names)}: a maintainer must approve the run(s)")
        if log:
            log.emit("approval_required", checks=names)
        _finish_exit(EXIT_APPROVAL_REQUIRED, "approval_required", log)
    if verdict.state == "never_reported":
        print(
            f"NEVER-REPORTED  {', '.join(verdict.missing)}: required but no check run exists "
            "and every workflow run on this head commit has finished"
        )
        if log:
            log.emit("never_reported", checks=verdict.missing)
        _finish_exit(EXIT_NEVER_REPORTED, "never_reported", log)
    if verdict.state == "stale":
        names = [j.name for j in verdict.judgments if j.required and j.state == "stale"]
        print(f"STALE  {', '.join(names)}: GitHub marked the check stale; re-run needed")
        if log:
            log.emit("stale", checks=names)
        _finish_exit(EXIT_STALE, "stale", log)
    if verdict.state != "fail":
        return False

    if verdict.cancellation_derived:
        # A cancellation is often concurrency reacting to a new push. Never
        # exit on stale data: confirm the head before acting.
        fresh = PRSnapshot.fetch(pr, repo)  # ci-lint: allow GHAPI-001 once per failure verdict, which then exits or drops the verdict
        if fresh is None or fresh.head_sha != snapshot.head_sha:
            moved_to = fresh.head_sha if fresh is not None else None
            print(
                f"NOTE  head moved {snapshot.head_sha[:7]} -> {(moved_to or '?')[:7]}; "
                "dropping the verdict",
                file=sys.stderr,
            )
            if log:
                log.emit("head_moved", old=snapshot.head_sha, new=moved_to)
            return True

    rerun = _rerun_since_verdict(verdict, repo_for_reports)
    if rerun is not None:
        print(f"NOTE  run {rerun} was re-run since the verdict; not cancelling it", file=sys.stderr)
        if log:
            log.emit("cancel_skipped", reason="rerun_since_verdict", run_id=rerun)
        return False

    first = verdict.failing[0]
    if log:
        log.emit(
            "required_failure",
            check={
                "name": first.name,
                "state": first.conclusion,
                "link": first.link,
                "workflow": first.workflow,
            },
        )
    # Diagnose first, then cancel. The probe is one bounded request against
    # the failing *job*; cancelling first raced the log's availability and
    # left the caller a bare "FAIL <name>" with nothing to act on.
    reports = [_build_failure_report(first.as_check_row(), repo_for_reports)]
    reports += [
        FailureReport(j.as_check_row(), str(j.run_id) if j.run_id else None, "", None)
        for j in verdict.failing[1:]
    ]
    _cancel_for_exit(
        "fail", pr, repo, snapshot.head_sha, opts, log, scope=dict(verdict.failing_run_ids)
    )
    if "fail" in opts.on or "always" in opts.on:
        print(
            "NOTE  cancelling the failing run and older runs of its workflow; "
            "newer or same-second runs and other workflows are left alone"
        )
    for judgment, report in zip(verdict.failing, reports):
        print(report.render())
        if judgment.workflow_broken:
            print("  note:       startup_failure: the workflow is broken, not the code")
    causes = root_cause_reports(
        reports[0],
        verdict.advisory_failing,
        lambda j: _build_failure_report(j.as_check_row(), repo_for_reports),
    )
    if causes:
        print(f"NOTE  {first.name} names no cause; failing jobs behind it:")
        for cause in causes:
            print(cause.render())
    if log and (reports[0].first_error or reports[0].classifier):
        log.emit(
            "failure_diagnostic",
            check_name=first.name,
            first_error=reports[0].first_error,
            classifier=reports[0].classifier,
        )
    _finish_exit(EXIT_REQUIRED_FAIL, "fail", log)
    return False


def _rerun_since_verdict(verdict: Verdict, repo: str | None) -> int | None:
    """A failing run whose attempt or status changed since the verdict (#1449).

    Its run id now names a newer attempt than the failing check, so cancelling
    it would kill the re-run. Returns that run id, or None.
    """
    if not repo:
        return None
    for j in verdict.failing:
        if j.run_id is None or j.run_attempt is None:
            continue
        run = gh_json("api", f"repos/{repo}/actions/runs/{j.run_id}")  # ci-lint: allow GHAPI-001 conditional: gh_json sends REST GETs through api_get (session broker ETag, else If-None-Match)
        if not isinstance(run, dict):
            continue
        attempt = _as_int(run.get("run_attempt"))
        if (attempt is not None and attempt != j.run_attempt) or (
            _lower(run.get("status")) != j.run_status
        ):
            return j.run_id
    return None


def _is_required(
    c: CheckRow, required: set[str] | None, require_re: re.Pattern[str] | None
) -> bool:
    if require_re is not None:
        return bool(require_re.search(c.name))
    if not required:
        # No protection (None), no allowlist, OR protection that lists zero
        # checks (empty set): treat every check as required so a red lane
        # fails the wait immediately instead of the watcher idling until
        # the whole matrix — including the slow Mac lanes — finishes.
        return True
    return c.name in required


def _build_failure_report(c: CheckRow, repo: str | None) -> FailureReport:
    run_id = _extract_run_id_from_link(c.link)
    # A failing check's link is `…/actions/runs/<run>/job/<job>`. `job_id` was
    # declared for this and never assigned, so every probe fell back to the
    # run-level log — the one that is unavailable while the run is in
    # progress, which on this path it always is.
    job_id = c.job_id or _extract_job_id_from_link(c.link)
    first_err, classifier, unavailable = ("", None, "")
    if repo and (run_id or job_id):
        first_err, classifier, unavailable = classify_failure_explained(repo, run_id, job_id)
    return FailureReport(
        check=c,
        run_id=run_id,
        first_error=first_err,
        classifier=classifier,
        log_unavailable=unavailable,
    )


def _extract_run_id_from_link(link: str | None) -> str | None:
    if not link:
        return None
    m = re.search(r"/actions/runs/(\d+)", link)
    return m.group(1) if m else None


def _extract_job_id_from_link(link: str | None) -> str | None:
    if not link:
        return None
    m = re.search(r"/job/(\d+)", link)
    return m.group(1) if m else None


# ---- following a queued PR to its merge (#2035) --------------------------------------------------

# Polls a just-merged-or-enqueued PR may still show no queue entry, while
# `gh pr merge` finishes enqueueing it.
ENQUEUE_GRACE_POLLS = 3
# A cancelled merge-group job is the queue rebuilding a group, not a verdict:
# the PR moves to a new merge-group commit.
MERGE_GROUP_FAILURES = {"failure", "timed_out", "startup_failure", "action_required"}

MERGE_QUEUE_QUERY = """
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      state
      baseRefName
      autoMergeRequest { enabledAt }
      mergeQueueEntry { state position enqueuedAt headCommit { oid } }
      timelineItems(last: 1, itemTypes: [REMOVED_FROM_MERGE_QUEUE_EVENT]) {
        nodes { ... on RemovedFromMergeQueueEvent { reason createdAt } }
      }
    }
  }
}
"""


@dataclass
class QueueState:
    state: str
    base_ref: str
    auto_merge: bool
    entry_state: str | None
    position: int | None
    queue_sha: str | None
    removal_reason: str | None
    removed_at: datetime | None
    # When this entry joined the queue: older merge-group runs belong to an
    # earlier attempt and must not eject it again (#2481).
    enqueued_at: datetime | None = None


def _removal(pr: dict) -> tuple[str | None, datetime | None]:
    nodes = (pr.get("timelineItems") or {}).get("nodes") or []
    node = nodes[-1] if nodes and isinstance(nodes[-1], dict) else {}
    stamp = node.get("createdAt")
    when = datetime.fromisoformat(stamp.replace("Z", "+00:00")) if isinstance(stamp, str) else None
    reason = node.get("reason")
    if reason is not None and not isinstance(reason, str):
        raise ValueError("invalid queue removal reason")
    return (reason.strip() or None) if reason is not None else None, when


def parse_queue_state(data: object) -> QueueState | None:
    if not isinstance(data, dict) or data.get("errors"):
        return None
    try:
        pr = data["data"]["repository"]["pullRequest"]  # type: ignore[index]
    except (KeyError, TypeError):
        return None
    if not isinstance(pr, dict):
        return None
    if not _complete_queue_response(pr):
        return None
    entry = pr.get("mergeQueueEntry") or {}
    try:
        reason, removed_at = _removal(pr)
        if removed_at is not None and removed_at.utcoffset() is None:
            return None
    except (ValueError, TypeError, AttributeError):
        return None
    return QueueState(
        state=str(pr.get("state") or ""),
        base_ref=str(pr.get("baseRefName") or ""),
        auto_merge=bool(pr.get("autoMergeRequest")),
        entry_state=entry.get("state") if entry else None,
        position=entry.get("position") if entry else None,
        queue_sha=(entry.get("headCommit") or {}).get("oid") if entry else None,
        removal_reason=reason,
        removed_at=removed_at,
        enqueued_at=_stamp(entry.get("enqueuedAt")) if entry else None,
    )


def _stamp(value: object) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _complete_queue_response(pr: dict) -> bool:
    state = pr.get("state")
    if not isinstance(state, str) or state not in {"OPEN", "CLOSED", "MERGED"}:
        return False
    if not isinstance(pr.get("baseRefName"), str) or not pr["baseRefName"]:
        return False
    if "autoMergeRequest" not in pr or "mergeQueueEntry" not in pr:
        return False
    auto = pr["autoMergeRequest"]
    if (auto is not None
            and (not isinstance(auto, dict) or not isinstance(auto.get("enabledAt"), str))):
        return False
    timeline = pr.get("timelineItems")
    if not isinstance(timeline, dict) or not isinstance(timeline.get("nodes"), list):
        return False
    nodes = timeline["nodes"]
    if nodes and (not isinstance(nodes[-1], dict) or "reason" not in nodes[-1]
                  or not isinstance(nodes[-1].get("createdAt"), str)):
        return False
    entry = pr["mergeQueueEntry"]
    if entry is None:
        return True
    if (not isinstance(entry, dict) or not isinstance(entry.get("state"), str)
            or isinstance(entry.get("position"), bool)
            or not isinstance(entry.get("position"), int) or "headCommit" not in entry):
        return False
    head = entry["headCommit"]
    return head is None or (isinstance(head, dict) and isinstance(head.get("oid"), str))


def fetch_queue_state(repo: str, pr: int) -> QueueState | None:
    owner, _, name = repo.partition("/")
    data = gh_json(
        "api", "graphql", "-f", f"query={MERGE_QUEUE_QUERY}",
        "-F", f"owner={owner}", "-F", f"name={name}", "-F", f"number={pr}",
    )
    return parse_queue_state(data)


def failed_merge_group_job(
    jobs_by_run: list[tuple[dict, list[dict]]], required: set[str] | None,
) -> tuple[dict, dict] | None:
    """The first required failed job on the queue's commit, with its run."""
    for run, jobs in jobs_by_run:
        for job in jobs:
            conclusion = _lower(job.get("conclusion"))
            name = str(job.get("name") or "")
            if conclusion not in MERGE_GROUP_FAILURES or (required and name not in required):
                continue
            return run, job
    return None


def merge_group_failure(jobs_by_run: list[tuple[dict, list[dict]]], required: set[str] | None) -> str | None:
    """The first required failed job on the queue's commit, as `workflow / job`."""
    found = failed_merge_group_job(jobs_by_run, required)
    if found is None:
        return None
    run, job = found
    conclusion = _lower(job.get("conclusion"))
    name = str(job.get("name") or "")
    return f"{run.get('name') or 'workflow'} / {name}: {conclusion} {job.get('html_url') or ''}".rstrip()


def merge_group_cause(repo: str, run: dict, job: dict) -> str:
    """The ejecting job's first error line, so no follow-up log read is needed."""
    first_err, _label, unavailable = classify_failure_explained(
        repo, _id_text(run.get("id")), _id_text(job.get("id")),
    )
    if first_err:
        return f"\n  first error: {first_err}"
    return f"\n  log unavailable: {unavailable}" if unavailable else ""


def _id_text(value: object) -> str | None:
    return str(value) if value not in (None, "") else None


def fetch_merge_group_jobs(
    repo: str, sha: str, since: datetime | None = None,
) -> list[tuple[dict, list[dict]]]:
    data = gh_json("api", f"repos/{repo}/actions/runs?head_sha={sha}&event=merge_group&per_page=100")
    runs = data.get("workflow_runs") if isinstance(data, dict) else None
    result = []
    for run in current_attempt_runs(runs if isinstance(runs, list) else [], since):
        jobs = gh_json("api", f"repos/{repo}/actions/runs/{run.get('id')}/jobs?per_page=100")
        listed = jobs.get("jobs") if isinstance(jobs, dict) else None
        result.append((run, listed if isinstance(listed, list) else []))
    return result


def current_attempt_runs(runs: list, since: datetime | None) -> list[dict]:
    """Runs from this queue attempt; one created before `since` is stale (#2481)."""
    kept = []
    for run in runs:
        if not isinstance(run, dict):
            continue
        created = _stamp(run.get("created_at"))
        if since is not None and created is not None and created < since:
            continue
        kept.append(run)
    return kept


@dataclass
class MergeFollow:
    """What the follower has seen so far, carried from poll to poll."""

    started: datetime
    enqueued: bool = False
    polls: int = 0
    required: set[str] | None = None
    required_read: bool = False
    api_failures: int = 0
    last_queue: tuple[int | None, str] | None = None
    unmergeable_polls: int = 0
    removal_candidate: tuple[datetime, str] | None = None


def _queue_verdict(q: QueueState, seen: MergeFollow) -> tuple[int, str] | None:
    """Exit for a PR state that settles the wait without reading any run."""
    if q.state == "MERGED":
        return EXIT_GREEN, "merged"
    if q.state == "CLOSED":
        return EXIT_PR_CLOSED, "closed"
    removed_now = q.removed_at is not None and q.removed_at >= seen.started
    if (q.entry_state is None and (seen.enqueued or removed_now)
            and (EXPLICIT_MERGE_WAIT or not q.auto_merge)):
        reason = (q.removal_reason or "").strip()
        if removed_now and q.removed_at is not None and reason and reason.casefold() != "merged":
            candidate = (q.removed_at, reason)
            if seen.removal_candidate == candidate:
                return EXIT_EJECTED, f"removed from the merge queue: {reason}"
            seen.removal_candidate = candidate
        else:
            seen.removal_candidate = None
        # A missing entry or a "merged" removal is not an authoritative PR state.
        return None
    seen.removal_candidate = None
    if (not EXPLICIT_MERGE_WAIT and q.entry_state is None and not q.auto_merge
            and not seen.enqueued and seen.polls >= ENQUEUE_GRACE_POLLS):
        return EXIT_NOT_ENQUEUED, "open, not in the merge queue and not set to auto-merge"
    return None


def _merge_group_verdict(repo: str, q: QueueState, seen: MergeFollow) -> tuple[int, str] | None:
    if not q.queue_sha:
        return None
    if not seen.required_read:
        seen.required = fetch_required_check_names(repo, q.base_ref) if q.base_ref else None
        seen.required_read = True
    jobs = fetch_merge_group_jobs(repo, q.queue_sha, q.enqueued_at)
    failed = merge_group_failure(jobs, seen.required)
    if not failed:
        return None
    run, job = failed_merge_group_job(jobs, seen.required) or ({}, {})
    cause = merge_group_cause(repo, run, job)
    return EXIT_EJECTED, f"required merge-group check failed: {failed}{cause}"


_UNTIL_MERGED_LABELS = {
    EXIT_GREEN: "MERGED",
    EXIT_PR_CLOSED: "PR-STATE CLOSED",
    EXIT_EJECTED: "EJECTED",
    EXIT_NOT_ENQUEUED: "NOT-ENQUEUED",
    EXIT_UNMERGEABLE: "UNMERGEABLE",
}


def _poll_queue(repo: str, pr: int, seen: MergeFollow, log: WatchLog | None) -> tuple[int, str] | None:
    q = fetch_queue_state(repo, pr)
    if q is None:
        seen.api_failures += 1
        seen.unmergeable_polls = 0
        seen.removal_candidate = None
        if seen.api_failures >= MAX_CONSECUTIVE_API_FAILURES:
            _exit_unreachable("merge queue state unreadable", log)
        return None
    seen.api_failures = 0
    seen.polls += 1
    verdict = _queue_verdict(q, seen)
    if verdict is None and EXPLICIT_MERGE_WAIT:
        seen.unmergeable_polls = seen.unmergeable_polls + 1 if q.entry_state == "UNMERGEABLE" else 0
        if seen.unmergeable_polls >= 2:
            return EXIT_UNMERGEABLE, "queue entry remained unmergeable"
    if not EXPLICIT_MERGE_WAIT:
        verdict = verdict or _merge_group_verdict(repo, q, seen)
    if verdict is None and q.entry_state is not None:
        current = (q.position, q.entry_state)
        if not seen.enqueued or (EXPLICIT_MERGE_WAIT and current != seen.last_queue):
            print(f"ENQUEUED  #{pr} position={q.position} state={q.entry_state}")
        seen.last_queue = current
        seen.enqueued = True
    elif q.entry_state is None:
        seen.last_queue = None
    if log:
        log.emit("queue", state=q.state, entry=q.entry_state, position=q.position,
                 queue_sha=q.queue_sha, auto_merge=q.auto_merge)
    return verdict


# (interval, monotonic deadline) of this watch, shared with the merge follower.
FOLLOW: tuple[int, float] = (20, 0.0)
# `--no-merge-wait` clears it: green on the PR's checks ends the watch.
MERGE_WAIT = True
EXPLICIT_MERGE_WAIT = False


def follow_merge(pr: int, repo: str | None, log: WatchLog | None) -> int:
    interval, deadline = FOLLOW
    seen = MergeFollow(started=_utc_now())
    while time.monotonic() < deadline:
        started = time.monotonic()
        verdict = _poll_queue(str(repo), pr, seen, log)
        if verdict is not None:
            code, why = verdict
            print(f"{_UNTIL_MERGED_LABELS[code]}  #{pr} {why}")
            _finish_exit(code, why.split(":")[0], log)
        wait_interval = min(interval, max(0.0, deadline - started))
        _sleep_remaining_interval(started, wait_interval)
    if seen.enqueued and seen.last_queue is None:
        print(f"TIMEOUT  #{pr} queue state unconfirmed; merge or removal not established")
    else:
        print(f"TIMEOUT  #{pr} not merged yet (queued={seen.enqueued})")
    _finish_exit(EXIT_TIMEOUT, "timeout", log)
    return EXIT_TIMEOUT


def _env_int(name: str, default: int) -> int:
    raw = os.environ.get(name, "").strip()
    try:
        return int(raw) if raw else default
    except ValueError:
        return default


class _UsageParser(argparse.ArgumentParser):
    """argparse, but a usage error exits 64, not 2 (2 is "review activity")."""

    def error(self, message: str) -> NoReturn:
        self.print_usage(sys.stderr)
        self.exit(EXIT_USAGE, f"{self.prog}: error: {message}\n")


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = _UsageParser(
        prog="pr_merge_watch",
        description="Fail-fast PR-check waiter for clud (issue #408).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "exit codes: 0 green (after CodeRabbit finishes the head commit, when active), "
            "1 required check failed, 2 review activity, "
            "3 PR closed, 4 timeout (never cancels), 5 approval required, 6 required check never "
            "reported, 7 stale (re-run needed), 8 no checks will ever report, "
            "9 merge conflict, 10 GitHub unreachable, 11 queued too long, "
            "12 auto-merge withdrawn before queueing, 13 ejected from the merge queue, "
            "64 usage error (fix the command; not a verdict), "
            "124 clud watchdog stopped the watch (retry), 130/143 killed\n\n"
            "supersession rule (#1330): checks on the PR's current head commit are "
            "grouped by (workflow file, check name) and ordered by check-run id. A "
            "cancelled check is replaced by any newer check, even a queued one; a "
            "completed result only by a newer check that actually ran (not skipped). "
            "A cancelled check (or a failure in a cancelled run) with no newer check, "
            "no newer run of its workflow and no live sibling run fails; runs are "
            "ordered by created_at, never by run id. success, neutral and skipped "
            "pass. A cancellation-derived failure is acted on only after re-reading "
            "the head; on failure only the failing run and older runs of its "
            "workflow are cancelled."
        ),
    )
    p.add_argument(
        "--triggers-for-base",
        metavar="BRANCH",
        default=None,
        help="print JSON {pr_workflows, dispatchable} for a PR into BRANCH, read "
        "from the default branch's workflows on --repo, and exit (#1893)",
    )
    p.add_argument(
        "pr",
        nargs="?",
        default=None,
        help="PR to watch: a number, a branch, or a PR URL (default: the current "
        "branch's PR). Anything but a URL resolves on --repo",
    )
    p.add_argument(
        "--repo",
        help="owner/name (default: the repo of `git remote get-url origin`, so a fork "
        "watches its own PR, not the parent's; gh's default repo only when origin is "
        "not a github.com URL). Every gh call carries it explicitly",
    )
    p.add_argument(
        "--interval",
        type=int,
        default=20,
        help="seconds between polls (default 20)",
    )
    p.add_argument(
        "--timeout",
        type=int,
        default=_env_int("CLUD_PR_MERGE_WATCH_TIMEOUT", DEFAULT_TIMEOUT_SEC),
        help="overall wait cap in seconds (default $CLUD_PR_MERGE_WATCH_TIMEOUT, else "
        "3600); the tool runner accepts at most 86400 seconds",
    )
    merge_mode = p.add_mutually_exclusive_group()
    merge_mode.add_argument(
        "--until", choices=["merged"], default=None,
        help="wait for the PR to merge, reporting queue changes (#2141)",
    )
    merge_mode.add_argument(
        "--no-merge-wait",
        dest="merge_wait",
        action="store_false",
        help="stop at green on the PR's own checks; do not follow a merge queue or "
        "auto-merge to the merge itself (#2035)",
    )
    p.add_argument(
        "--no-checks-grace",
        type=int,
        default=_env_int("CLUD_PR_MERGE_WATCH_NO_CHECKS_GRACE", DEFAULT_NO_CHECKS_GRACE_SEC),
        help="seconds an empty rollup is waited for before exiting 8 NO_CHECKS "
        "(default $CLUD_PR_MERGE_WATCH_NO_CHECKS_GRACE, else 60)",
    )
    p.add_argument(
        "--max-queued",
        type=int,
        default=None,
        help="exit 11 when a run has waited this many seconds for a runner (default: off)",
    )
    p.add_argument(
        "--no-broker-wait",
        dest="broker_wait",
        action="store_false",
        help="in a clud session with the gh read broker, poll at --interval instead of "
        "waiting on the broker for a change between polls (#1743)",
    )
    p.add_argument(
        "--coderabbit-wait",
        type=int,
        default=DEFAULT_CODERABBIT_WAIT_SEC,
        help="opt in: once CI is green, seconds to wait for a pending CodeRabbit review "
        "of the head commit (default 0: never wait; bounded by --timeout). A repo whose "
        ".coderabbit.yaml disables auto review, or a head with no CodeRabbit status, "
        "never waits. The wait always ends green, never in a failure or a cancel",
    )
    p.add_argument(
        "--require",
        default=None,
        help="regex of check names to treat as required; overrides branch protection. "
        "Use --require '.*' to wait for all reported checks, including advisory lanes. "
        "Without this option, use branch-required checks (all reported checks when "
        "protection is absent or inaccessible)",
    )
    # Cancellation control
    p.add_argument(
        "--cancel-on",
        default=",".join(sorted(CANCEL_ON_DEFAULTS)),
        help="comma-separated subset of "
        f"{sorted(CANCEL_ON_CHOICES)} (default: fail,review,closed; a timeout never cancels unless asked)",
    )
    p.add_argument(
        "--cancel-mode",
        default="runs",
        choices=sorted(CANCEL_MODE_CHOICES),
        help="granularity of cancellation (default: runs)",
    )
    p.add_argument(
        "--cancel-timeout",
        type=int,
        default=30,
        help="seconds to wait for cancellations to settle (default 30)",
    )
    p.add_argument("--no-cancel", action="store_true", help="shortcut for --cancel-on=never")
    p.add_argument(
        "--require-cancel",
        action="store_true",
        help="mark cancellation API errors as required in the event log",
    )
    p.add_argument(
        "--dry-run-cancel",
        action="store_true",
        help="list workflow runs that would be cancelled without POSTing",
    )
    p.add_argument(
        "--ignore-permission-errors",
        dest="ignore_perm",
        action="store_true",
        default=True,
        help="warn + continue on cancel 403s (default; --no-ignore-permission-errors flips)",
    )
    p.add_argument("--no-ignore-permission-errors", dest="ignore_perm", action="store_false")
    p.add_argument(
        "--quiet",
        action="store_true",
        help="suppress per-poll progress lines; print only the final verdict (#2368)",
    )
    p.add_argument(
        "--no-retry", action="store_true", help="disable backoff/retry on cancel API calls"
    )
    # #1839: the tool runner forwards a `--` placed after the PR number as
    # data (`1835 -- --no-cancel`), and argparse then reads every later flag as a
    # positional and exits 64 -- in the background, silently wasting a CI
    # wait. This tool takes no passthrough, so a literal `--` is only ever
    # that forwarding separator.
    ns = p.parse_args([arg for arg in argv if arg != "--"])
    command_cap = _env_int("CLUD_TOOL_COMMAND_TIMEOUT_SECS", 0)
    if command_cap > 0:
        ns.timeout = min(ns.timeout, max(1, command_cap - 60))
    return ns


def _resolve_cancel_options(ns: argparse.Namespace) -> CancelOptions:
    raw = "never" if ns.no_cancel else ns.cancel_on
    parts = {p.strip() for p in raw.split(",") if p.strip()}
    invalid = parts - CANCEL_ON_CHOICES
    if invalid:
        raise SystemExit(
            f"--cancel-on: invalid values {sorted(invalid)}; "
            f"choices are {sorted(CANCEL_ON_CHOICES)}"
        )
    if "never" in parts:
        parts = set()
    elif "always" in parts:
        parts = CANCEL_ON_CHOICES - {"never", "always"}
        parts.add("always")
    return CancelOptions(
        on=parts,
        mode="none" if ns.no_cancel else ns.cancel_mode,
        timeout=ns.cancel_timeout,
        require=ns.require_cancel,
        dry_run=ns.dry_run_cancel,
        ignore_permission_errors=ns.ignore_perm,
        no_retry=ns.no_retry,
    )


def _print_triggers(repo: str | None, base: str) -> int:
    repo = repo or _resolve_origin_repo()
    if not repo:
        print("UNRESOLVED  --triggers-for-base needs --repo OWNER/NAME", file=sys.stderr)
        return EXIT_USAGE
    default = gh_json("api", f"repos/{repo}")  # ci-lint: allow GHAPI-001 one-shot query mode, not a watch loop
    ref = default.get("default_branch") if isinstance(default, dict) else None
    texts = fetch_workflow_texts(repo, str(ref or "HEAD"))
    if texts is None:
        print(f"UNREACHABLE  could not read {repo} workflows", file=sys.stderr)
        return EXIT_GITHUB_UNREACHABLE
    print(json.dumps({"base": base, **triggers_for_base(texts, base)}))
    return EXIT_GREEN


def main(argv: list[str] | None = None) -> int:
    global FOLLOW, MERGE_WAIT, EXPLICIT_MERGE_WAIT, QUIET
    ns = parse_args(argv if argv is not None else sys.argv[1:])
    QUIET = ns.quiet
    if ns.triggers_for_base is not None:
        return _print_triggers(ns.repo, ns.triggers_for_base)
    opts = _resolve_cancel_options(ns)
    resolved = resolve_pr_selector(ns.pr, ns.repo)
    if isinstance(resolved[0], int):
        print(f"UNRESOLVED  {resolved[1]}", file=sys.stderr)
        return resolved[0]
    repo, pr_number = resolved
    log = WatchLog.create(pr_number, repo)
    # Honor an env override for testing.
    if os.environ.get("CLUD_PR_MERGE_WATCH_DRY_RUN") == "1":
        print(
            f"DRY-RUN pr={pr_number} repo={repo} "
            f"timeout={ns.timeout} "
            f"require={ns.require or 'branch-protection'} cancel_on={sorted(opts.on)}"
        )
        log.emit("dry_run", timeout=ns.timeout, cancel_on=sorted(opts.on))
        log.emit("EXIT", code=EXIT_GREEN, reason="dry_run")
        log.close()
        return EXIT_GREEN
    install_kill_handlers()
    try:
        FOLLOW = (ns.interval, time.monotonic() + ns.timeout)
        MERGE_WAIT = ns.merge_wait
        EXPLICIT_MERGE_WAIT = ns.until == "merged"
        if EXPLICIT_MERGE_WAIT:
            return follow_merge(pr_number, repo, log)
        code = watch(
            pr_number,
            repo,
            ns.interval,
            ns.timeout,
            ns.require,
            opts,
            log,
            no_checks_grace=ns.no_checks_grace,
            max_queued=ns.max_queued,
            coderabbit_wait=ns.coderabbit_wait,
            broker_wait=ns.broker_wait,
        )
    except WatchKilled as killed:
        print(f"KILLED  {killed}", file=sys.stderr)
        _finish_exit(killed.code, "killed", log, signal=killed.signum)
        return killed.code
    return _close_log(code, log)


def _close_log(code: int, log: WatchLog) -> int:
    if not log.closed:
        log.emit("EXIT", code=code, reason="return")
        log.close()
    return code


if __name__ == "__main__":
    sys.exit(main())
