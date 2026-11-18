#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "running-process==4.10.1",
# ]
# ///
# managed-by: clud
"""run_watch.py — wait on one GitHub Actions run that has no PR (#1941).

Usage:
  run_watch.py <run-id> [--repo OWNER/REPO] [--timeout SECS]
               [--interval SECS] [--cancel]

The sanctioned waiter for a `workflow_dispatch`, `schedule` or `push` run,
which `pr_merge_watch.py` cannot reach because it takes a PR. Start it once
in the background and wait for its exit; it reports failed jobs and a job
count when the run settles.

Fails fast: the first job that concludes failure, cancelled or timed_out ends
the watch, naming the first three failed jobs, their failed step(s) and the
tail of each failed-step log (40 lines of at most 300 chars). `--cancel` then
also cancels the rest of the run.
`--interval` is clamped to at least 30 seconds to spare the API rate limit.

Run through clud's `tool run` subcommand:
  "$CLUD_EXE" tool run github/run_watch.py -- 38310141872 --repo OWNER/REPO

Exit codes:
  0    the run concluded success (or skipped/neutral)
  1    a job failed, or the run concluded failure/cancelled/timed_out
  3    gh could not read the run
  64   usage error
  124  timed out while the run was still in progress (resumable: re-run with
       the same arguments)
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from urllib.parse import urlparse

from running_process import PIPE, RunningProcess, TimeoutExpired

GH_TIMEOUT = 60.0
FAILED = {"failure", "cancelled", "timed_out", "startup_failure", "action_required", "stale"}
EXIT_OK = 0
EXIT_FAILED = 1
EXIT_GH = 3
EXIT_USAGE = 64
EXIT_TIMEOUT = 124
PAGE_SIZE = 30
MAX_PAGES = 200
_ETAGS: dict[str, tuple[str, object]] = {}
_WATCH_DEADLINE: float | None = None
MIN_INTERVAL = 30.0
LOG_TAIL_LINES = 40
LOG_LINE_CHARS = 300
MAX_FAILED_REPORTS = 3


def gh(*args: str) -> tuple[int, str, str]:
    """Run gh; return (exit code, stdout, stderr)."""
    timeout = GH_TIMEOUT
    if _WATCH_DEADLINE is not None and args[:2] != ("run", "cancel"):
        timeout = min(timeout, _WATCH_DEADLINE - time.monotonic())
        if timeout <= 0:
            return 124, "", "watch deadline expired"
    try:
        # stderr=PIPE: running-process merges stderr into stdout otherwise (#1175).
        res = RunningProcess.run(
            ["gh", *args], capture_output=True, stderr=PIPE, text=True, timeout=timeout
        )
    except (TimeoutError, TimeoutExpired):
        return 124, "", f"gh {' '.join(args)} timed out after {timeout:g}s"
    except OSError as exc:
        return 127, "", f"could not start gh: {exc}"
    return res.returncode, res.stdout or "", res.stderr or ""


def evaluate(run: dict) -> tuple[str, list[dict]]:
    """Pure: classify one `gh run view --json status,conclusion,jobs` sample.

    Returns `("failed", failed_jobs)` as soon as any job failed, even while
    others still run; `("success", [])` when the run completed cleanly;
    `("failed", [])` when the run itself concluded badly with no failed job
    to name; otherwise `("pending", [])`.
    """
    jobs = run.get("jobs") or []
    failed = [
        job
        for job in jobs
        if job.get("status") == "completed" and (job.get("conclusion") or "") in FAILED
    ]
    if failed:
        return "failed", failed
    if run.get("status") != "completed":
        return "pending", []
    conclusion = run.get("conclusion")
    if not isinstance(conclusion, str):
        return "pending", []
    if conclusion in FAILED:
        return "failed", []
    if conclusion in {"success", "neutral", "skipped"}:
        return "success", []
    return "pending", []


def render_jobs(jobs: list[dict]) -> str:
    failed = [
        job
        for job in jobs
        if job.get("status") == "completed" and (job.get("conclusion") or "") in FAILED
    ]
    lines = [f"  {len(jobs)} jobs observed; {len(failed)} failed"]
    for job in failed[:8]:
        raw_name = str(job.get("name") or "?")[:160]
        name = "".join(char if char.isprintable() else "?" for char in raw_name)
        lines.append(f"  {str(job.get('conclusion') or '?')[:24]:<12} {name}")
    if len(failed) > 8:
        lines.append(f"  {len(failed) - 8} additional failed jobs omitted; inspect the run URL")
    return "\n".join(lines)


def failed_steps(job: dict) -> list[str]:
    """Pure: names of the steps in `job` that concluded badly."""
    return [
        step.get("name", "?")
        for step in job.get("steps") or []
        if (step.get("conclusion") or "") in FAILED
    ]


def _printable(text: str, limit: int) -> str:
    """Pure: `text` cut to `limit` chars with control characters replaced."""
    return "".join(char if char.isprintable() else "?" for char in text[:limit])


def log_tail(log: str, lines: int = LOG_TAIL_LINES) -> str:
    """Pure: the last `lines` non-empty lines of a `--log-failed` dump, each bounded."""
    kept = [line for line in log.splitlines() if line.strip()]
    return "\n".join(_printable(line, LOG_LINE_CHARS) for line in kept[-lines:])


def report_failed_job(job: dict, repo: str | None) -> None:
    """Print a failed job's failed steps and the tail of its failed-step log."""
    steps = [_printable(str(step), 160) for step in failed_steps(job)]
    print(f"FAILED job: {_printable(str(job.get('name') or '?'), 160)}")
    print(f"  failed step(s): {', '.join(steps) if steps else '(none reported)'}")
    # The REST jobs page carries `id`; `gh run view --json jobs` says `databaseId`.
    job_id = job.get("id") or job.get("databaseId")
    if not job_id:
        return
    args = ["run", "view", "--job", str(job_id), "--log-failed"]
    if repo:
        args += ["--repo", repo]
    code, out, err = gh(*args)
    tail = log_tail(out) if code == 0 else ""
    if tail:
        print(f"  --- last {LOG_TAIL_LINES} lines of the failed-step log ---")
        print(tail)
    else:
        reason = _printable(err.strip(), LOG_LINE_CHARS) or f"gh exit {code}"
        print(f"  (failed-step log unavailable: {reason})")


