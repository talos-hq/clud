"""A stand-in for the GitHub CLI, backed by a JSON state file.

The real-harness tests (#1323) put a `gh` wrapper that runs this file first on
PATH. Everything a test world needs from GitHub lives in ``$FAKE_GH_STATE``:

    {"repo": "o/r",
     "issues": {"7": {"title": "...", "body": "...", "state": "open",
                      "sub_issues": [{"number": 8, "state": "open"}]}},
     "prs": [{"number": 1, "head": "feat/x", "state": "MERGED", "title": "..."}],
     "fail": false,
     "calls": []}

Every invocation is appended to ``calls``. ``"fail": true`` makes every call
exit 1, to exercise "gh is broken" paths. Calls are serialized with a lock
file next to the state, so parallel agents never lose each other's writes.

Optional additions (#1402); every key is defaulted, so the shape above still
works unchanged:

- top level ``default_branch`` (default ``"main"``), ``next_id`` (comment id
  counter), ``requires_review`` (every merge into ``default_branch`` needs an
  approval or ``--admin``), ``fail_on`` (a list of fault keys that always
  fail) and ``faults``: ``{"<key>": {"code": 1, "stderr": "...", "times": N,
  "after": M}}``. The key is the first two argv words (``"issue close"``), or
  ``"api <METHOD>"`` / ``"api graphql"`` for ``api``; a longer key narrows it
  by prefix of the rest of the command (``"issue comment 5"``,
  ``"api POST repos/o/r/issues/1/sub_issues"``). The most specific matching
  key wins. A fault lets its first ``after`` matching calls through, then
  fires ``times`` times (forever without ``times``). Faults fire after the
  call is recorded and before dispatch, so a failed call changes nothing.
- per issue: ``labels`` (list of str), ``comments`` (``[{"id", "body"}]``),
  ``closed_by`` (``{"kind": "pr"|"user"|"command"|"commit", "pr": int|None,
  "oid": str}``) and ``parent`` (int or None).
- per PR: ``base``, ``draft``, ``body``, ``merge_method``, ``admin``,
  ``requires_review`` (alias ``reviews_required``) and ``approved``.
- per PR ``queue_sequence``: queue snapshots consumed by merge-watch GraphQL
  reads; the last snapshot remains available for subsequent polls. A snapshot
  may carry ``removed`` (``{"reason": str, "createdAt": ISO8601-Z}``) to render the
  queue-removal timeline node a merged-queue ejection is read from (#2266); a
  snapshot whose ``createdAt`` predates the watch is not an ejection.
- ``repositories`` maps owner/name to independent worlds with their own
  issues, PRs and default branch. Explicit repo flags, issue/PR URLs and REST
  endpoints select one; an unknown repo fails instead of reading the primary.
  Root ``calls`` records every original invocation, including failed selection.
- ``release_after_views`` maps a tag to ``{after: N, release: {...}}`` to
  publish it after N missing observations, without sleeping in a fixture.

Supported commands: ``issue view|create|comment|edit|close|reopen|list``,
``pr list|create|edit|ready|merge|view|checks``, ``repo view`` (name and
``defaultBranchRef``), ``auth status``, ``label create`` (a no-op) and
``api``: issue GET/PATCH, issue comments GET/POST, comment GET/PATCH/DELETE,
sub-issue GET, POST (``replace_parent``) and DELETE with GitHub's one-parent /
100-children / 8-levels rules, ``.../parent`` GET, and ``graphql``
stubs for issue closers and PR merge queues. ``api`` takes ``-X/--method``, ``-f/-F field=value``
and ``--input file``; with no method it is POST when fields are given, else
GET. ``{owner}``/``{repo}`` placeholders are filled from ``repo``. REST issue
objects carry ``id == number``, so ``sub_issue_id=<number>`` works.

``--body-file <f>`` (``-`` for stdin) works wherever ``--body`` does, and
``-q/--jq`` supports paths (``.a.b``, ``.[]``, ``.[0]``), ``|`` and
``length``. Issue list/search take ``--label``, ``--state``, ``--limit`` and
``--search`` (words, ``in:title``/``in:body``, ``[-]label:``, ``is:`` /
``state:``, and ``head:``/``base:`` for PRs). Bodies over 65536 characters are
rejected. Merging a PR into ``default_branch`` closes the open issues its body
names with a closing keyword (``Closes/Fixes/Resolves #N``) and records the PR
as their closer; merges into other bases close nothing. A PR without a
target (``gh pr view --json body``) or a ``--head`` resolves to
``$FAKE_GH_HEAD`` or the current git branch. PR numbers start at 101 and are
numbered apart from issues.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import sys
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any

try:
    import fcntl
except ImportError:  # Windows
    fcntl = None  # type: ignore[assignment]
    import msvcrt

BODY_LIMIT = 65536
SUB_ISSUE_LIMIT = 100
DEPTH_LIMIT = 8
_CLOSING = re.compile(
    r"\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s*:?\s+(?:([\w.-]+/[\w.-]+))?#(\d+)",
    re.IGNORECASE,
)
_API_VALUE_FLAGS = {
    "-X",
    "--method",
    "-f",
    "--raw-field",
    "-F",
    "--field",
    "--input",
    "-H",
    "--header",
    "-q",
    "--jq",
    "-t",
    "--template",
    "--hostname",
    "--cache",
    "-p",
    "--preview",
}


class _RepositoryState(dict):
    """A repo view whose saves retain the entire locked multi-repo world."""

    def __init__(self, root: dict, name: str):
        super().__init__(root["repositories"][name])
        self.root = root
        self["repo"] = name
        self.setdefault("fail", root.get("fail", False))
        root["repositories"][name] = self


def _load() -> tuple[Path, dict]:
    path = Path(os.environ["FAKE_GH_STATE"])
    return path, json.loads(path.read_text(encoding="utf-8"))


def _save(path: Path, state: dict) -> None:
    root = state.root if isinstance(state, _RepositoryState) else state
    path.write_text(json.dumps(root, indent=1), encoding="utf-8")


@contextmanager
def _locked(path: Path) -> Iterator[None]:
    """Hold an exclusive lock on ``<state>.lock`` for one whole call."""
    with open(path.with_name(path.name + ".lock"), "a+b") as fh:
        if fcntl is not None:
            fcntl.flock(fh.fileno(), fcntl.LOCK_EX)
        else:
            while True:
                try:
                    fh.seek(0)
                    msvcrt.locking(fh.fileno(), msvcrt.LK_LOCK, 1)
                    break
                except OSError:
                    continue
        try:
            yield
        finally:
            if fcntl is not None:
                fcntl.flock(fh.fileno(), fcntl.LOCK_UN)
            else:
                fh.seek(0)
                msvcrt.locking(fh.fileno(), msvcrt.LK_UNLCK, 1)


def _flag(argv: list[str], name: str) -> str | None:
    for i, arg in enumerate(argv):
        if arg == name and i + 1 < len(argv):
            return argv[i + 1]
        if arg.startswith(name + "="):
            return arg.split("=", 1)[1]
    return None


def _flags(argv: list[str], *names: str) -> list[str]:
    out = []
    for i, arg in enumerate(argv):
        for name in names:
            if arg == name and i + 1 < len(argv):
                out.append(argv[i + 1])
            elif name.startswith("--") and arg.startswith(name + "="):
                out.append(arg.split("=", 1)[1])
    return out


def _body(argv: list[str], text_flags: tuple[str, ...], file_flags: tuple[str, ...]) -> str | None:
    """The body from ``--body``-style flags, else from a ``--body-file`` (``-`` = stdin)."""
    for name in text_flags:
        value = _flag(argv, name)
        if value is not None:
            return value
    for name in file_flags:
        source = _flag(argv, name)
        if source is not None:
            if source == "-":
                return sys.stdin.read()
            return Path(source).read_text(encoding="utf-8")
    return None


def _split_labels(values: list[str]) -> list[str]:
    return [p.strip() for v in values for p in v.split(",") if p.strip()]


def _number(target: str) -> str:
    return target.rstrip("/").rsplit("/", 1)[-1].lstrip("#")


def _repo(state: dict) -> str:
    return state.get("repo", "o/r")


def _issue_url(state: dict, number) -> str:
    return f"https://github.com/{_repo(state)}/issues/{number}"


def _pr_url(state: dict, number) -> str:
    return f"https://github.com/{_repo(state)}/pull/{number}"


def _now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def _too_long(body: str | None) -> bool:
    if body is not None and len(body) > BODY_LIMIT:
        print(
            f"gh: body is too long (maximum is {BODY_LIMIT} characters) (HTTP 422)",
            file=sys.stderr,
        )
        return True
    return False


def _jq(expr: str, data: Any) -> list[Any]:  # noqa: C901
    """A tiny jq: ``.a.b``, ``.[]``, ``.[N]``, ``.["k"]``, ``|`` and ``length``."""
    step = re.compile(r'\.([A-Za-z_][\w-]*)|\.?\[(-?\d*)\]|\.?\["([^"]+)"\]')
    values = [data]
    for segment in expr.split("|"):
        seg = segment.strip()
        if seg in ("", "."):
            continue
        if seg == "length":
            values = [len(v) if v is not None else 0 for v in values]
            continue
        pos = 0
        while pos < len(seg):
            m = step.match(seg, pos)
            if m is None:
                raise ValueError(expr)
            name, index, quoted = m.groups()
            key = name if name is not None else quoted
            nxt: list[Any] = []
            for v in values:
                if key is not None:
                    nxt.append(v.get(key) if isinstance(v, dict) else None)
                elif index == "":
                    if isinstance(v, dict):
                        nxt.extend(v.values())
                    elif isinstance(v, list):
                        nxt.extend(v)
                else:
                    i = int(index)
                    ok = isinstance(v, list) and -len(v) <= i < len(v)
                    nxt.append(v[i] if ok else None)
            values = nxt
            pos = m.end()
    return values


def _emit(argv: list[str], data: Any) -> int:
    """Print ``data`` as JSON, or through ``-q/--jq`` as gh does."""
    expr = _flag(argv, "--jq") or _flag(argv, "-q")
    if expr is None:
        print(json.dumps(data))
        return 0
    try:
        values = _jq(expr, data)
    except ValueError:
        print(f"fake gh: unsupported --jq expression {expr!r}", file=sys.stderr)
        return 1
    for v in values:
        print(v if isinstance(v, str) else json.dumps(v))
    return 0


def _current_branch() -> str | None:
    """``$FAKE_GH_HEAD``, else the branch checked out in the cwd's git repo."""
    env = os.environ.get("FAKE_GH_HEAD")
    if env:
        return env
    here = Path.cwd()
    for d in (here, *here.parents):
        dot_git = d / ".git"
        if dot_git.is_dir():
            head = dot_git / "HEAD"
        elif dot_git.is_file():
            text = dot_git.read_text(encoding="utf-8").strip()
            if not text.startswith("gitdir:"):
                return None
            gitdir = Path(text.split(":", 1)[1].strip())
            head = (gitdir if gitdir.is_absolute() else d / gitdir) / "HEAD"
        else:
            continue
        try:
            ref = head.read_text(encoding="utf-8").strip()
        except OSError:
            return None
        prefix = "ref: refs/heads/"
        return ref[len(prefix) :] if ref.startswith(prefix) else None
    return None


