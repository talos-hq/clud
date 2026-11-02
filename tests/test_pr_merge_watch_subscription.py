"""#1743 phase 3: the watcher waits on the session read broker between polls.

Inside a clud session whose gh reads go through the daemon's read broker,
`pr_merge_watch.py` blocks on `POST /gh/watch` instead of sleeping, and
re-polls only when a watched read changed (or a heartbeat passed). The exit
codes are the polling watcher's. These tests drive the client against a fake
daemon listener, and `watch()` against a fake subscription.
"""

from __future__ import annotations

import http.server
import importlib.util
import json
import re
import sys
import threading
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "crates" / "clud-bin" / "assets" / "tools" / "github" / "pr_merge_watch.py"
TOKEN = "test-capability"


@pytest.fixture
def watcher():
    name = "clud_test_pr_merge_watch_subscription"
    spec = importlib.util.spec_from_file_location(name, SCRIPT)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    try:
        yield module
    finally:
        sys.modules.pop(name, None)


def test_forwarded_env_mirrors_the_broker(watcher) -> None:
    """The watch must read as the same identity as its own brokered reads,
    or it would watch other cache keys than the ones its polls fill."""
    source = (ROOT / "crates" / "clud-bin" / "src" / "gh_broker" / "mod.rs").read_text()
    block = source.split("pub const FORWARDED_ENV: &[&str] = &[", 1)[1].split("];", 1)[0]
    assert tuple(re.findall(r'"([^"]+)"', block)) == watcher.BROKER_FORWARDED_ENV


class FakeDaemon:
    """`/gh/watch` with a scripted list of replies, recording each request."""

    def __init__(self, state_dir: Path, replies: list[dict]) -> None:
        self.requests: list[dict] = []
        self.replies = list(replies)
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length))
                if f"clud_dashboard_token={TOKEN}" not in self.headers.get("Cookie", ""):
                    self.send_response(403)
                    self.end_headers()
                    return
                outer.requests.append(body)
                reply = outer.replies.pop(0) if outer.replies else {"digests": [], "changed": []}
                data = json.dumps(reply).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_args) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        port = self.server.server_address[1]
        (state_dir / "daemon.json").write_text(
            json.dumps({"dashboard_port": port, "dashboard_token": TOKEN})
        )
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self) -> None:
        self.server.shutdown()


def session_env(state: Path) -> dict[str, str]:
    return {
        "CLUD_GH_READ_BROKER": "1",
        "CLUD_GH_SHIM_TARGET": "/usr/bin/gh",
        "CLUD_DAEMON_STATE_DIR": str(state),
        "CLUD_SESSION_ID": "s1",
        "GH_TOKEN": "t0ken",
        "UNRELATED": "x",
    }


def test_no_subscription_outside_a_broker_session(watcher, tmp_path: Path) -> None:
    assert watcher.BrokerSubscription.from_env({}) is None
    env = session_env(tmp_path)
    assert watcher.BrokerSubscription.from_env({**env, "CLUD_GH_READ_BROKER": "0"}) is None
    # No daemon.json: no daemon to wait on.
    assert watcher.BrokerSubscription.from_env(env) is None


def test_wait_takes_a_baseline_then_blocks_until_a_change(watcher, tmp_path: Path) -> None:
    daemon = FakeDaemon(
        tmp_path,
        [
            {"digests": ["a", "b"], "changed": [0, 1]},
            {"digests": ["a", "b"], "changed": []},
            {"digests": ["a", "c"], "changed": [1]},
        ],
    )
    try:
        sub = watcher.BrokerSubscription.from_env(session_env(tmp_path))
        assert sub is not None
        assert sub.wait(["k1", "k2"], 120) == "changed"
    finally:
        daemon.close()
    baseline, quiet, woke = daemon.requests
    assert baseline["seen"] == []
    assert baseline["wait_ms"] == 0
    assert quiet["seen"] == ["a", "b"]
    assert quiet["endpoints"] == ["k1", "k2"]
    assert 0 < quiet["wait_ms"] <= watcher.BROKER_WAIT_SEC * 1000
    assert woke["seen"] == ["a", "b"]
    # Only the identity keys go to the daemon, and the session id.
    assert baseline["env"] == [["GH_TOKEN", "t0ken"]]
    assert baseline["gh"] == "/usr/bin/gh"
    assert baseline["session_id"] == "s1"
    assert sub.seen == ["a", "c"]