class _UsageParser(argparse.ArgumentParser):
    def error(self, message: str) -> None:  # type: ignore[override]
        self.print_usage(sys.stderr)
        sys.stderr.write(f"run_watch.py: error: {message}\n")
        sys.exit(EXIT_USAGE)


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = _UsageParser(prog="run_watch.py", description=__doc__.splitlines()[0])
    p.add_argument("run_id", type=int)
    p.add_argument("--repo", default=None)
    p.add_argument("--timeout", type=float, default=3300.0)
    p.add_argument("--interval", type=float, default=30.0)
    p.add_argument("--cancel", action="store_true", help="cancel the run on the first failed job")
    ns = p.parse_args(argv)
    ns.interval = max(MIN_INTERVAL, ns.interval)
    return ns


def api_get(path: str, hostname: str | None = None) -> object | None:
    """Fetch one conditional REST page, using the session broker when present."""
    broker = os.environ.get("CLUD_GH_READ_BROKER") == "1" and bool(
        os.environ.get("CLUD_GH_SHIM_TARGET")
    )
    cache_key = f"{hostname or 'github.com'}:{path}"
    cached = _ETAGS.get(cache_key)
    args = ["api", path] if broker else ["api", "-i", path]
    if hostname:
        args += ["--hostname", hostname]
    if not broker and cached:
        args += ["-H", f"If-None-Match: {cached[0]}"]
    code, out, err = gh(*args)
    if "clud: gh read broker: rate-limit reserve" in err:
        return None
    if not broker and out.startswith("HTTP/"):
        header, _, body = out.replace("\r\n", "\n").partition("\n\n")
        lines = header.splitlines()
        parts = lines[0].split()
        status = int(parts[1]) if len(parts) > 1 and parts[1].isdigit() else 0
        if status == 304 and cached:
            return cached[1]
        if status != 200:
            return None
        headers = dict(line.split(":", 1) for line in lines[1:] if ":" in line)
        etag = next((v.strip() for k, v in headers.items() if k.lower() == "etag"), None)
    else:
        body, etag = out, None
    if code != 0:
        sys.stderr.write(f"run_watch: job page unavailable: {err.strip()[:300]}\n")
        return None
    try:
        data = json.loads(body)
    except json.JSONDecodeError:
        return None
    if etag:
        _ETAGS[cache_key] = (etag, data)
    return data