def _search_terms(query: str | None) -> dict[str, Any]:  # noqa: C901
    """Split a GitHub search string into filters the fake understands."""
    out: dict[str, Any] = {
        "words": [],
        "no_words": [],
        "in": set(),
        "labels": [],
        "no_labels": [],
        "state": None,
        "head": None,
        "base": None,
    }
    if not query:
        return out
    try:
        tokens = shlex.split(query)
    except ValueError:
        tokens = query.split()
    for token in tokens:
        key, sep, value = token.partition(":")
        low = key.lower()
        if not sep or not value:
            if token.startswith("-") and len(token) > 1:
                out["no_words"].append(token[1:].lower())
            else:
                out["words"].append(token.lower())
        elif low == "in":
            out["in"].update(v.strip().lower() for v in value.split(","))
        elif low == "label":
            out["labels"].append(value)
        elif low == "-label":
            out["no_labels"].append(value)
        elif low in ("is", "state") and value.lower() in ("open", "closed", "merged"):
            out["state"] = value.lower()
        elif low == "head":
            out["head"] = value
        elif low == "base":
            out["base"] = value
        # Other qualifiers (author:, sort:, is:issue, ...) are ignored.
    return out


def _matches_search(terms: dict[str, Any], title: str, body: str, labels: list[str]) -> bool:
    if any(lbl not in labels for lbl in terms["labels"]):
        return False
    if any(lbl in labels for lbl in terms["no_labels"]):
        return False
    scope = terms["in"] or {"title", "body"}
    haystack = " ".join(
        text for name, text in (("title", title), ("body", body)) if name in scope
    ).lower()
    if any(word in haystack for word in terms["no_words"]):
        return False
    return all(word in haystack for word in terms["words"])


