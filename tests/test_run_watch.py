"""Unit coverage for the bundled `run_watch.py` waiter (#1941)."""

from __future__ import annotations

import json
import sys
from pathlib import Path

TOOL_DIR = (
    Path(__file__).resolve().parents[1] / "crates" / "clud-bin" / "assets" / "tools" / "github"
)


def _load():
    if str(TOOL_DIR) not in sys.path:
        sys.path.insert(0, str(TOOL_DIR))
    import run_watch

    return run_watch


run_watch = _load()


def _job(name: str, status: str, conclusion: str = "") -> dict:
    return {"name": name, "status": status, "conclusion": conclusion}


def test_a_failed_job_fails_fast_while_others_still_run() -> None:
    run = {
        "status": "in_progress",
        "jobs": [_job("build", "completed", "failure"), _job("test", "in_progress")],
    }
    state, failed = run_watch.evaluate(run)
    assert state == "failed"
    assert [job["name"] for job in failed] == ["build"]


def test_an_in_progress_run_without_failures_is_pending() -> None:
    run = {"status": "in_progress", "jobs": [_job("build", "completed", "success")]}
    assert run_watch.evaluate(run) == ("pending", [])


def test_a_clean_completed_run_is_success_and_skipped_jobs_do_not_fail_it() -> None:
    run = {
        "status": "completed",
        "conclusion": "success",
        "jobs": [_job("build", "completed", "success"), _job("mac", "completed", "skipped")],
    }
    assert run_watch.evaluate(run) == ("success", [])


def test_a_run_that_concluded_badly_with_no_failed_job_still_fails() -> None:
    run = {"status": "completed", "conclusion": "cancelled", "jobs": []}
    assert run_watch.evaluate(run) == ("failed", [])


def test_fetch_paginates_current_attempt_and_finds_failure_on_later_page(monkeypatch) -> None:
    jobs = [dict(_job(f"job-{i}", "completed", "success"), id=i + 1) for i in range(116)]
    jobs.append(dict(_job("later-failure", "completed", "failure"), id=117))
    seen = []

    def fake_gh(*args):
        seen.append(args)
        if args[:2] == ("run", "view"):
            if "status,conclusion,jobs,url,event" in args:
                return 1, "", "HTTP 502: jobs?per_page=100"
            return 0, json.dumps({"status": "in_progress", "conclusion": "", "attempt": 2}), ""
        if args[0] == "api":
            url = args[2]
            assert "/attempts/2/jobs?per_page=30&page=" in url
            page = int(url.rsplit("=", 1)[1])
            data = {"total_count": len(jobs), "jobs": jobs[(page - 1) * 30 : page * 30]}
            return 0, json.dumps(data), ""
        raise AssertionError(args)

    monkeypatch.delenv("CLUD_GH_READ_BROKER", raising=False)
    monkeypatch.setattr(run_watch, "gh", fake_gh)
    run = run_watch.fetch(123, "owner/repo")
    assert run is not None
    assert run_watch.evaluate(run)[1][0]["name"] == "later-failure"
    assert sum(call[0] == "api" for call in seen) == 4


def test_fetch_rejects_incomplete_job_pages(monkeypatch, capsys) -> None:
    def fake_gh(*args):
        if args[:2] == ("run", "view"):
            return 0, json.dumps({"status": "completed", "conclusion": "success", "attempt": 2}), ""
        return 0, json.dumps({"total_count": 31, "jobs": []}), ""

    monkeypatch.setattr(run_watch, "gh", fake_gh)
    assert run_watch.fetch(123, "owner/repo") is None
    assert "attempt 2 status=completed" in capsys.readouterr().err


def test_fetch_rejects_attempt_changed_during_pagination(monkeypatch) -> None:
    snapshots = iter(
        [
            {"status": "completed", "conclusion": "success", "attempt": 2},
            {"status": "completed", "conclusion": "success", "attempt": 3},
        ]
    )
    monkeypatch.setattr(run_watch, "run_metadata", lambda *_: next(snapshots))
    monkeypatch.setattr(run_watch, "api_get", lambda *_: {"total_count": 0, "jobs": []})
    assert run_watch.fetch(123, "owner/repo") is None