def run_metadata(run_id: int, repo: str | None) -> dict | None:
    args = ["run", "view", str(run_id), "--json", "status,conclusion,attempt,url,event"]
    if repo:
        args += ["--repo", repo]
    code, out, err = gh(*args)
    if code != 0 or "clud: gh read broker: rate-limit reserve" in err:
        sys.stderr.write(f"run_watch: run metadata unavailable: {err.strip()[:300]}\n")
        return None
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        return None
    return data if isinstance(data, dict) else None


def valid_job(job: object, seen: set[int]) -> bool:
    """Validate each job before accepting a page into the snapshot."""
    if not isinstance(job, dict):
        return False
    job_id = job.get("id")
    if not isinstance(job_id, int) or isinstance(job_id, bool) or job_id in seen:
        return False
    status = job.get("status")
    if not isinstance(status, str):
        return False
    if status == "completed":
        conclusion = job.get("conclusion")
        if not isinstance(conclusion, str) or conclusion not in (
            FAILED | {"success", "neutral", "skipped"}
        ):
            return False
    seen.add(job_id)
    return True


def validate_page(data: object, expected: int | None, seen: set[int]) -> tuple[int, list] | None:
    """Reject malformed, oversized, duplicate, or changing job pages."""
    if not isinstance(data, dict) or not isinstance(data.get("jobs"), list):
        return None
    count, batch = data.get("total_count"), data["jobs"]
    if (
        not isinstance(count, int)
        or isinstance(count, bool)
        or count < 0
        or (expected is not None and count != expected)
        or len(batch) > PAGE_SIZE
    ):
        return None
    if any(not valid_job(job, seen) for job in batch):
        return None
    return count, batch


def job_pages(repo: str, run_id: int, attempt: int) -> tuple[list[dict] | None, str]:
    """Retrieve every page for one exact attempt; fail closed on partial data."""
    parts = repo.split("/")
    hostname = parts[0] if len(parts) == 3 else None
    if hostname:
        repo = "/".join(parts[1:])
    jobs: list[dict] = []
    seen: set[int] = set()
    expected: int | None = None
    for page in range(1, MAX_PAGES + 1):
        path = (
            f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs"
            f"?per_page={PAGE_SIZE}&page={page}"
        )
        data = api_get(path, hostname) if hostname else api_get(path)
        result = validate_page(data, expected, seen)
        if result is None:
            return None, f"job page {page} unavailable, invalid, or inconsistent"
        expected, batch = result
        jobs.extend(batch)
        if len(jobs) > expected:
            return None, "job pages exceed the declared count"
        # A confirmed failure is conclusive even when later pages are unavailable.
        if any(
            job.get("status") == "completed" and job.get("conclusion") in FAILED for job in batch
        ):
            return jobs, ""
        if len(jobs) == expected:
            return jobs, ""
        if len(batch) != PAGE_SIZE:
            return None, "job pages are incomplete"
    return None, "job page limit exceeded"


def resolve_repo() -> str | None:
    """Discover the repository and retain its GitHub Enterprise authority."""
    code, out, _ = gh("repo", "view", "--json", "nameWithOwner,url")
    if code != 0:
        return None
    try:
        data = json.loads(out)
        owner_repo = data["nameWithOwner"]
        parsed_url = urlparse(data["url"])
        hostname = parsed_url.hostname
        if not isinstance(owner_repo, str) or owner_repo.count("/") != 1 or not hostname:
            return None
        if parsed_url.port is not None:
            hostname = f"{hostname}:{parsed_url.port}"
    except (json.JSONDecodeError, KeyError, TypeError, ValueError):
        return None
    return owner_repo if hostname == "github.com" else f"{hostname}/{owner_repo}"


