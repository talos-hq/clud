#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "running-process==4.10.1",
# ]
# ///
# managed-by: clud
"""notify/watch.py — one watcher for N clud sessions and M PRs, with a
durable event log a chat orchestrator (Hermes) can wake on.

Why: a Telegram message reaches the *user* but does not wake the *agent*.
The agent wakes on a cron tick or a background-job completion. So this tool
writes every transition to an append-only JSONL log with a monotonically
increasing `seq`, and exposes a cursor read (`since N`) plus a stable
`tick` digest whose output only changes when a new event lands — exactly
what a Hermes `monitor_script` job hashes to decide whether to run a turn.

Usage (all via `"$CLUD_EXE" tool run notify/watch.py -- …`):
  add-session SESS_ID [--label L]     watch a daemon (API) or PTY session
  add-pr OWNER/REPO#N                 watch a PR: queued/merged/ejected/failed
  remove SUBJECT | list
  poll                                one pass; exit 2 if it emitted an error
  loop [--interval S]                 poll forever (until nothing is watched);
                                      with --telegram-env, typing while busy
  since N [--json]                    events with seq > N, then `cursor M`
  tick [--tail K]                     poll, then print the last K events
                                      (stable bytes until something changes)

Event record (one JSON object per line in events.jsonl):
  seq, ts (UTC ISO), kind, subject, detail, key
Kinds: session-sealed, pr-queued, pr-merged, pr-ejected, pr-failed,
pr-closed, checks-timed-out, error. `key` is the dedupe identity, e.g.
`session-sealed:<id>:<turn>`, `pr-merged:<repo#n>:<sha>`; errors use
`error:<source>:<subject>:<UTC hour>` so a persistent outage is reported
once an hour, never silently. A key already in the bounded `seen` set is
never appended again, so restarts and double runs do not re-announce.

State lives in $CLUD_WATCH_DIR (default ~/.clud/state/watch/).
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from contextlib import contextmanager
from datetime import UTC, datetime
from pathlib import Path

from running_process import PIPE, RunningProcess, TimeoutExpired

SEEN_LIMIT = 4096
TAIL_CHARS = 1500
TYPING_EVERY_S = 4.0
DEFAULT_QUEUE_TIMEOUT_S = 3600
TELEGRAM_API = "https://api.telegram.org/bot{token}/{method}"

PR_QUERY = """query($o:String!,$r:String!,$n:Int!){repository(owner:$o,name:$r){
pullRequest(number:$n){state merged mergedAt mergeCommit{oid} headRefOid
mergeQueueEntry{state enqueuedAt}
commits(last:1){nodes{commit{statusCheckRollup{state}}}}}}}"""


# ---------------------------------------------------------------- storage


def state_dir() -> Path:
    raw = os.environ.get("CLUD_WATCH_DIR")
    base = Path(raw) if raw else Path.home() / ".clud" / "state" / "watch"
    base.mkdir(parents=True, exist_ok=True)
    return base


@contextmanager
def locked(base: Path):
    """Serialize writers (a cron tick and a `loop` may overlap)."""
    handle = (base / "lock").open("a+")
    try:
        try:
            import fcntl

            fcntl.flock(handle, fcntl.LOCK_EX)
        except ImportError:  # no flock on this host; single-writer by convention
            pass
        yield
    finally:
        handle.close()


def load_state(base: Path) -> dict:
    path = base / "state.json"
    if path.exists():
        data = json.loads(path.read_text(encoding="utf-8"))
    else:
        data = {}
    data.setdefault("seq", 0)
    data.setdefault("seen", [])
    data.setdefault("sessions", {})
    data.setdefault("prs", {})
    data.setdefault("telegram_cursor", 0)
    return data


def save_state(base: Path, data: dict) -> None:
    data["seen"] = data["seen"][-SEEN_LIMIT:]
    tmp = base / "state.json.tmp"
    tmp.write_text(json.dumps(data, indent=1, sort_keys=True), encoding="utf-8")
    tmp.replace(base / "state.json")


def emit(base: Path, data: dict, kind: str, subject: str, detail: str, key: str) -> dict | None:
    """Append one event unless its key was already recorded. Returns it."""
    if key in data["seen"]:
        return None
    data["seq"] += 1
    event = {
        "seq": data["seq"],
        "ts": datetime.now(UTC).isoformat(timespec="seconds"),
        "kind": kind,
        "subject": subject,
        "detail": detail,
        "key": key,
    }
    with (base / "events.jsonl").open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(event, sort_keys=True) + "\n")
    data["seen"].append(key)
    return event


def emit_error(base: Path, data: dict, source: str, subject: str, detail: str) -> dict | None:
    hour = datetime.now(UTC).strftime("%Y%m%dT%H")
    print(f"watch error [{source}] {subject}: {detail}", file=sys.stderr)
    return emit(
        base, data, "error", subject, f"{source}: {detail}", f"error:{source}:{subject}:{hour}"
    )


def read_events(base: Path) -> list[dict]:
    path = base / "events.jsonl"
    if not path.exists():
        return []
    return [
        json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()
    ]


# ---------------------------------------------------------------- processes


def run(argv: list[str], timeout: int = 60) -> tuple[int, str, str]:
    try:
        res = RunningProcess.run(argv, capture_output=True, stderr=PIPE, text=True, timeout=timeout)
    except (OSError, TimeoutError, TimeoutExpired) as exc:
        return 124, "", f"{argv[0]}: {exc}"
    return res.returncode, res.stdout or "", res.stderr or ""


def clud_exe() -> str:
    return os.environ.get("CLUD_EXE") or "clud"


# ---------------------------------------------------------------- sessions


class DaemonDownError(Exception):
    pass


def daemon_info() -> tuple[str, str]:
    code, out, err = run([clud_exe(), "daemon", "api-info", "--json"], timeout=20)
    try:
        info = json.loads(out)
    except ValueError:
        raise DaemonDownError(
            f"`clud daemon api-info` failed (exit {code}): {err.strip()[:200]}"
        ) from None
    if not info.get("base_url"):
        raise DaemonDownError("`clud daemon api-info` returned no base_url")
    return info["base_url"], info.get("token", "")


def fetch_session(base_url: str, token: str, sid: str) -> dict | None:
    """The daemon record, or None on 404. Raises DaemonDownError otherwise."""
    req = urllib.request.Request(
        f"{base_url}/v1/sessions/{urllib.parse.quote(sid)}",
        headers={"Authorization": f"Bearer {token}"},
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return None
        raise DaemonDownError(f"GET /v1/sessions/{sid}: HTTP {exc.code}") from None
    except (urllib.error.URLError, TimeoutError, ValueError) as exc:
        raise DaemonDownError(f"GET /v1/sessions/{sid}: {exc}") from None


def pty_running_ids() -> set[str]:
    """Ids in the RUNNING table of `clud list` (stops at the API table)."""
    code, out, err = run([clud_exe(), "list"], timeout=30)
    if code != 0:
        raise DaemonDownError(f"`clud list` failed (exit {code}): {err.strip()[:200]}")
    return parse_pty_running(out)


def parse_pty_running(text: str) -> set[str]:
    ids: set[str] = set()
    for line in text.splitlines():
        if line.startswith("API SESSION"):
            break
        word = line.split(" ", 1)[0]
        if word.startswith("sess-"):
            ids.add(word)
    return ids


def result_tail(record: dict, turn_id: str) -> str:
    for event in reversed(record.get("events", [])):
        data = event.get("data") or {}
        if event.get("turn_id") == turn_id and isinstance(data.get("result"), str):
            text = data["result"].strip()
            return text if len(text) <= TAIL_CHARS else "…" + text[-TAIL_CHARS:]
    return ""


def session_verdict(record: dict) -> tuple[str, str, str] | None:
    """(turn_id, disposition, tail) when the latest turn sealed, else None."""
    turns = record.get("turns") or []
    if not turns:
        return None if record.get("state") == "running" else ("none", record.get("state", "?"), "")
    turn = turns[-1]
    if turn.get("state") == "running":
        return None
    disposition = turn.get("disposition") or turn.get("state", "?")
    return turn["id"], disposition, result_tail(record, turn["id"])


def check_session(
    sid: str, watch: dict, api: tuple[str, str], pty_ids: set[str]
) -> tuple[str, str, str]:
    """('running'|'sealed'|'unknown', detail, key)."""
    record = fetch_session(api[0], api[1], sid)
    if record is not None:
        verdict = session_verdict(record)
        if verdict is None:
            return "running", "", ""
        turn_id, disposition, tail = verdict
        detail = f"{watch.get('label') or sid} sealed ({disposition})" + (
            f"\n{tail}" if tail else ""
        )
        return "sealed", detail, f"session-sealed:{sid}:{turn_id}"
    if sid in pty_ids:
        return "running", "", ""
    if watch.get("seen_running"):
        return (
            "sealed",
            f"{watch.get('label') or sid} sealed (pty session exited)",
            f"session-sealed:{sid}:pty",
        )
    return "unknown", "session id not found in the daemon or `clud list`", ""


def poll_sessions(base: Path, data: dict) -> tuple[int, bool]:
    """Returns (number still running, whether an error was emitted)."""
    if not data["sessions"]:
        return 0, False
    try:
        api = daemon_info()
        pty_ids = pty_running_ids()
    except DaemonDownError as exc:
        emit_error(base, data, "daemon", "clud-daemon", str(exc))
        return len(data["sessions"]), True
    running, errored = 0, False
    for sid, watch in list(data["sessions"].items()):
        try:
            status, detail, key = check_session(sid, watch, api, pty_ids)
        except DaemonDownError as exc:
            emit_error(base, data, "daemon", sid, str(exc))
            errored = True
            continue
        if status == "running":
            watch["seen_running"] = True
            running += 1
        elif status == "sealed":
            emit(base, data, "session-sealed", sid, detail, key)
            del data["sessions"][sid]
        else:
            emit_error(base, data, "session", sid, detail)
            errored = True
    return running, errored


# ---------------------------------------------------------------- PRs


def parse_pr_subject(subject: str) -> tuple[str, str, int]:
    repo, _, num = subject.partition("#")
    owner, _, name = repo.partition("/")
    if not owner or not name or not num.isdigit():
        raise ValueError(f"PR subject must look like OWNER/REPO#N, got {subject!r}")
    return owner, name, int(num)


def fetch_pr(subject: str) -> dict:
    owner, name, num = parse_pr_subject(subject)
    code, out, err = run(
        [
            "gh",
            "api",
            "graphql",
            "-f",
            f"query={PR_QUERY}",
            "-f",
            f"o={owner}",
            "-f",
            f"r={name}",
            "-F",
            f"n={num}",
        ]
    )
    if code != 0:
        raise RuntimeError(f"gh api graphql exit {code}: {(err or out).strip()[:300]}")
    pr = ((json.loads(out).get("data") or {}).get("repository") or {}).get("pullRequest")
    if pr is None:
        raise RuntimeError("PR not found (or token lacks access)")
    return pr


def rollup_state(pr: dict) -> str:
    nodes = (pr.get("commits") or {}).get("nodes") or []
    rollup = (nodes[-1].get("commit") or {}).get("statusCheckRollup") if nodes else None
    return (rollup or {}).get("state") or ""


def pr_transitions(
    subject: str, pr: dict, watch: dict, now: float, queue_timeout: int
) -> list[tuple[str, str, str]]:
    """Pure: the (kind, detail, key) events this observation implies."""
    out: list[tuple[str, str, str]] = []
    entry = pr.get("mergeQueueEntry")
    prev_enqueued = watch.get("enqueued_at")
    if pr.get("merged"):
        sha = (pr.get("mergeCommit") or {}).get("oid", "")
        out.append(
            (
                "pr-merged",
                f"merged {sha[:12]} at {pr.get('mergedAt')}",
                f"pr-merged:{subject}:{sha}",
            )
        )
        return out
    if pr.get("state") == "CLOSED":
        out.append(
            ("pr-closed", "closed without merging", f"pr-closed:{subject}:{pr.get('headRefOid')}")
        )
        return out
    if entry:
        enq = entry.get("enqueuedAt", "")
        out.append(("pr-queued", f"entered the merge queue at {enq}", f"pr-queued:{subject}:{enq}"))
        if entry.get("state") == "UNMERGEABLE":
            out.append(
                ("pr-ejected", "merge queue entry is UNMERGEABLE", f"pr-ejected:{subject}:{enq}")
            )
        age = now - datetime.fromisoformat(enq.replace("Z", "+00:00")).timestamp() if enq else 0
        if age > queue_timeout:
            out.append(
                (
                    "checks-timed-out",
                    f"queued {int(age // 60)}m (> {queue_timeout // 60}m)",
                    f"checks-timed-out:{subject}:{enq}",
                )
            )
    elif prev_enqueued:
        out.append(
            (
                "pr-ejected",
                "left the merge queue without merging",
                f"pr-ejected:{subject}:{prev_enqueued}",
            )
        )
    if rollup_state(pr) in ("FAILURE", "ERROR"):
        head = pr.get("headRefOid", "")
        out.append(
            (
                "pr-failed",
                f"checks {rollup_state(pr)} on {head[:12]}",
                f"pr-failed:{subject}:{head}",
            )
        )
    return out


def poll_prs(base: Path, data: dict, queue_timeout: int) -> tuple[int, bool]:
    open_count, errored = 0, False
    for subject, watch in list(data["prs"].items()):
        try:
            pr = fetch_pr(subject)
        except (RuntimeError, ValueError) as exc:
            emit_error(base, data, "github", subject, str(exc))
            errored = True
            open_count += 1
            continue
        for kind, detail, key in pr_transitions(subject, pr, watch, time.time(), queue_timeout):
            emit(base, data, kind, subject, detail, key)
        entry = pr.get("mergeQueueEntry")
        watch["enqueued_at"] = entry.get("enqueuedAt") if entry else None
        if pr.get("merged") or pr.get("state") == "CLOSED":
            del data["prs"][subject]
        else:
            open_count += 1
    return open_count, errored


# ---------------------------------------------------------------- telegram


def load_env_file(path: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if not path.exists():
        return out
    for line in path.read_text(encoding="utf-8", errors="ignore").splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            out[key.strip()] = value.strip().strip('"').strip("'")
    return out


def telegram_creds(env_path: str | None) -> tuple[str, str] | None:
    if not env_path:
        return None
    env = load_env_file(Path(env_path).expanduser())
    return env.get("TELEGRAM_BOT_TOKEN", ""), env.get("TELEGRAM_HOME_CHANNEL", "")


def telegram(creds: tuple[str, str], method: str, fields: dict[str, str]) -> str | None:
    """None on success, else the error text. Plain text only: no parse_mode,
    so a worker's stray `_` or `*` cannot 400 the message."""
    payload = urllib.parse.urlencode({"chat_id": creds[1], **fields}).encode()
    req = urllib.request.Request(TELEGRAM_API.format(token=creds[0], method=method), data=payload)
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            return None if json.loads(resp.read()).get("ok") else "telegram returned ok=false"
    except (urllib.error.URLError, TimeoutError, ValueError) as exc:
        return f"telegram {method}: {exc}"