def test_fetch_rejects_duplicate_job_ids(monkeypatch) -> None:
    run = {"status": "completed", "conclusion": "success", "attempt": 2}
    monkeypatch.setattr(run_watch, "run_metadata", lambda *_: run.copy())
    jobs = [dict(_job("same", "completed", "success"), id=1)] * 2
    monkeypatch.setattr(run_watch, "api_get", lambda *_: {"total_count": 2, "jobs": jobs})
    assert run_watch.fetch(123, "owner/repo") is None


def test_unknown_run_conclusion_is_not_success() -> None:
    run = {"status": "completed", "conclusion": "", "jobs": []}
    assert run_watch.evaluate(run) == ("pending", [])


def test_malformed_conclusions_do_not_crash_or_claim_success(monkeypatch) -> None:
    for conclusion in ([], {"invalid": True}):
        run = {"status": "completed", "conclusion": conclusion, "jobs": []}
        assert run_watch.evaluate(run) == ("pending", [])
        metadata = {"status": "completed", "conclusion": "success", "attempt": 2}
        monkeypatch.setattr(
            run_watch, "run_metadata", lambda *_, metadata=metadata: metadata.copy()
        )
        job = {"id": 1, "name": "bad", "status": "completed", "conclusion": conclusion}
        monkeypatch.setattr(
            run_watch, "api_get", lambda *_, job=job: {"total_count": 1, "jobs": [job]}
        )
        assert run_watch.fetch(123, "owner/repo") is None


def test_fetch_rejects_completed_job_with_unknown_conclusion(monkeypatch) -> None:
    run = {"status": "completed", "conclusion": "success", "attempt": 2}
    monkeypatch.setattr(run_watch, "run_metadata", lambda *_: run.copy())
    jobs = [dict(_job("incomplete", "completed"), id=1)]
    monkeypatch.setattr(run_watch, "api_get", lambda *_: {"total_count": 1, "jobs": jobs})
    assert run_watch.fetch(123, "owner/repo") is None


def test_transient_page_failure_recovers_and_cancel_is_opt_in(monkeypatch, capsys) -> None:
    failure = {
        "status": "in_progress",
        "attempt": 2,
        "jobs": [_job("bad", "completed", "failure")],
    }
    calls = []

    def fake_gh(*args):
        calls.append(args)
        return 0, "", ""

    monkeypatch.setattr(run_watch, "gh", fake_gh)
    monkeypatch.setattr(run_watch.time, "sleep", lambda _: None)
    samples = iter([None, failure])
    monkeypatch.setattr(run_watch, "fetch", lambda *_: next(samples))
    assert run_watch.main(["123", "--repo", "owner/repo"]) == run_watch.EXIT_FAILED
    assert not calls
    assert "bad" in capsys.readouterr().out

    samples = iter([None, failure])
    assert run_watch.main(["123", "--repo", "owner/repo", "--cancel"]) == run_watch.EXIT_FAILED
    assert calls == [("run", "cancel", "123", "--repo", "owner/repo")]


def test_expired_watch_does_not_start_an_api_call(monkeypatch, capsys) -> None:
    calls = []
    monkeypatch.setattr(run_watch, "fetch", lambda *_: calls.append(True))
    assert run_watch.main(["123", "--timeout", "0"]) == run_watch.EXIT_TIMEOUT
    assert not calls
    assert "TIMEOUT" in capsys.readouterr().out


def test_failed_cancel_is_not_reported_as_success(monkeypatch, capsys) -> None:
    failure = {"status": "in_progress", "jobs": [_job("bad", "completed", "failure")]}
    monkeypatch.setattr(run_watch, "fetch", lambda *_: failure)
    monkeypatch.setattr(run_watch, "gh", lambda *_: (1, "", "permission denied"))
    assert run_watch.main(["123", "--cancel"]) == run_watch.EXIT_FAILED
    out, err = capsys.readouterr()
    assert "cancelled run" not in out
    assert "cancellation failed" in err