def fetch(run_id: int, repo: str | None) -> dict | None:
    """Read a complete, internally consistent snapshot of the current attempt."""
    repo = repo or resolve_repo()
    if not repo:
        return None
    run = run_metadata(run_id, repo)
    if not run or not isinstance(run.get("attempt"), int) or run["attempt"] < 1:
        return None
    valid_statuses = {"queued", "in_progress", "completed", "waiting", "pending", "requested"}
    if not isinstance(run.get("status"), str) or run["status"] not in valid_statuses:
        return None
    jobs, reason = job_pages(repo, run_id, run["attempt"])
    if jobs is None:
        sys.stderr.write(
            f"run_watch: run {run_id} attempt {run['attempt']} "
            f"status={run['status']}: {reason}; retrying\n"
        )
        return None
    after = run_metadata(run_id, repo)
    if (
        not after
        or after.get("attempt") != run["attempt"]
        or not isinstance(after.get("status"), str)
        or after["status"] not in valid_statuses
    ):
        return None
    confirmed_failure = any(
        job["status"] == "completed" and job.get("conclusion") in FAILED for job in jobs
    )
    if not confirmed_failure:
        if any(after.get(key) != run.get(key) for key in ("status", "conclusion")):
            return None
        if after["status"] == "completed" and any(job["status"] != "completed" for job in jobs):
            return None
    after["jobs"] = jobs
    return after


def report_terminal(run_id: int, repo: str | None, cancel_requested: bool, run: dict) -> int | None:
    """Report a conclusive snapshot, optionally cancelling a failed active run."""
    state, failed = evaluate(run)
    if state == "pending":
        return None
    print(f"run {run_id} ({run.get('event', '?')}) {run.get('url', '')}")
    print(render_jobs(run.get("jobs") or []))
    if state == "success":
        print(f"GREEN  run {run_id} succeeded")
        return EXIT_OK
    reason = (
        f"{len(failed)} failed jobs"
        if failed
        else f"run conclusion {run.get('conclusion', 'unknown')}"
    )
    print(f"RED    run {run_id}: {reason}; inspect run URL")
    for job in failed[:MAX_FAILED_REPORTS]:
        report_failed_job(job, repo)
    if cancel_requested and run.get("status") != "completed":
        cancel = ["run", "cancel", str(run_id)]
        if repo:
            cancel += ["--repo", repo]
        code, _, err = gh(*cancel)
        if code == 0:
            print(f"cancelled run {run_id}")
        else:
            sys.stderr.write(f"run_watch: cancellation failed: {err.strip()[:300]}\n")
    return EXIT_FAILED


def main(argv: list[str] | None = None) -> int:
    ns = parse_args(sys.argv[1:] if argv is None else argv)
    deadline = time.monotonic() + ns.timeout
    global _WATCH_DEADLINE
    _WATCH_DEADLINE = deadline
    misses = 0
    while True:
        if time.monotonic() >= deadline:
            print(
                f"TIMEOUT run {ns.run_id}: watch deadline reached; re-run with the same arguments"
            )
            return EXIT_TIMEOUT
        run = fetch(ns.run_id, ns.repo)
        if run is None:
            misses += 1
            if misses >= 3 and time.monotonic() < deadline:
                sys.stderr.write(
                    "run_watch: observations unavailable; re-run with the same arguments\n"
                )
                return EXIT_GH
        else:
            misses = 0
            verdict = report_terminal(ns.run_id, ns.repo, ns.cancel, run)
            if verdict is not None:
                return verdict
        if time.monotonic() >= deadline:
            print(
                f"TIMEOUT run {ns.run_id}: watch deadline reached; re-run with the same arguments"
            )
            return EXIT_TIMEOUT
        time.sleep(min(ns.interval, max(1.0, deadline - time.monotonic())))


if __name__ == "__main__":
    sys.exit(main())