def deliver(base: Path, data: dict, creds: tuple[str, str] | None) -> bool:
    """Post every event after the delivery cursor; stop at the first failure
    so it is retried next pass instead of lost."""
    if creds is None:
        return False
    if not creds[0] or not creds[1]:
        emit_error(
            base, data, "telegram", "config", "TELEGRAM_BOT_TOKEN or TELEGRAM_HOME_CHANNEL missing"
        )
        return True
    for event in read_events(base):
        if event["seq"] <= data["telegram_cursor"]:
            continue
        text = f"[clud {event['kind']}] {event['subject']}\n{event['detail']}"[:4000]
        failure = telegram(creds, "sendMessage", {"text": text})
        if failure:
            print(f"watch error [telegram] {failure}", file=sys.stderr)
            return True
        data["telegram_cursor"] = event["seq"]
    return False


# ---------------------------------------------------------------- commands


def poll_once(args: argparse.Namespace) -> tuple[int, bool]:
    base = state_dir()
    with locked(base):
        data = load_state(base)
        running, err_s = poll_sessions(base, data)
        open_prs, err_p = poll_prs(base, data, args.queue_timeout)
        err_t = deliver(base, data, telegram_creds(args.telegram_env))
        save_state(base, data)
    return running + open_prs, err_s or err_p or err_t