def _limit(argv: list[str]) -> int | None:
    raw = _flag(argv, "--limit") or _flag(argv, "-L")
    return int(raw) if raw and raw.isdigit() else None


def _issue_json(state: dict, number: str) -> dict | None:
    issue = state.get("issues", {}).get(number)
    if issue is None:
        return None
    return {
        "number": int(number),
        "title": issue.get("title", ""),
        "body": issue.get("body", ""),
        "state": issue.get("state", "open").upper(),
        "url": _issue_url(state, number),
        "labels": [{"name": n} for n in issue.get("labels", [])],
        "assignees": [{"login": n} for n in issue.get("assignees", [])],
        "comments": [
            {"id": c["id"], "body": c.get("body", "")} for c in issue.get("comments", [])
        ],
    }


def _sub_state(state: dict, sub: dict) -> str:
    """A sub-issue's state: the live issue's when it exists, else the stored one."""
    issue = state.get("issues", {}).get(str(sub["number"]))
    return (issue or sub).get("state", "open")


def _rest_issue(state: dict, number: str) -> dict:
    """``GET repos/o/r/issues/N`` in REST shape (``id`` is the number)."""
    issue = state["issues"][number]
    subs = issue.get("sub_issues", [])
    done = sum(1 for s in subs if _sub_state(state, s) == "closed")
    return {
        "id": int(number),
        "number": int(number),
        "title": issue.get("title", ""),
        "body": issue.get("body", ""),
        "state": issue.get("state", "open").lower(),
        "state_reason": issue.get("state_reason"),
        "labels": [{"name": n} for n in issue.get("labels", [])],
        "html_url": _issue_url(state, number),
        "sub_issues_summary": {
            "total": len(subs),
            "completed": done,
            "percent_completed": (100 * done // len(subs)) if subs else 0,
        },
    }


def _rest_comment(state: dict, number: str, comment: dict) -> dict:
    return {
        "id": comment["id"],
        "body": comment.get("body", ""),
        "html_url": f"{_issue_url(state, number)}#issuecomment-{comment['id']}",
        "issue_url": f"https://api.github.com/repos/{_repo(state)}/issues/{number}",
        "user": {"login": comment.get("author", state.get("login", "fake-gh"))},
    }


def _next_comment_id(state: dict) -> int:
    cid = int(state.get("next_id", 1000))
    state["next_id"] = cid + 1
    return cid


def _find_comment(state: dict, cid: int) -> tuple[str, dict] | None:
    for number, issue in state.get("issues", {}).items():
        for c in issue.get("comments", []):
            if c.get("id") == cid:
                return number, c
    return None


def _set_issue_state(state: dict, number: str, new_state: str) -> None:
    issue = state["issues"][number]
    issue["state"] = new_state
    parent = issue.get("parent")
    if parent is not None:
        for s in state["issues"].get(str(parent), {}).get("sub_issues", []):
            if str(s["number"]) == number:
                s["state"] = new_state


def _close(state: dict, number: str, closed_by: dict) -> None:
    _set_issue_state(state, number, "closed")
    state["issues"][number]["closed_by"] = closed_by


def _fault_key(argv: list[str]) -> str:
    if argv[:1] == ["api"]:
        if _api_endpoint(argv) == "graphql":
            return "api graphql"
        return "api " + _api_method(argv)
    return " ".join(argv[:2])


def _fault_detail(argv: list[str]) -> str:
    """The rest of the command after the fault key, for narrower keys."""
    if argv[:1] == ["api"]:
        endpoint = _api_endpoint(argv)
        return "" if endpoint == "graphql" else endpoint.strip("/")
    return " ".join(argv[2:])


def _fault(state: dict, argv: list[str]) -> dict | None:
    """The fault to fire for this call, consuming ``after``/``times``; else None."""
    base = _fault_key(argv)
    full = f"{base} {_fault_detail(argv)}".strip()

    def hit(key: str) -> bool:
        return full == key or full.startswith(key + " ") or key == base

    if any(hit(k) for k in state.get("fail_on", [])):
        return {"code": 1, "stderr": f"fake gh: injected failure ({base})"}
    for key in sorted(state.get("faults", {}), key=len, reverse=True):
        if not hit(key):
            continue
        fault = state["faults"][key]
        if int(fault.get("after", 0)) > 0:
            fault["after"] = int(fault["after"]) - 1
            return None
        if fault.get("times", 1) == 0:
            return None
        if "times" in fault:
            fault["times"] = int(fault["times"]) - 1
        return fault
    return None


def _api_method(argv: list[str]) -> str:
    method = _flag(argv, "-X") or _flag(argv, "--method")
    if method:
        return method.upper()
    if _flags(argv, "-f", "-F", "--field", "--raw-field") or _flag(argv, "--input"):
        return "POST"
    return "GET"


def _api_endpoint(argv: list[str]) -> str:
    """The first positional argument after ``api``, skipping flag values."""
    skip = False
    for arg in argv[1:]:
        if skip:
            skip = False
            continue
        if arg in _API_VALUE_FLAGS:
            skip = True
            continue
        if arg.startswith("-"):
            continue
        return arg
    return ""


def _api_fields(argv: list[str]) -> dict:
    fields: dict = {}
    source = _flag(argv, "--input")
    if source:
        text = sys.stdin.read() if source == "-" else Path(source).read_text(encoding="utf-8")
        fields.update(json.loads(text))
    for raw in _flags(argv, "-f", "--raw-field"):
        key, _, value = raw.partition("=")
        fields[key] = value
    for raw in _flags(argv, "-F", "--field"):
        key, _, value = raw.partition("=")
        if value in ("true", "false"):
            fields[key] = value == "true"
        elif value.lstrip("-").isdigit():
            fields[key] = int(value)
        elif value == "null":
            fields[key] = None
        elif value.startswith("@"):
            fields[key] = Path(value[1:]).read_text(encoding="utf-8")
        else:
            fields[key] = value
    return fields


def _depth(state: dict, number: str) -> int:
    """Levels from the root down to ``number`` (a root is level 1)."""
    depth, seen = 1, {number}
    parent = state["issues"].get(number, {}).get("parent")
    while parent is not None and str(parent) not in seen:
        seen.add(str(parent))
        depth += 1
        parent = state["issues"].get(str(parent), {}).get("parent")
    return depth


def _height(state: dict, number: str, seen: set | None = None) -> int:
    """Levels in the subtree rooted at ``number`` (a leaf is 1)."""
    seen = seen or set()
    if number in seen:
        return 0
    seen.add(number)
    subs = state["issues"].get(number, {}).get("sub_issues", [])
    return 1 + max((_height(state, str(s["number"]), seen) for s in subs), default=0)


def _is_ancestor(state: dict, ancestor: str, number: str) -> bool:
    """True when ``ancestor`` is ``number`` or above it in the parent chain."""
    seen: set[str] = set()
    current: str | None = number
    while current is not None and current not in seen:
        if current == ancestor:
            return True
        seen.add(current)
        parent = state["issues"].get(current, {}).get("parent")
        current = str(parent) if parent is not None else None
    return False


def _unprocessable(message: str) -> int:
    print(f"gh: {message} (HTTP 422)", file=sys.stderr)
    return 1


def _not_found() -> int:
    print("gh: Not Found (HTTP 404)", file=sys.stderr)
    return 1


def _detach(state: dict, child: str) -> None:
    issue = state["issues"][child]
    parent = issue.get("parent")
    if parent is not None and str(parent) in state["issues"]:
        p = state["issues"][str(parent)]
        p["sub_issues"] = [s for s in p.get("sub_issues", []) if str(s["number"]) != child]
    issue["parent"] = None


def _graphql_merge(path: Path, state: dict, argv: list[str]) -> int | None:
    fields = _api_fields(argv)
    query = str(fields.get("query", ""))
    if "timelineItems(last:" in query and "mergeQueueEntry" in query:
        pr = _find_pr(state, str(fields.get("number", "")))
        if pr is None:
            return _emit(argv, {"data": {"repository": {"pullRequest": None}}})
        snapshots = pr.get("queue_sequence", [])
        snapshot = snapshots.pop(0) if len(snapshots) > 1 else (snapshots[0] if snapshots else {})
        _save(path, state)
        entry_state = snapshot.get("entry")
        entry = {"state": entry_state, "position": snapshot.get("position"),
                 "headCommit": None} if entry_state else None
        # A queue *removal* is only readable when the timeline's last node carries
        # the reason and its timestamp (#2266): an empty `mergeQueueEntry` with no
        # removal node is an unreadable sample, not an ejection, and merge-watch
        # treats it as such. A fixture asks for one with `removed` on the snapshot.
        removal = snapshot.get("removed")
        nodes = ([{"reason": removal.get("reason"), "createdAt": removal.get("createdAt")}]
                 if isinstance(removal, dict) else [])
        value = {"state": snapshot.get("state", "OPEN"), "baseRefName": pr.get("base", "main"),
                 "autoMergeRequest": None, "mergeQueueEntry": entry,
                 "timelineItems": {"nodes": nodes}}
        return _emit(argv, {"data": {"repository": {"pullRequest": value}}})
    if "enqueuePullRequest" in query:
        pr = next((p for p in state.get("prs", [])
                   if f"{_repo(state)}#{p['number']}" == fields.get("id")), None)
        if pr is None or pr.get("head_oid") != fields.get("sha"):
            return _emit(argv, {"errors": [{"message": "head changed"}]})
        if _needs_review(state, pr) and not pr.get("approved"):
            return _emit(argv, {"errors": [{"message": "review required"}]})
        pr["queued"] = True
        _save(path, state)
        return _emit(argv, {"data": {"enqueuePullRequest": {"mergeQueueEntry": {"id": "entry"}}}})
    if "pullRequest(number:" in query and "headRefOid" in query:
        pr = _find_pr(state, str(fields.get("number", "")))
        value = None if pr is None else {
            "id": f"{_repo(state)}#{pr['number']}", "state": pr.get("state", "OPEN"),
            "isDraft": pr.get("draft", False), "headRefOid": pr.get("head_oid"),
            "baseRefName": pr.get("base", "main"),
            "mergeQueueEntry": {"id": "entry"} if pr.get("queued") else None,
        }
        return _emit(argv, {"data": {"repository": {"pullRequest": value}}})
    if "mergeQueue(branch:" in query:
        queue = {"id": "queue"} if fields.get("base") in state.get("queue_branches", []) else None
        return _emit(argv, {"data": {"repository": {"mergeQueue": queue}}})
    return None


def _graphql(path: Path, state: dict, argv: list[str]) -> int:
    result = _graphql_merge(path, state, argv)
    if result is not None:
        return result
    issues = state.setdefault("issues", {})
    fields = _api_fields(argv)
    query = str(fields.get("query", ""))
    if "ClosedEvent" not in query and "closer" not in query:
        print(json.dumps({"data": {}}))
        return 0
    number = fields.get("number") or fields.get("issue")
    if number is None:
        m = re.search(r"issue\s*\(\s*number\s*:\s*(\d+)", query)
        number = m.group(1) if m else None
    issue = issues.get(str(number)) if number is not None else None
    if issue is None:
        return _emit(argv, {"data": {"repository": {"issue": None}}})
    nodes = []
    if issue.get("state", "open") == "closed":
        by = issue.get("closed_by") or {}
        if by.get("kind") == "pr" and by.get("pr") is not None:
            closer_pr = next((p for p in state.get("prs", []) if p["number"] == by["pr"]), {})
            closer = {
                "__typename": "PullRequest",
                "number": by["pr"],
                "merged": closer_pr.get("state") == "MERGED",
                "baseRefName": closer_pr.get("base", state.get("default_branch", "main")),
            }
            nodes.append({"closer": closer})
        elif by.get("kind") == "commit" and by.get("oid"):
            nodes.append({"closer": {"__typename": "Commit", "oid": by["oid"]}})
        else:
            nodes.append({"closer": None})
    return _emit(argv, {"data": {"repository": {"issue": {"timelineItems": {"nodes": nodes}}}}})


def _api(path: Path, state: dict, argv: list[str]) -> int:  # noqa: C901
    issues = state.setdefault("issues", {})
    if _api_endpoint(argv) == "graphql":
        return _graphql(path, state, argv)

    method = _api_method(argv)
    owner, _, name = _repo(state).partition("/")
    endpoint = _api_endpoint(argv).replace("{owner}", owner).replace("{repo}", name)
    parts = endpoint.split("?", 1)[0].strip("/").split("/")
    if len(parts) >= 4 and parts[0] == "repos" and parts[3] == "releases":
        return _release_api(state, argv, parts, method)
    is_issues = len(parts) >= 5 and parts[0] == "repos" and parts[3] == "issues"

    # repos/o/r/issues/comments/{id}
    if is_issues and len(parts) == 6 and parts[4] == "comments":
        found = _find_comment(state, int(parts[5])) if parts[5].isdigit() else None
        if found is None:
            return _not_found()
        number, comment = found
        if method == "DELETE":
            issue = issues[number]
            issue["comments"] = [c for c in issue["comments"] if c is not comment]
            _save(path, state)
            return 0
        if method == "PATCH":
            body = _api_fields(argv).get("body")
            if _too_long(body):
                return 1
            if body is not None:
                comment["body"] = body
            _save(path, state)
        return _emit(argv, _rest_comment(state, number, comment))

    # repos/o/r/issues/N
    if is_issues and len(parts) == 5:
        issue = issues.get(parts[4])
        if issue is None:
            return _not_found()
        if method == "PATCH":
            fields = _api_fields(argv)
            if _too_long(fields.get("body")):
                return 1
            for key in ("title", "body"):
                if fields.get(key) is not None:
                    issue[key] = fields[key]
            if isinstance(fields.get("labels"), list):
                issue["labels"] = [str(x) for x in fields["labels"]]
            if fields.get("state") == "closed" and issue.get("state", "open") != "closed":
                _close(state, parts[4], {"kind": "user", "pr": None})
            elif fields.get("state") == "open":
                _set_issue_state(state, parts[4], "open")
                issue["closed_by"] = None
            _save(path, state)
        return _emit(argv, _rest_issue(state, parts[4]))

    # repos/o/r/issues/N/comments
    if is_issues and len(parts) == 6 and parts[5] == "comments":
        issue = issues.get(parts[4])
        if issue is None:
            return _not_found()
        if method == "POST":
            body = _api_fields(argv).get("body")
            if _too_long(body):
                return 1
            comment = {"id": _next_comment_id(state), "body": body or ""}
            issue.setdefault("comments", []).append(comment)
            _save(path, state)
            return _emit(argv, _rest_comment(state, parts[4], comment))
        return _emit(
            argv,
            [[_rest_comment(state, parts[4], c) for c in issue.get("comments", [])]]
            if "--slurp" in argv
            else [_rest_comment(state, parts[4], c) for c in issue.get("comments", [])],
        )

    # repos/o/r/issues/N/parent
    if is_issues and len(parts) == 6 and parts[5] == "parent":
        issue = issues.get(parts[4])
        parent = issue.get("parent") if issue is not None else None
        if parent is None or str(parent) not in issues:
            return _not_found()
        return _emit(argv, _rest_issue(state, str(parent)))

    # repos/o/r/issues/N/sub_issues and .../sub_issue
    if is_issues and len(parts) == 6 and parts[5] in ("sub_issues", "sub_issue"):
        issue = issues.get(parts[4])
        if issue is None:
            return _not_found()
        if method == "GET":
            rows = []
            for s in issue.get("sub_issues", []):
                row = {"id": s["number"], "number": s["number"], "state": _sub_state(state, s)}
                if str(s["number"]) in issues:
                    row["title"] = issues[str(s["number"])].get("title", "")
                row["html_url"] = _issue_url(state, s["number"])
                rows.append(row)
            return _emit(argv, rows)
        fields = _api_fields(argv)
        child = str(fields.get("sub_issue_id", ""))
        if child not in issues:
            return _not_found()
        if method == "DELETE":
            if str(issues[child].get("parent")) != parts[4]:
                return _not_found()
            _detach(state, child)
            _save(path, state)
            return _emit(argv, _rest_issue(state, parts[4]))
        if method == "POST":
            replace = fields.get("replace_parent") in (True, "true")
            if _is_ancestor(state, child, parts[4]):
                return _unprocessable("Validation Failed: an issue cannot be its own sub-issue")
            current = issues[child].get("parent")
            if current is not None and str(current) == parts[4]:
                if replace:
                    return _emit(argv, _rest_issue(state, parts[4]))
                return _unprocessable("Validation Failed: duplicate sub-issue")
            if current is not None and not replace:
                return _unprocessable("Validation Failed: sub-issue may only have one parent")
            if len(issue.get("sub_issues", [])) >= SUB_ISSUE_LIMIT:
                return _unprocessable(
                    f"Validation Failed: parent may only have {SUB_ISSUE_LIMIT} sub-issues"
                )
            if _depth(state, parts[4]) + _height(state, child) > DEPTH_LIMIT:
                return _unprocessable(
                    f"Validation Failed: sub-issues may only be nested {DEPTH_LIMIT} levels deep"
                )
            if current is not None:
                _detach(state, child)
            issue.setdefault("sub_issues", []).append(
                {"number": int(child), "state": issues[child].get("state", "open")}
            )
            issues[child]["parent"] = int(parts[4])
            _save(path, state)
            return _emit(argv, _rest_issue(state, parts[4]))

    print(json.dumps({}))
    return 0


def _issue(path: Path, state: dict, argv: list[str]) -> int:  # noqa: C901
    issues = state.setdefault("issues", {})
    sub = argv[1] if len(argv) > 1 else ""

    if sub == "create":
        body = _body(argv, ("--body", "-b"), ("--body-file", "-F")) or ""
        if _too_long(body):
            return 1
        taken = [int(k) for k in issues if k.isdigit()] + [
            p["number"] for p in state.get("prs", [])
        ]
        number = str(max(taken, default=0) + 1)
        issues[number] = {
            "title": _flag(argv, "--title") or _flag(argv, "-t") or "",
            "body": body,
            "state": "open",
            "labels": _split_labels(_flags(argv, "--label", "-l")),
            "comments": [],
            "sub_issues": [],
            "parent": None,
            "closed_by": None,
        }
        _save(path, state)
        print(_issue_url(state, number))
        return 0

    if sub == "list":
        labels = _split_labels(_flags(argv, "--label", "-l"))
        terms = _search_terms(_flag(argv, "--search") or _flag(argv, "-S"))
        wanted = (_flag(argv, "--state") or _flag(argv, "-s") or terms["state"] or "open").lower()
        out = []
        for key in sorted(issues, key=lambda k: int(k) if k.isdigit() else 0):
            issue = issues[key]
            have = issue.get("labels", [])
            if wanted != "all" and issue.get("state", "open").lower() != wanted:
                continue
            if any(lbl not in have for lbl in labels):
                continue
            if not _matches_search(terms, issue.get("title", ""), issue.get("body", ""), have):
                continue
            out.append(
                {
                    "number": int(key),
                    "title": issue.get("title", ""),
                    "body": issue.get("body", ""),
                    "state": issue.get("state", "open").upper(),
                    "labels": [{"name": n} for n in have],
                    "url": _issue_url(state, key),
                }
            )
        limit = _limit(argv)
        return _emit(argv, out[:limit] if limit is not None else out)

    if len(argv) < 3:
        print(f"gh: issue {sub} needs an issue number", file=sys.stderr)
        return 1
    number = _number(argv[2])
    issue = issues.get(number)
    if issue is None:
        print(f"gh: issue {number} not found", file=sys.stderr)
        return 1

    if sub == "view":
        data = _issue_json(state, number)
        if "--json" in argv or any(a.startswith("--json=") for a in argv):
            return _emit(argv, data)
        print(f"title:\t{data['title']}\nstate:\t{data['state']}\n--\n{data['body']}")
        if "--comments" in argv or "-c" in argv:
            for c in data["comments"]:
                print(f"--\n{c['body']}")
        return 0

    if sub == "comment":
        body = _body(argv, ("--body", "-b"), ("--body-file", "-F")) or ""
        if _too_long(body):
            return 1
        cid = _next_comment_id(state)
        issue.setdefault("comments", []).append({"id": cid, "body": body})
        _save(path, state)
        print(f"{_issue_url(state, number)}#issuecomment-{cid}")
        return 0

    if sub == "edit":
        body = _body(argv, ("--body", "-b"), ("--body-file", "-F"))
        if _too_long(body):
            return 1
        if body is not None:
            issue["body"] = body
        title = _flag(argv, "--title") or _flag(argv, "-t")
        if title is not None:
            issue["title"] = title
        labels = issue.setdefault("labels", [])
        for lbl in _split_labels(_flags(argv, "--add-label")):
            if lbl not in labels:
                labels.append(lbl)
        remove = set(_split_labels(_flags(argv, "--remove-label")))
        issue["labels"] = [lbl for lbl in labels if lbl not in remove]
        _save(path, state)
        print(_issue_url(state, number))
        return 0

    if sub == "close":
        if issue.get("state", "open") == "closed":
            print(f"! Issue #{number} is already closed", file=sys.stderr)
            return 0
        comment = _flag(argv, "--comment") or _flag(argv, "-c")
        if _too_long(comment):
            return 1
        if comment is not None:
            issue.setdefault("comments", []).append(
                {"id": _next_comment_id(state), "body": comment}
            )
        _close(state, number, {"kind": "command", "pr": None})
        reason = _flag(argv, "--reason") or _flag(argv, "-r")
        issue["state_reason"] = "not_planned" if reason == "not planned" else "completed"
        _save(path, state)
        print(f"Closed issue #{number}")
        return 0

    if sub == "reopen":
        if issue.get("state", "open") != "closed":
            print(f"! Issue #{number} is already open", file=sys.stderr)
            return 0
        _set_issue_state(state, number, "open")
        issue["closed_by"] = None
        issue["state_reason"] = "reopened"
        _save(path, state)
        print(f"Reopened issue #{number}")
        return 0

    return 0


def _find_pr(state: dict, target: str) -> dict | None:
    """A PR by number, ``#N``, URL or head branch; no target means the current branch."""
    prs = state.get("prs", [])
    text = target.strip()
    m = re.search(r"/pull/(\d+)", text)
    if m:
        number: int | None = int(m.group(1))
    elif text.lstrip("#").isdigit():
        number = int(text.lstrip("#"))
    else:
        number = None
    if number is not None:
        return next((p for p in prs if p["number"] == number), None)
    head = text or _current_branch()
    if not head:
        return None
    matches = [p for p in prs if p.get("head") == head]
    live = [p for p in matches if p.get("state", "OPEN") == "OPEN"]
    return (live or matches or [None])[-1]


def _review_decision(state: dict, pr: dict) -> str:
    if pr.get("approved"):
        return "APPROVED"
    if _needs_review(state, pr):
        return "REVIEW_REQUIRED"
    return ""


def _needs_review(state: dict, pr: dict) -> bool:
    if pr.get("requires_review") or pr.get("reviews_required"):
        return True
    default_branch = state.get("default_branch", "main")
    return bool(state.get("requires_review")) and pr.get("base", default_branch) == default_branch


def _pr_json(state: dict, pr: dict) -> dict:
    default_branch = state.get("default_branch", "main")
    return {
        "number": pr["number"],
        "state": pr.get("state", "OPEN"),
        "headRefName": pr.get("head"),
        "baseRefName": pr.get("base", default_branch),
        "isDraft": bool(pr.get("draft", False)),
        "body": pr.get("body", ""),
        "title": pr.get("title", ""),
        "url": _pr_url(state, pr["number"]),
        "mergedAt": pr.get("merged_at"),
        "reviewDecision": _review_decision(state, pr),
    }


def _pr(path: Path, state: dict, argv: list[str]) -> int:  # noqa: C901
    sub = argv[1] if len(argv) > 1 else ""
    default_branch = state.get("default_branch", "main")

    if sub == "list":
        head = _flag(argv, "--head") or _flag(argv, "-H")
        base = _flag(argv, "--base") or _flag(argv, "-B")
        terms = _search_terms(_flag(argv, "--search") or _flag(argv, "-S"))
        wanted = (_flag(argv, "--state") or _flag(argv, "-s") or terms["state"] or "open").lower()
        prs = [
            p
            for p in state.get("prs", [])
            if (head is None or p.get("head") == head)
            and (base is None or p.get("base", default_branch) == base)
            and (wanted == "all" or p.get("state", "OPEN").lower() == wanted)
            and (terms["head"] is None or str(p.get("head", "")).startswith(terms["head"]))
            and (terms["base"] is None or p.get("base", default_branch) == terms["base"])
            and _matches_search(terms, p.get("title", ""), p.get("body", ""), [])
        ]
        limit = _limit(argv)
        rows = [_pr_json(state, p) for p in prs]
        return _emit(argv, rows[:limit] if limit is not None else rows)

    if sub == "create":
        body = _body(argv, ("--body", "-b"), ("--body-file", "-F")) or ""
        if _too_long(body):
            return 1
        prs = state.setdefault("prs", [])
        head = _flag(argv, "--head") or _flag(argv, "-H") or _current_branch() or "HEAD"
        base = _flag(argv, "--base") or _flag(argv, "-B") or default_branch
        dup = next(
            (
                p
                for p in prs
                if p.get("head") == head
                and p.get("base", default_branch) == base
                and p.get("state", "OPEN") == "OPEN"
            ),
            None,
        )
        if dup is not None:
            print(
                f'a pull request for branch "{head}" into branch "{base}" already exists:\n'
                f"{_pr_url(state, dup['number'])}",
                file=sys.stderr,
            )
            return 1
        number = max((p["number"] for p in prs), default=100) + 1
        prs.append(
            {
                "number": number,
                "head": head,
                "state": "OPEN",
                "title": _flag(argv, "--title") or _flag(argv, "-t") or "",
                "base": base,
                "draft": "--draft" in argv or "-d" in argv,
                "body": body,
            }
        )
        _save(path, state)
        print(_pr_url(state, number))
        return 0

    target = argv[2] if len(argv) > 2 and not argv[2].startswith("-") else ""
    pr = _find_pr(state, target)
    if pr is None:
        print(f"gh: no pull requests found for {target or 'the current branch'}", file=sys.stderr)
        return 1
    number = pr["number"]

    if sub == "edit":
        body = _body(argv, ("--body", "-b"), ("--body-file", "-F"))
        if _too_long(body):
            return 1
        if body is not None:
            pr["body"] = body
        title = _flag(argv, "--title") or _flag(argv, "-t")
        if title is not None:
            pr["title"] = title
        base = _flag(argv, "--base") or _flag(argv, "-B")
        if base is not None:
            pr["base"] = base
        _save(path, state)
        print(_pr_url(state, number))
        return 0

    if sub == "ready":
        pr["draft"] = "--undo" in argv
        _save(path, state)
        if pr["draft"]:
            print(f"Pull request #{number} is converted to draft")
        else:
            print(f"Pull request #{number} is marked as ready for review")
        return 0

    if sub == "merge":
        admin = "--admin" in argv
        if pr.get("state", "OPEN") == "MERGED":
            print(f"gh: pull request #{number} was already merged", file=sys.stderr)
            return 1
        if pr.get("state", "OPEN") == "CLOSED":
            print(f"gh: pull request #{number} is closed and cannot be merged", file=sys.stderr)
            return 1
        method = next(
            (m for m in ("merge", "squash", "rebase") if f"--{m}" in argv or f"-{m[0]}" in argv),
            None,
        )
        if method is None:
            print(
                "gh: --merge, --rebase, or --squash required when not running interactively",
                file=sys.stderr,
            )
            return 1
        if pr.get("draft"):
            print(f"gh: pull request #{number} is still a draft", file=sys.stderr)
            return 1
        if _needs_review(state, pr) and not pr.get("approved") and not admin:
            print(
                f"gh: pull request #{number} is not mergeable: "
                "the base branch policy prohibits the merge (review required)",
                file=sys.stderr,
            )
            return 1
        pr["merge_method"] = method
        pr["admin"] = admin
        pr["state"] = "MERGED"
        pr["merged_at"] = _now()
        if pr.get("base", default_branch) == default_branch:
            for repo, ref in _CLOSING.findall(pr.get("body") or ""):
                if repo and repo.lower() != _repo(state).lower():
                    continue
                issue = state.get("issues", {}).get(ref)
                if issue is not None and issue.get("state", "open") != "closed":
                    _close(state, ref, {"kind": "pr", "pr": number})
        _save(path, state)
        print(f"Merged pull request #{number}")
        return 0

    if sub == "checks":
        print("All checks were successful")
        return 0

    if sub == "view":
        return _emit(argv, _pr_json(state, pr))

    return 0


def _release_json(state: dict, tag: str, value: dict) -> dict:
    return {
        "tagName": tag, "name": tag, "isDraft": False,
        "url": f"https://github.com/{_repo(state)}/releases/tag/{tag}",
        "publishedAt": None, **value,
    }


def _release_tag(argv: list[str]) -> str | None:
    args = iter(argv[2:])
    for arg in args:
        if arg in {"--repo", "-R", "--json", "--jq", "-q", "--title", "--notes", "--target"}:
            next(args, None)
        elif arg == "--":
            return next(args, None)
        elif not arg.startswith("-"):
            return arg
    return None


def _release_observation(path: Path, state: dict, argv: list[str], tag: str) -> None:
    transition = state.get("release_after_views", {}).get(tag)
    if argv[1] != "view" or tag in state["releases"] or not isinstance(transition, dict):
        return
    after = transition.get("after")
    if not isinstance(after, int) or isinstance(after, bool) or after < 0:
        return
    views = sum(call[:2] == ["release", "view"] and _release_tag(call) == tag
                for call in state.get("calls", []))
    if views > after:
        state["releases"][tag] = _release_json(state, tag, transition.get("release", {}))
        _save(path, state)


def _release(path: Path, state: dict, argv: list[str]) -> int:
    tag = _release_tag(argv)
    if tag is None:
        print("fake gh: a literal release tag is required", file=sys.stderr)
        return 1
    releases = state.setdefault("releases", {})
    _release_observation(path, state, argv, tag)
    if argv[1] == "create":
        if tag in releases:
            print("release already exists", file=sys.stderr)
            return 1
        draft = "--draft" in argv
        releases[tag] = _release_json(state, tag, {
            "name": _flag(argv, "--title") or tag, "isDraft": draft,
            "publishedAt": None if draft else _now(),
        })
        _save(path, state)
        print(releases[tag]["url"])
        return 0
    if argv[1] == "view" and tag in releases:
        return _emit(argv, _release_json(state, tag, releases[tag]))
    print(f"release not found or unsupported operation: {tag}", file=sys.stderr)
    return 1


def _release_api(state: dict, argv: list[str], parts: list[str], method: str) -> int:
    releases = state.get("releases", {})
    if method != "GET":
        return _not_found()
    tags = list(releases) if len(parts) == 4 else parts[5:6]
    if len(parts) != 4 and (
        len(parts) != 6 or parts[4] != "tags" or not tags or tags[0] not in releases
    ):
        return _not_found()
    values = []
    for tag in tags:
        release = _release_json(state, tag, releases[tag])
        values.append({"tag_name": tag, "draft": release["isDraft"],
                       "published_at": release["publishedAt"], "html_url": release["url"]})
    return _emit(argv, values if len(parts) == 4 else values[0])


def _preflight_read(state: dict, argv: list[str]) -> int | None:
    if argv[:2] == ["api", "user"]:
        return _emit(argv, {"login": state.get("login", "fake-gh")})
    if argv[:2] == ["label", "list"]:
        return _emit(argv, [{"name": name} for name in state.get("labels", [])])
    return None


def _dispatch(path: Path, state: dict, argv: list[str]) -> int:
    state.setdefault("calls", []).append(argv)
    _save(path, state)
    if state.get("fail"):
        print("fake gh: simulated failure (not signed in)", file=sys.stderr)
        return 1

    fault = _fault(state, argv) if argv else None
    if fault is not None:
        _save(path, state)
        print(fault.get("stderr", "fake gh: injected fault"), file=sys.stderr)
        return int(fault.get("code", 1))
    _save(path, state)

    preflight = _preflight_read(state, argv)
    if preflight is not None:
        return preflight
    if argv[:1] == ["api"] and len(argv) > 1:
        return _api(path, state, argv)
    if argv[:1] == ["issue"]:
        return _issue(path, state, argv)
    if argv[:1] == ["pr"]:
        return _pr(path, state, argv)
    if argv[:1] == ["release"]:
        return _release(path, state, argv)
    if argv[:2] == ["repo", "view"]:
        return _emit(
            argv,
            {
                "nameWithOwner": _repo(state),
                "defaultBranchRef": {"name": state.get("default_branch", "main")},
            },
        )
    if argv[:2] == ["auth", "status"]:
        print("github.com: logged in to github.com as fake-gh")
        return 0
    return 0


def _without_repo_prefix(argv: list[str]) -> list[str]:
    if argv[:1] in (["-R"], ["--repo"]):
        return argv[2:]
    if argv and (argv[0].startswith("--repo=") or argv[0].startswith("-R")):
        return argv[1:]
    return argv


def _requested_repository(state: dict, argv: list[str]) -> str:
    explicit = _flag(argv, "--repo") or _flag(argv, "-R")
    if explicit:
        return explicit.removeprefix("github.com/")
    joined = next((arg[2:] for arg in argv if arg.startswith("-R") and len(arg) > 2), None)
    if joined:
        return joined
    args = _without_repo_prefix(argv)
    if args[:1] == ["api"]:
        endpoint = _api_endpoint(args)
        if endpoint == "graphql":
            fields = _api_fields(args)
            if fields.get("owner") and fields.get("name"):
                return f"{fields['owner']}/{fields['name']}"
            if "enqueuePullRequest" in str(fields.get("query", "")):
                node = str(fields.get("id", ""))
                if "#" in node:
                    return node.rsplit("#", 1)[0]
        match = re.match(r"/?repos/([^/]+/[^/?]+)(?:/|$)", endpoint)
        if match and "{" not in match[1]:
            return match[1]
    target = args[2] if len(args) > 2 else ""
    if args[:2] == ["repo", "view"] and re.fullmatch(r"[^/\s]+/[^/\s]+", target):
        return target
    match = re.match(r"https://github\.com/([^/]+/[^/]+)/(?:issues|pull)/", target)
    return match[1] if match else _repo(state)


def _repository_state(state: dict, argv: list[str]) -> _RepositoryState:
    requested = _requested_repository(state, argv)
    names = state["repositories"]
    name = next((name for name in names if name.lower() == requested.lower()), None)
    if name is None:
        raise ValueError(f"unknown fixture repository {requested}")
    return _RepositoryState(state, name)


def main(argv: list[str]) -> int:
    path = Path(os.environ["FAKE_GH_STATE"])
    with _locked(path):
        _, state = _load()
        if isinstance(state.get("repositories"), dict):
            state.setdefault("calls", []).append(argv)
            try:
                selected = _repository_state(state, argv)
            except ValueError as error:
                _save(path, state)
                print(str(error), file=sys.stderr)
                return 1
            return _dispatch(path, selected, _without_repo_prefix(argv))
        return _dispatch(path, state, argv)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