def test_a_dead_daemon_falls_back_to_polling(watcher, tmp_path: Path, monkeypatch) -> None:
    (tmp_path / "daemon.json").write_text(
        json.dumps({"dashboard_port": 9, "dashboard_token": TOKEN})
    )
    sub = watcher.BrokerSubscription.from_env(session_env(tmp_path))
    assert sub is not None
    assert sub.wait(["k1"], 30) == "unavailable"
    slept: list = []
    monkeypatch.setattr(watcher, "_sleep_remaining_interval", lambda *args: slept.append(args))
    # The deadline is on time.monotonic(), which counts from host boot: an
    # absolute 10_000.0 is already past on any host up for ~3 h (act shares
    # the host's clock), turning the wait into 0 s and skipping the fallback.
    watcher._await_next_poll(sub, ["k1"], 0.0, 20, time.monotonic() + 10_000.0, None)
    assert slept, "unavailable means the ordinary interval sleep"


class FakeSubscription:
    """Stands in for the broker: every wait reports a change at once."""

    def __init__(self) -> None:
        self.waits: list[tuple[list[str], float]] = []
        self.deferred_until = None

    def wait(self, keys: list[str], wait_sec: float) -> str:
        self.waits.append((keys, wait_sec))
        return "changed"


def gate(watcher, rows: list, *, mergeable: str = "MERGEABLE"):
    check_runs = []
    for number, (name, status, conclusion) in enumerate(rows, start=1):
        check_runs.append(
            {
                "id": number,
                "name": name,
                "head_sha": "abc123",
                "status": status,
                "conclusion": conclusion,
                "details_url": f"https://github.com/o/r/actions/runs/900/job/{number}",
            }
        )
    done = all(status == "completed" for _, status, _ in rows)
    run = {
        "id": 900,
        "run_number": 1,
        "path": ".github/workflows/ci.yml",
        "head_sha": "abc123",
        "event": "pull_request",
        "status": "completed" if done else "in_progress",
        "conclusion": "success" if done else None,
        "created_at": "2026-10-02T00:00:00Z",
    }
    rollup = [
        watcher.CheckRow(
            name, "pending" if status != "completed" else "pass", (conclusion or "").upper()
        )
        for name, status, conclusion in rows
    ]
    return watcher.GateSnapshot(
        pr=watcher.PRSnapshot(527, "OPEN", mergeable, "abc123", "main"),
        checks=rollup,
        human_review_ids=frozenset(),
        coderabbit_probe=watcher.CodeRabbitProbe("not_detected", 0),
        coderabbit=watcher.CodeRabbitObservation("quiet"),
        head_checks=watcher.HeadChecks(check_runs, [run]),
    )


def drive(watcher, tmp_path, monkeypatch, polls: list, sub, *, max_queued=None, broker_wait=True):
    """Run `watch()` over scripted gate snapshots with `sub` as the broker."""
    log = watcher.WatchLog.create(527, "o/r", root=tmp_path)
    monkeypatch.setattr(
        watcher.PRSnapshot,
        "fetch",
        lambda *_args: watcher.PRSnapshot(527, "OPEN", "MERGEABLE", "abc123", "main"),
    )
    monkeypatch.setattr(watcher, "fetch_required_check_names", lambda *args: {"unit"})
    monkeypatch.setattr(watcher, "emit_progress_report", lambda *args: None)
    monkeypatch.setattr(
        watcher,
        "_build_failure_report",
        lambda check, _repo: watcher.FailureReport(check, None, "", None),
    )
    monkeypatch.setattr(watcher, "gh_json", lambda *args: None)
    monkeypatch.setattr(watcher, "cancel_pr_runs", lambda *args, **kwargs: 0)
    monkeypatch.setattr(watcher.BrokerSubscription, "from_env", classmethod(lambda cls: sub))
    sleeps: list = []
    monkeypatch.setattr(watcher, "_sleep_remaining_interval", lambda *args: sleeps.append(args))
    seen = {"polls": 0}

    def gates(*_args, **_kwargs):
        index = min(seen["polls"], len(polls) - 1)
        seen["polls"] += 1
        return polls[index](watcher)

    monkeypatch.setattr(watcher, "fetch_gate_snapshot", gates)
    opts = watcher.CancelOptions({"fail"}, "runs", 30, False, False, True, False)
    with pytest.raises(SystemExit) as exc:
        watcher.watch(
            527, "o/r", 20, 3600, None, opts, log, max_queued=max_queued, broker_wait=broker_wait
        )
    return exc.value.code, seen["polls"], sleeps