def test_api_get_revalidates_cached_page_and_rejects_broker_stale(monkeypatch) -> None:
    run_watch._ETAGS.clear()
    calls = []
    replies = iter(
        [
            (0, 'HTTP/2 200 OK\r\nETag: "one"\r\n\r\n{"jobs": []}', ""),
            (1, "HTTP/2 304 Not Modified\r\n\r\n", ""),
        ]
    )

    def fake_gh(*args):
        calls.append(args)
        return next(replies)

    monkeypatch.delenv("CLUD_GH_READ_BROKER", raising=False)
    monkeypatch.setattr(run_watch, "gh", fake_gh)
    assert run_watch.api_get("repos/o/r/jobs") == {"jobs": []}
    assert run_watch.api_get("repos/o/r/jobs") == {"jobs": []}
    assert calls[1][-1] == 'If-None-Match: "one"'
    monkeypatch.setenv("CLUD_GH_READ_BROKER", "1")
    monkeypatch.setenv("CLUD_GH_SHIM_TARGET", "gh")
    monkeypatch.setattr(
        run_watch, "gh", lambda *_: (0, '{"jobs": []}', "clud: gh read broker: rate-limit reserve")
    )
    assert run_watch.api_get("repos/o/r/jobs") is None
    run_watch._ETAGS.clear()


def test_gh_call_respects_remaining_watch_deadline(monkeypatch) -> None:
    from types import SimpleNamespace

    observed = []

    def fake_run(*args, **kwargs):
        observed.append(kwargs["timeout"])
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(run_watch, "_WATCH_DEADLINE", 100.0)
    monkeypatch.setattr(run_watch.time, "monotonic", lambda: 98.0)
    monkeypatch.setattr(run_watch.RunningProcess, "run", fake_run)
    assert run_watch.gh("api", "repos/o/r") == (0, "", "")
    assert observed == [2.0]


def test_repeated_unavailable_observations_exit_resumably(monkeypatch, capsys) -> None:
    calls = []
    monkeypatch.setattr(run_watch, "fetch", lambda *_: calls.append(True))
    monkeypatch.setattr(run_watch.time, "sleep", lambda _: None)
    assert run_watch.main(["123"]) == run_watch.EXIT_GH
    assert len(calls) == 3
    assert "re-run with the same arguments" in capsys.readouterr().err


def test_render_jobs_bounds_noise_and_keeps_failed_job() -> None:
    jobs = [_job(f"passing-{i}", "completed", "success") for i in range(500)]
    jobs.append(_job("failing-build", "completed", "failure"))
    rendered = run_watch.render_jobs(jobs)
    assert "501 jobs observed; 1 failed" in rendered
    assert "failing-build" in rendered
    assert "passing-" not in rendered
    assert len(rendered.encode()) < 1000


def test_complete_snapshot_can_report_success(monkeypatch) -> None:
    run = {"status": "completed", "conclusion": "success", "attempt": 2}
    jobs = [dict(_job("passed", "completed", "success"), id=1)]
    monkeypatch.setattr(run_watch, "run_metadata", lambda *_: run.copy())
    monkeypatch.setattr(run_watch, "api_get", lambda *_: {"total_count": 1, "jobs": jobs})
    result = run_watch.fetch(123, "owner/repo")
    assert result is not None
    assert run_watch.evaluate(result) == ("success", [])


def test_missing_gh_returns_resumable_error(monkeypatch) -> None:
    def missing(*_args, **_kwargs):
        raise FileNotFoundError("gh")

    monkeypatch.setattr(run_watch, "_WATCH_DEADLINE", None)
    monkeypatch.setattr(run_watch.RunningProcess, "run", missing)
    code, _, err = run_watch.gh("api", "repos/o/r")
    assert code == 127
    assert "could not start gh" in err


def test_render_many_long_failed_jobs_is_bounded() -> None:
    jobs = [_job("x" * 5000, "completed", "failure") for _ in range(100)]
    result = run_watch.render_jobs(jobs)
    assert "92 additional failed jobs omitted" in result
    assert len(result.encode()) < 2000