def cmd_poll(args: argparse.Namespace) -> int:
    _, errored = poll_once(args)
    return 2 if errored else 0


def cmd_loop(args: argparse.Namespace) -> int:
    creds = telegram_creds(args.telegram_env)
    while True:
        active, _ = poll_once(args)
        if active == 0:
            return 0
        deadline = time.monotonic() + args.interval
        while time.monotonic() < deadline:
            if creds and creds[0] and load_state(state_dir())["sessions"]:
                telegram(creds, "sendChatAction", {"action": "typing"})
            time.sleep(min(TYPING_EVERY_S, args.interval))


def format_event(event: dict) -> str:
    return f"#{event['seq']} {event['ts']} {event['kind']} {event['subject']}: {event['detail']}"


def cmd_since(args: argparse.Namespace) -> int:
    events = [e for e in read_events(state_dir()) if e["seq"] > args.cursor]
    for event in events:
        print(json.dumps(event, sort_keys=True) if args.json else format_event(event))
    last = events[-1]["seq"] if events else args.cursor
    print(json.dumps({"cursor": last}) if args.json else f"cursor {last}")
    return 0


def cmd_tick(args: argparse.Namespace) -> int:
    _, errored = poll_once(args)
    for event in read_events(state_dir())[-args.tail :]:
        print(format_event(event))
    return 2 if errored else 0