PENDING = [("unit", "in_progress", None)]
FAILED = [("unit", "completed", "failure")]
GREEN = [("unit", "completed", "success")]


def test_a_required_failure_still_exits_1_on_the_poll_after_the_wake(
    watcher, tmp_path, monkeypatch
) -> None:
    sub = FakeSubscription()
    code, polls, sleeps = drive(
        watcher,
        tmp_path,
        monkeypatch,
        [lambda w: gate(w, PENDING), lambda w: gate(w, PENDING), lambda w: gate(w, FAILED)],
        sub,
    )
    assert code == watcher.EXIT_REQUIRED_FAIL
    assert polls == 3
    # Between polls the watch waited on the broker, never on the clock.
    assert len(sub.waits) == 2
    assert not sleeps
    keys, wait_sec = sub.waits[0]
    assert keys == watcher.broker_watch_keys("o/r", 527, "abc123")
    assert wait_sec == 20 * watcher.SUBSCRIPTION_HEARTBEAT_POLLS
    assert any("check-runs?filter=all&per_page=100&page=1" in key for key in keys)


def test_green_exits_0_and_mergeable_unknown_polls_at_the_interval(
    watcher, tmp_path, monkeypatch
) -> None:
    sub = FakeSubscription()
    code, polls, sleeps = drive(
        watcher,
        tmp_path,
        monkeypatch,
        [
            lambda w: gate(w, GREEN, mergeable="UNKNOWN"),
            lambda w: gate(w, GREEN, mergeable="UNKNOWN"),
            lambda w: gate(w, GREEN),
        ],
        sub,
    )
    assert code == watcher.EXIT_GREEN
    assert polls == 3
    # `mergeable=UNKNOWN` is counted in polls (exit 9 after six): those
    # polls keep their interval instead of waiting on the broker.
    assert len(sleeps) == 2
    assert not sub.waits


def test_a_queue_limit_keeps_interval_polling(watcher, tmp_path, monkeypatch) -> None:
    sub = FakeSubscription()
    monkeypatch.setattr(watcher, "_exit_if_queued_too_long", lambda *args: None)
    code, _polls, sleeps = drive(
        watcher,
        tmp_path,
        monkeypatch,
        [lambda w: gate(w, PENDING), lambda w: gate(w, FAILED)],
        sub,
        max_queued=600,
    )
    assert code == watcher.EXIT_REQUIRED_FAIL
    assert sleeps
    assert not sub.waits


def test_a_poll_that_read_a_stale_copy_never_decides(watcher, tmp_path, monkeypatch) -> None:
    """Below the rate-limit reserve the broker serves cached copies marked on
    stderr; the watch neither fails nor passes on them."""
    sub = FakeSubscription()
    marks = iter([1, 0, 0])

    def stale_gate(rows):
        def build(w):
            w.STALE_READS += next(marks)
            return gate(w, rows)

        return build

    code, polls, sleeps = drive(
        watcher,
        tmp_path,
        monkeypatch,
        [stale_gate(FAILED), stale_gate(PENDING), stale_gate(FAILED)],
        sub,
    )
    assert code == watcher.EXIT_REQUIRED_FAIL
    assert polls == 3, "the stale failure on the first poll was not acted on"
    assert len(sleeps) == 1


def test_gh_counts_reads_the_broker_marked_stale(watcher, monkeypatch) -> None:
    class Result:
        returncode = 0
        stdout = "{}"
        stderr = (
            "clud: gh read broker: rate-limit reserve (400 of 5000 requests left until "
            "2026-10-03T04:00:00Z): a cached copy from 40s ago, not a refresh\n"
        )

    monkeypatch.setattr(watcher.RunningProcess, "run", lambda *args, **kwargs: Result())
    before = watcher.STALE_READS
    watcher.gh("api", "repos/o/r/pulls/1")
    assert watcher.STALE_READS == before + 1