def test_fetch_rejects_a_page_larger_than_requested(monkeypatch) -> None:
    run = {"status": "completed", "conclusion": "success", "attempt": 2}
    monkeypatch.setattr(run_watch, "run_metadata", lambda *_: run.copy())
    jobs = [dict(_job(f"job-{i}", "completed", "success"), id=i + 1) for i in range(31)]
    monkeypatch.setattr(run_watch, "api_get", lambda *_: {"total_count": 31, "jobs": jobs})
    assert run_watch.fetch(123, "owner/repo") is None


def test_stale_job_does_not_produce_green() -> None:
    run = {
        "status": "completed",
        "conclusion": "success",
        "jobs": [_job("stale", "completed", "stale")],
    }
    assert run_watch.evaluate(run)[0] == "failed"


def test_confirmed_failure_does_not_wait_for_later_unavailable_pages(monkeypatch) -> None:
    run = {"status": "in_progress", "conclusion": "", "attempt": 2}
    pages = []
    jobs = [dict(_job(f"job-{i}", "completed", "success"), id=i + 1) for i in range(30)]
    jobs[0]["conclusion"] = "failure"

    def page(path):
        pages.append(path)
        return {"total_count": 117, "jobs": jobs} if len(pages) == 1 else None

    monkeypatch.setattr(run_watch, "run_metadata", lambda *_: run.copy())
    monkeypatch.setattr(run_watch, "api_get", page)
    result = run_watch.fetch(123, "owner/repo")
    assert result is not None
    assert run_watch.evaluate(result)[0] == "failed"
    assert len(pages) == 1


def test_render_rejects_terminal_controls_and_bounds_unicode() -> None:
    name = "\x1b[31m" + "🚀" * 5000 + "\n"
    jobs = [_job(name, "completed", "failure") for _ in range(100)]
    result = run_watch.render_jobs(jobs)
    assert "\x1b" not in result
    assert "92 additional failed jobs omitted" in result
    assert len(result.encode()) < 6000


def test_confirmed_failure_survives_run_completing_during_read(monkeypatch) -> None:
    snapshots = iter(
        [
            {"status": "in_progress", "conclusion": "", "attempt": 2},
            {"status": "completed", "conclusion": "failure", "attempt": 2},
        ]
    )
    jobs = [
        dict(_job("failed", "completed", "failure"), id=1),
        dict(_job("still-updating", "in_progress"), id=2),
    ]
    monkeypatch.setattr(run_watch, "run_metadata", lambda *_: next(snapshots))
    monkeypatch.setattr(run_watch, "api_get", lambda *_: {"total_count": 2, "jobs": jobs})
    result = run_watch.fetch(123, "owner/repo")
    assert result is not None
    assert result["status"] == "completed"
    assert run_watch.evaluate(result)[0] == "failed"


def test_enterprise_repo_host_is_preserved_explicitly_and_when_inferred(monkeypatch) -> None:
    run = {"status": "completed", "conclusion": "success", "attempt": 2}
    calls = []

    def fake_gh(*args):
        calls.append(args)
        if args[:2] == ("repo", "view"):
            return 0, json.dumps({"nameWithOwner": "o/r", "url": "https://ghe.example.com/o/r"}), ""
        if args[:2] == ("run", "view"):
            assert "ghe.example.com/o/r" in args
            return 0, json.dumps(run), ""
        assert args[0] == "api"
        assert "--hostname" in args
        assert "ghe.example.com" in args
        assert "repos/o/r/actions/runs/123/attempts/2/jobs?per_page=30&page=1" in args
        return 0, json.dumps({"total_count": 0, "jobs": []}), ""

    monkeypatch.delenv("CLUD_GH_READ_BROKER", raising=False)
    monkeypatch.setattr(run_watch, "gh", fake_gh)
    for repo in ("ghe.example.com/o/r", None):
        result = run_watch.fetch(123, repo)
        assert result is not None
        assert run_watch.evaluate(result) == ("success", [])
    assert len([args for args in calls if args[0] == "api"]) == 2