def cmd_add_session(args: argparse.Namespace) -> int:
    base = state_dir()
    with locked(base):
        data = load_state(base)
        data["sessions"][args.session] = {"label": args.label, "added": time.time()}
        save_state(base, data)
    print(f"watching session {args.session}")
    return 0


def cmd_add_pr(args: argparse.Namespace) -> int:
    parse_pr_subject(args.pr)
    base = state_dir()
    with locked(base):
        data = load_state(base)
        data["prs"].setdefault(args.pr, {"enqueued_at": None})
        save_state(base, data)
    print(f"watching PR {args.pr}")
    return 0


def cmd_remove(args: argparse.Namespace) -> int:
    base = state_dir()
    with locked(base):
        data = load_state(base)
        found = data["sessions"].pop(args.subject, None) or data["prs"].pop(args.subject, None)
        save_state(base, data)
    return 0 if found is not None else 1


def cmd_list(_: argparse.Namespace) -> int:
    data = load_state(state_dir())
    print(
        json.dumps({"seq": data["seq"], "sessions": data["sessions"], "prs": data["prs"]}, indent=1)
    )
    return 0


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("add-session")
    p.add_argument("session")
    p.add_argument("--label", default="")
    p.set_defaults(func=cmd_add_session)
    p = sub.add_parser("add-pr")
    p.add_argument("pr", help="OWNER/REPO#N")
    p.set_defaults(func=cmd_add_pr)
    p = sub.add_parser("remove")
    p.add_argument("subject")
    p.set_defaults(func=cmd_remove)
    sub.add_parser("list").set_defaults(func=cmd_list)
    for name, func in (("poll", cmd_poll), ("loop", cmd_loop), ("tick", cmd_tick)):
        p = sub.add_parser(name)
        p.add_argument(
            "--telegram-env", help="env file with TELEGRAM_BOT_TOKEN / TELEGRAM_HOME_CHANNEL"
        )
        p.add_argument("--queue-timeout", type=int, default=DEFAULT_QUEUE_TIMEOUT_S)
        p.add_argument("--interval", type=float, default=30.0)
        p.add_argument("--tail", type=int, default=20)
        p.set_defaults(func=func)
    p = sub.add_parser("since")
    p.add_argument("cursor", type=int)
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=cmd_since)
    return ap


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