def test_rest_reads_outside_a_broker_are_conditional(watcher, monkeypatch) -> None:
    calls: list[tuple[str, ...]] = []
    replies = iter(
        [
            watcher.GhResult(0, 'HTTP/2.0 200 OK\r\nEtag: W/"e1"\r\n\r\n{"n":1}', ""),
            watcher.GhResult(
                1, 'HTTP/2.0 304 Not Modified\r\nEtag: W/"e1"\r\n\r\n', "gh: HTTP 304"
            ),
        ]
    )

    def fake_gh(*args, **_kwargs):
        calls.append(args)
        return next(replies)

    monkeypatch.setattr(watcher, "gh", fake_gh)
    monkeypatch.delenv("CLUD_GH_READ_BROKER", raising=False)
    monkeypatch.delenv("CLUD_GH_BACKEND", raising=False)
    assert watcher.gh_json("api", "repos/o/r/pulls/1") == {"n": 1}
    assert watcher.gh_json("api", "repos/o/r/pulls/1") == {"n": 1}
    assert calls[0] == ("api", "-i", "repos/o/r/pulls/1")
    assert calls[1] == ("api", "-i", "repos/o/r/pulls/1", "-H", 'If-None-Match: W/"e1"')


def test_rest_reads_in_a_broker_session_go_through_the_broker(watcher, monkeypatch) -> None:
    calls: list[tuple[str, ...]] = []
    monkeypatch.setattr(
        watcher, "gh", lambda *args, **kwargs: calls.append(args) or watcher.GhResult(0, "[]", "")
    )
    monkeypatch.setenv("CLUD_GH_READ_BROKER", "1")
    monkeypatch.setenv("CLUD_GH_SHIM_TARGET", "/usr/bin/gh")
    assert watcher.gh_json("api", "repos/o/r/pulls/1/reviews?per_page=100") == []
    # A plain read: the shim brokers it (an `-i` or `-H` read it would not).
    assert calls == [("api", "repos/o/r/pulls/1/reviews?per_page=100")]


def test_rest_reads_routed_through_ghd_are_plain(watcher, monkeypatch) -> None:
    """#2368: ghd shares a plain read across watchers; no per-process ETag."""
    calls: list[tuple[str, ...]] = []
    monkeypatch.setattr(
        watcher, "gh", lambda *args, **kwargs: calls.append(args) or watcher.GhResult(0, "{}", "")
    )
    monkeypatch.setenv("CLUD_GH_READ_BROKER", "0")
    monkeypatch.setenv("CLUD_GH_BACKEND", "ghd")
    monkeypatch.setenv("CLUD_GH_SHIM_TARGET", "/usr/bin/gh")
    assert watcher.gh_json("api", "repos/o/r/pulls/1") == {}
    assert watcher.gh_json("api", "repos/o/r/pulls/1") == {}
    assert calls == [("api", "repos/o/r/pulls/1")] * 2


def test_gh_backend_without_ghd_keeps_the_etag_path(watcher, monkeypatch) -> None:
    calls: list[tuple[str, ...]] = []
    monkeypatch.setattr(
        watcher,
        "gh",
        lambda *args, **kwargs: calls.append(args)
        or watcher.GhResult(0, 'HTTP/2.0 200 OK\r\n\r\n{"n":1}', ""),
    )
    monkeypatch.setenv("CLUD_GH_READ_BROKER", "0")
    monkeypatch.setenv("CLUD_GH_SHIM_TARGET", "/usr/bin/gh")
    for backend in ("gh", None):
        if backend is None:
            monkeypatch.delenv("CLUD_GH_BACKEND", raising=False)
        else:
            monkeypatch.setenv("CLUD_GH_BACKEND", backend)
        calls.clear()
        assert watcher.gh_json("api", "repos/o/r/pulls/2") == {"n": 1}
        assert calls[0][:2] == ("api", "-i")
    # ghd named but no gh alias in the session: nothing routes through ghd.
    monkeypatch.setenv("CLUD_GH_BACKEND", "ghd")
    monkeypatch.delenv("CLUD_GH_SHIM_TARGET")
    assert not watcher.ghd_reads()


def test_no_broker_wait_polls_at_the_interval(watcher, tmp_path, monkeypatch) -> None:
    sub = FakeSubscription()
    code, _polls, sleeps = drive(
        watcher,
        tmp_path,
        monkeypatch,
        [lambda w: gate(w, PENDING), lambda w: gate(w, FAILED)],
        sub,
        broker_wait=False,
    )
    assert code == watcher.EXIT_REQUIRED_FAIL
    assert sleeps
    assert not sub.waits
    assert watcher.parse_args(["527", "--no-broker-wait"]).broker_wait is False
    assert watcher.parse_args(["527"]).broker_wait is True