def test_inferred_enterprise_host_preserves_port(monkeypatch) -> None:
    run = {"status": "completed", "conclusion": "success", "attempt": 2}

    def fake_gh(*args):
        if args[:2] == ("repo", "view"):
            data = {"nameWithOwner": "o/r", "url": "https://ghe.example.com:8443/o/r"}
            return 0, json.dumps(data), ""
        if args[:2] == ("run", "view"):
            assert "ghe.example.com:8443/o/r" in args
            return 0, json.dumps(run), ""
        assert "--hostname" in args
        assert "ghe.example.com:8443" in args
        return 0, json.dumps({"total_count": 0, "jobs": []}), ""

    monkeypatch.delenv("CLUD_GH_READ_BROKER", raising=False)
    monkeypatch.setattr(run_watch, "gh", fake_gh)
    assert run_watch.fetch(123, None) is not None


def test_fetch_rejects_malformed_status(monkeypatch) -> None:
    for status in ([], {"bad": True}):
        run = {"status": status, "conclusion": "success", "attempt": 2}
        monkeypatch.setattr(run_watch, "run_metadata", lambda *_, run=run: run)
        assert run_watch.fetch(123, "o/r") is None


def test_failed_steps_names_only_the_steps_that_concluded_badly() -> None:
    job = {
        "name": "build",
        "steps": [
            {"name": "checkout", "conclusion": "success"},
            {"name": "compile", "conclusion": "failure"},
            {"name": "upload", "conclusion": "skipped"},
        ],
    }
    assert run_watch.failed_steps(job) == ["compile"]
    assert run_watch.failed_steps({"name": "x"}) == []


def test_log_tail_keeps_the_last_non_empty_lines() -> None:
    log = "\n".join(f"line {i}" for i in range(100)) + "\n\n"
    tail = run_watch.log_tail(log, lines=3)
    assert tail == "line 97\nline 98\nline 99"


def test_interval_is_clamped_to_the_minimum() -> None:
    assert run_watch.parse_args(["1", "--interval", "5"]).interval == run_watch.MIN_INTERVAL
    assert run_watch.parse_args(["1", "--interval", "90"]).interval == 90.0


def test_a_failed_job_report_prints_step_and_log_tail(monkeypatch, capsys) -> None:
    calls = []

    def fake_gh(*args):
        calls.append(args)
        return 0, "noise\nerror: boom\n", ""

    monkeypatch.setattr(run_watch, "gh", fake_gh)
    job = {
        "name": "build",
        "databaseId": 42,
        "steps": [{"name": "compile", "conclusion": "failure"}],
    }
    run_watch.report_failed_job(job, "o/r")
    out = capsys.readouterr().out
    assert "FAILED job: build" in out
    assert "failed step(s): compile" in out
    assert "error: boom" in out
    assert calls == [("run", "view", "--job", "42", "--log-failed", "--repo", "o/r")]


def test_a_rest_job_report_uses_its_id_and_bounds_each_log_line(monkeypatch, capsys) -> None:
    calls = []

    def fake_gh(*args):
        calls.append(args)
        return 0, "x" * 5000 + "\n", ""

    monkeypatch.setattr(run_watch, "gh", fake_gh)
    run_watch.report_failed_job({"name": "build", "id": 7, "steps": []}, None)
    out = capsys.readouterr().out
    assert calls == [("run", "view", "--job", "7", "--log-failed")]
    assert max(len(line) for line in out.splitlines()) <= run_watch.LOG_LINE_CHARS


def test_a_red_run_reports_at_most_three_failed_jobs(monkeypatch, capsys) -> None:
    reported = []
    monkeypatch.setattr(run_watch, "report_failed_job", lambda job, repo: reported.append(job))
    jobs = [dict(_job(f"job-{i}", "completed", "failure"), id=i + 1) for i in range(5)]
    run = {"status": "completed", "conclusion": "failure", "jobs": jobs}
    assert run_watch.report_terminal(1, "o/r", False, run) == run_watch.EXIT_FAILED
    assert len(reported) == run_watch.MAX_FAILED_REPORTS
