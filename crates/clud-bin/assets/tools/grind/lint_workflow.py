#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "running-process==4.10.1",
# ]
# ///
# managed-by: clud
"""lint_workflow.py: machine check for a tailored /grind coordinator workflow.

Usage:
  lint_workflow.py <workflow.js> --args <args.json> [--scenario NAME ...] [--json]
  lint_workflow.py --list-scenarios

The /grind router tailors `grind-coord.js` to each run (talos-hq/clud#2873).
Hand-tailored orchestration is where last run's backpressure came from: a
watcher storm (32 watchers and 25 fixers for 9 PRs), two full builds in one
checkout, and a queue stuck behind one merge. So before launch a tailored
script must pass this tool, then an LLM review.

Two layers:
  * Static rules on the source (GW0xx): metadata, determinism, no worktrees,
    known agent types.
  * A simulated run per scenario (GS1xx): the script executes in Node against
    mock agents whose chaperones report scripted events (red checks, reviews,
    stale reports, ejections, wait storms, a dirty checkout, a failed worker).
    The trace is checked for the coordinator invariants: one central agent at
    a time, fixups before new tasks, the in-flight cap, a chaperone for every
    pushed head, bounded watches and fixups, cleanup after a dirty checkout,
    dependency order, human gates, and termination.

Exit codes: 0 clean, 1 findings, 2 usage error or Node missing.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from running_process import PIPE, RunningProcess, TimeoutExpired

ALLOWED_AGENT_TYPES = {"grind-orchestrator", "grind-worker", "grind-chaperone", "grind-reviewer"}
CENTRAL_PREFIXES = ("work:", "fixup:", "cleanup:", "feature-pr")
TERMINAL = {"merged", "stuck", "base_red"}
SIM_TIMEOUT_S = 60
MAX_AGENTS = 600

# The mock host. It runs the workflow body like the real harness does (an
# async function with the hooks as parameters) and answers agents by label:
#   work:<id>, fixup:<id>#k, cleanup:<n>, chaperone:<id>#n, feature-pr.
HARNESS = r"""
import { writeFileSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
// Failure paths exit immediately. Write the whole result before exiting even
// when stdout is a pipe and a long trace cannot fit in its buffer.
const report = value => writeFileSync(1, JSON.stringify(value) + '\n');
const source = await readFile(process.argv[2], 'utf8');
const cfg = JSON.parse(await readFile(process.argv[3], 'utf8'));
const META = 'export const meta =';
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
let execute;
try {
  execute = new AsyncFunction('args', 'agent', 'log', 'phase', 'parallel', 'pipeline', 'workflow', 'budget',
    source.replace(META, 'const meta ='));
} catch (e) { report({ error: 'syntax: ' + e.message, trace: [] }); process.exit(0); }
let seq = 0;
const trace = [];
const logs = [];
const S = cfg.scenario;
const heads = {};
const chapIndex = {};
let agents = 0;
let active = 0;
let idleSince = 0;
let tick = 0;
const sleep = ms => new Promise(r => setTimeout(r, ms));
const tok = (list, i) => (list && i < list.length) ? list[i] : 'merged';
function reply(label, prompt) {
  const [kind, rest] = label.includes(':') ? [label.slice(0, label.indexOf(':')), label.slice(label.indexOf(':') + 1)] : [label, ''];
  const task = rest.split('#')[0];
  if (kind === 'work') {
    const w = (S.worker || {})[task] || 'pushed';
    heads[task] = `h-${task}-0`;
    const clean = !((S.dirty || []).includes(task));
    if (w === 'failed') return { status: 'failed', summary: 'red local gate', failure_log: 'x', checkout_clean: clean };
    if (w === 'covered') return { status: 'covered', covered_by: '#1', covered_state: 'open', summary: 'covered', checkout_clean: clean };
    if (w === 'covered_merged') return { status: 'covered', covered_by: '#1', covered_state: 'merged', summary: 'covered', checkout_clean: clean };
    return { status: 'pushed', pr_url: `https://github.com/o/r/pull/${task}`, branch: `grind/${task}`, head_sha: heads[task],
      local_gate: 'ok', summary: 'pushed', checkout_clean: clean };
  }
  if (kind === 'fixup') {
    const k = rest.split('#')[1];
    if ((S.fixup || {})[task] === 'no_change') return { status: 'no_change', summary: 'nothing to change', checkout_clean: true };
    heads[task] = `h-${task}-${k}`;
    return { status: 'pushed', pr_url: `https://github.com/o/r/pull/${task}`, branch: `grind/${task}`, head_sha: heads[task], summary: 'fixed', checkout_clean: true };
  }
  if (kind === 'cleanup') return { checkout_clean: !S.stuck_dirty, summary: S.stuck_dirty ? 'still dirty' : 'clean' };
  if (kind === 'chaperone') {
    const i = chapIndex[task] = (chapIndex[task] ?? -1) + 1;
    const t = tok((S.chaperone || {})[task], i);
    const head = heads[task];
    if (t === 'stale') return { verdict: 'needs_fix', problem: 'red_check', head_sha: `h-${task}-old`, evidence_id: 'run-stale', summary: 'stale' };
    if (['red_check', 'review', 'conflict', 'ejected', 'parent_merged'].includes(t))
      return { verdict: 'needs_fix', problem: t, head_sha: head, evidence_id: `${t}-${i}`, failure_log: 'log', summary: t };
    return { verdict: t, head_sha: head, summary: t, enqueued: t === 'queued' };
  }
  return { summary: 'ok', status: 'no_change', checkout_clean: true };
}
const agent = async (prompt, opts = {}) => {
  agents += 1;
  if (agents > __MAX__) throw new Error('runaway: more than __MAX__ agents');
  const label = String(opts.label || '');
  const id = seq++;
  active += 1;
  trace.push({ ev: 'start', id, seq: id, label, agentType: opts.agentType || null, model: opts.model || null,
    isolation: opts.isolation || null, prompt: String(prompt) });
  await sleep(label.startsWith('chaperone:') ? 12 : 2);
  const r = reply(label, String(prompt));
  active -= 1;
  idleSince = tick;
  trace.push({ ev: 'end', id, seq: seq++, label, reply: r });
  return r;
};
const parallel = tasks => Promise.all(tasks.map(t => t().catch(() => null)));
const pipeline = (items, ...stages) => Promise.all(items.map(async (item, i) => {
  let v = item;
  for (const s of stages) { try { v = await s(v, item, i); } catch { return null; } if (v == null) return null; }
  return v;
}));
const budget = { total: null, spent: () => 0, remaining: () => Infinity };
let done = false;
const timer = setTimeout(() => { if (!done) { report({ error: 'timeout: the workflow did not finish', trace, logs }); process.exit(0); } }, __TIMEOUT_MS__);
// Deadlock detector: the workflow is still awaiting something, yet no agent
// has been running for 300 ms, so nothing can ever wake it.
const idle = setInterval(() => {
  tick += 1;
  if (!done && active === 0 && tick - idleSince > 6) {
    report({ error: 'deadlock: the workflow is waiting with no agent running', deadlock: true, trace, logs });
    process.exit(0);
  }
}, 50);
try {
  const result = await execute(cfg.args, agent, l => logs.push(String(l)), () => {}, parallel, pipeline,
    async () => { throw new Error('nested workflow() is not allowed in a grind coordinator'); }, budget);
  done = true; clearTimeout(timer); clearInterval(idle);
  report({ result, trace, logs });
} catch (e) {
  done = true; clearTimeout(timer); clearInterval(idle);
  report({ error: String(e && e.message || e), trace, logs });
}
"""


@dataclass
class Finding:
    rule: str
    message: str
    scenario: str = ""

    def text(self) -> str:
        where = f" [{self.scenario}]" if self.scenario else ""
        return f"{self.rule}{where}: {self.message}"


@dataclass
class Policy:
    max_in_flight: int = 6
    max_fixups: int = 3
    max_watches: int = 12
    max_cleanups: int = 2

    @staticmethod
    def of(args: dict[str, Any]) -> Policy:
        p = args.get("policy") or {}

        def clamp(key: str, default: int, lo: int, hi: int) -> int:
            value = p.get(key)
            return max(lo, min(hi, int(default if value is None else value)))

        return Policy(
            clamp("maxInFlight", 6, 1, 32),
            clamp("maxFixups", 3, 0, 8),
            clamp("maxWatches", 12, 1, 40),
            clamp("maxCleanups", 2, 0, 5),
        )


@dataclass
class Scenario:
    name: str
    about: str
    chaperone: dict[str, list[str]] = field(default_factory=dict)
    worker: dict[str, str] = field(default_factory=dict)
    dirty: list[str] = field(default_factory=list)
    fixup: dict[str, str] = field(default_factory=dict)


# ---- static rules ------------------------------------------------------------


def static_findings(src: str) -> list[Finding]:
    out: list[Finding] = []
    if src.count("export const meta =") != 1:
        out.append(Finding("GW001", "expected exactly one `export const meta =` literal"))
    else:
        head = src.split("export const meta =", 1)[1][:2000]
        for key in ("name:", "description:"):
            if key not in head:
                out.append(Finding("GW001", f"meta is missing `{key[:-1]}`"))
    for pat, why in (
        (r"Date\.now\s*\(", "Date.now()"),
        (r"Math\.random\s*\(", "Math.random()"),
        (r"new\s+Date\s*\(\s*\)", "new Date()"),
    ):
        if re.search(pat, src):
            out.append(Finding("GW002", f"{why} breaks workflow resume; pass time in through args"))
    if re.search(r"isolation\s*:\s*['\"]worktree", src):
        out.append(
            Finding(
                "GW003",
                "a worktree agent: every build goes through the central checkout so caches stay warm",
            )
        )
    for m in re.finditer(r"agentType\s*:\s*['\"]([^'\"]+)['\"]", src):
        if m.group(1) not in ALLOWED_AGENT_TYPES:
            out.append(
                Finding(
                    "GW004",
                    f"agent type `{m.group(1)}` is not a coordinator role ({', '.join(sorted(ALLOWED_AGENT_TYPES))})",
                )
            )
    if re.search(r"\bworkflow\s*\(", src.split("export const meta =", 1)[-1]):
        out.append(
            Finding(
                "GW005",
                "nested workflow() call: a coordinator is one flat script the linter can simulate",
            )
        )
    return out


# ---- scenarios -----------------------------------------------------------------


def runnable(args: dict[str, Any]) -> list[dict[str, Any]]:
    """Tasks the coordinator may start: not waiting on a person, directly or
    through a dependency."""
    tasks = args.get("tasks") or []
    waiting = {str(t["id"]) for t in tasks if t.get("gate") == "human"}
    grew = True
    while grew:
        grew = False
        for t in tasks:
            tid = str(t["id"])
            if tid not in waiting and any(str(d) in waiting for d in t.get("after") or []):
                waiting.add(tid)
                grew = True
    return [t for t in tasks if str(t["id"]) not in waiting]


def scenarios(args: dict[str, Any], pol: Policy) -> list[Scenario]:
    tasks = runnable(args)
    ids = [str(t["id"]) for t in tasks]
    if not ids:
        return [Scenario("happy", "nothing to run")]
    first = ids[0]
    # A task nothing depends on, so a failure there blocks no other task.
    leaf = next(
        (i for i in reversed(ids) if not any(i in map(str, t.get("after") or []) for t in tasks)),
        first,
    )
    slow = {i: ["waiting", "queued", "queued", "merged"] for i in ids}
    red_each = {i: ["red_check", "merged"] for i in ids}
    return [
        Scenario(
            "happy", "every PR waits once, then merges", {i: ["waiting", "merged"] for i in ids}
        ),
        Scenario(
            "slow_queue", "every PR sits in the merge queue: the in-flight cap must hold", slow
        ),
        Scenario("red_check", "the first PR goes red once", {first: ["red_check", "merged"]}),
        Scenario("review", "the first PR gets review comments", {first: ["review", "merged"]}),
        Scenario(
            "stale_report",
            "a report about an old head must not be fixed",
            {first: ["red_check", "stale", "merged"]},
        ),
        Scenario(
            "ejected",
            "the first PR is ejected from the queue",
            {first: ["queued", "ejected", "merged"]},
        ),
        Scenario(
            "persistent_red",
            "red after every fixup: the PR must end stuck",
            {first: ["red_check"] * (pol.max_fixups + 2)},
        ),
        Scenario(
            "wait_storm",
            "checks never finish: watches must stay bounded",
            {first: ["waiting"] * (pol.max_watches + 6)},
        ),
        Scenario(
            "queue_storm",
            "a PR outlasts its watches in the queue: left there, never redone",
            {first: ["queued"] * (pol.max_watches + 2)},
        ),
        Scenario(
            "fix_no_change",
            "a fixup finds nothing to change: the same problem is never fixed twice",
            {first: ["red_check", "red_check", "merged"]},
            fixup={first: "no_change"},
        ),
        Scenario("dirty_checkout", "a worker leaves the central checkout dirty", {}, {}, [first]),
        Scenario("worker_failed", "a worker cannot get its gate green", {}, {leaf: "failed"}),
        Scenario(
            "covered_open",
            "an open PR already covers the first task: its dependents wait",
            {},
            {first: "covered"},
        ),
        Scenario(
            "fix_while_producing", "problems arrive while tasks remain: fixups go first", red_each
        ),
    ]


# ---- invariants on a trace -------------------------------------------------------


def task_of(label: str) -> str:
    return label.split(":", 1)[1].split("#", 1)[0] if ":" in label else ""


def watched_head(prompt: str) -> str:
    """The head a chaperone was told to watch: the label contract's
    `expected head <sha>.` phrase."""
    m = re.search(r"expected head (\S+?)\.", prompt)
    return m.group(1) if m else ""


def is_central(label: str) -> bool:
    return label.startswith(CENTRAL_PREFIXES)


class TraceCheck:
    """The coordinator invariants, checked on one simulated run."""

    def __init__(
        self, args: dict[str, Any], pol: Policy, sc: Scenario, run: dict[str, Any]
    ) -> None:
        self.args, self.pol, self.sc, self.run = args, pol, sc, run
        self.trace: list[dict[str, Any]] = run.get("trace") or []
        self.starts = [e for e in self.trace if e["ev"] == "start"]
        self.tasks = {str(t["id"]): t for t in args.get("tasks") or []}
        self.findings: list[Finding] = []

    def add(self, rule: str, msg: str) -> None:
        self.findings.append(Finding(rule, msg, self.sc.name))

    def check(self) -> list[Finding]:
        if self.run.get("error"):
            rule = "GS111" if self.run.get("deadlock") else "GS100"
            self.add(rule, f"the workflow did not complete: {self.run['error']}")
        for start in self.starts:
            self.roles(start)
        self.exclusive()
        self.walk()
        self.bounds()
        self.heads_watched()
        self.reported()
        return self.findings

    # GS101/GS107: roles, models, worktrees and the chaperone's wait rules.
    def roles(self, s: dict[str, Any]) -> None:
        lab, at, model = s["label"], s["agentType"], s["model"]
        if s["isolation"]:
            self.add(
                "GS101",
                f"{lab} runs with isolation={s['isolation']}; the central checkout is the only workspace",
            )
        if not lab.startswith((*CENTRAL_PREFIXES, "chaperone:", "triage", "review")):
            self.add("GS107", f"agent label `{lab}` is outside the coordinator label contract")
        if lab.startswith("chaperone:"):
            self.chaperone_role(lab, at, model, s["prompt"].lower())
        elif is_central(lab):
            if at != "grind-worker":
                self.add("GS107", f"{lab} must run as grind-worker, not {at}")
            if model == "haiku":
                self.add("GS107", f"{lab} runs on haiku; central work needs a full model")

    def chaperone_role(self, lab: str, at: str | None, model: str | None, prompt: str) -> None:
        if at != "grind-chaperone":
            self.add("GS107", f"{lab} must run as grind-chaperone, not {at}")
        if model not in ("haiku", "sonnet"):
            self.add(
                "GS107",
                f"{lab} runs on {model or 'the session model'}; chaperones only watch, use sonnet or haiku",
            )
        forbids = re.search(r"never (?:use )?run_in_background", prompt)
        allows = re.search(
            r"(?<!never )(?<!never use )(?:with|use|using) run_in_background", prompt
        )
        if not forbids or allows or "foreground" not in prompt:
            self.add(
                "GS107",
                f"{lab}'s prompt must require foreground waits and forbid run_in_background (#2877)",
            )
        if self.args.get("land") == "merge" and "pr_merge" not in prompt:
            self.add(
                "GS107", f"{lab}: a repo without a merge queue needs the pinned pr_merge.py merge"
            )

    # GS102: one central agent at a time.
    def exclusive(self) -> None:
        open_ids: set[int] = set()
        for e in self.trace:
            if not is_central(e["label"]):
                continue
            if e["ev"] == "end":
                open_ids.discard(e["id"])
                continue
            if open_ids:
                self.add(
                    "GS102",
                    f"{e['label']} started while another agent was working in the central checkout",
                )
            open_ids.add(e["id"])
        if open_ids:
            self.add("GS100", "a central agent never finished")

    # GS103/GS104/GS105/GS108/GS112: the ordered walk.
    def walk(self) -> None:
        w = Walk(self)
        for e in self.trace:
            (w.on_start if e["ev"] == "start" else w.on_end)(e)
        w.finish()

    # GS106: bounded watches and agents.
    def bounds(self) -> None:
        per: dict[str, int] = {}
        for s in self.starts:
            if s["label"].startswith("chaperone:"):
                t = task_of(s["label"])
                per[t] = per.get(t, 0) + 1
        for t, n in per.items():
            if n > self.pol.max_watches + 1:
                self.add(
                    "GS106",
                    f"{n} chaperone calls for {t} (maxWatches {self.pol.max_watches}): a watcher storm",
                )
        n_tasks = max(1, len(self.tasks))
        bound = (
            n_tasks * (2 + self.pol.max_watches + self.pol.max_fixups) + self.pol.max_cleanups + 4
        )
        if len(self.starts) > bound:
            self.add(
                "GS106", f"{len(self.starts)} agents for {n_tasks} tasks exceeds the bound {bound}"
            )

    # GS109: every pushed head is watched.
    def heads_watched(self) -> None:
        watched: dict[str, set[str]] = {}
        for s in self.starts:
            if s["label"].startswith("chaperone:"):
                watched.setdefault(task_of(s["label"]), set()).add(watched_head(s["prompt"]))
        for e in self.trace:
            r = e.get("reply") or {}
            if (
                e["ev"] == "end"
                and e["label"].startswith(("work:", "fixup:"))
                and r.get("status") == "pushed"
            ):
                t, head = task_of(e["label"]), r.get("head_sha", "")
                if head not in watched.get(t, set()):
                    self.add("GS109", f"pushed head {head} of {t} never got a chaperone")

    # GS110: every task is reported.
    def reported(self) -> None:
        result = self.run.get("result")
        if self.run.get("error") or not isinstance(result, dict):
            return
        got = {str(x.get("task")) for x in result.get("tasks", [])}
        missing = [t for t in self.tasks if t not in got]
        if missing:
            self.add("GS110", f"the result does not report tasks {', '.join(missing)}")


class Walk:
    """One ordered pass over the trace, tracking what the coordinator knew."""

    def __init__(self, tc: TraceCheck) -> None:
        self.tc = tc
        self.pushed: set[str] = set()
        self.settled: set[str] = set()
        self.merged: set[str] = set()
        self.worked: set[str] = set()
        self.owed: dict[str, list[str]] = {}  # task -> problem@head reports not yet fixed
        self.fixed: dict[str, set[str]] = {}  # task -> problem@head already fixed
        self.repeats: dict[str, list[str]] = {}  # reports of a problem already fixed on that head
        self.fixups: dict[str, int] = {}
        self.need_cleanup = False
        self.last_report = {
            task_of(e["label"]): e["seq"]
            for e in tc.trace
            if e["ev"] == "end" and e["label"].startswith("chaperone:")
        }

    def on_start(self, e: dict[str, Any]) -> None:
        lab = e["label"]
        if is_central(lab):
            if self.need_cleanup and not lab.startswith("cleanup:"):
                self.tc.add(
                    "GS105", f"{lab} started on a dirty central checkout; a cleanup must run first"
                )
            if lab.startswith("cleanup:"):
                self.need_cleanup = False
        if lab.startswith("work:"):
            self.start_work(lab, task_of(lab))
        elif lab.startswith("fixup:"):
            self.start_fixup(lab, task_of(lab))

    def start_work(self, lab: str, t: str) -> None:
        pol = self.tc.pol
        if t in self.worked:
            self.tc.add("GS112", f"{lab} started again: a task is worked once")
        self.worked.add(t)
        live = self.pushed - self.settled
        if len(live) >= pol.max_in_flight:
            self.tc.add(
                "GS103", f"{lab} started with {len(live)} PRs in flight (cap {pol.max_in_flight})"
            )
        owing = sorted(
            k for k, v in self.owed.items() if v and self.fixups.get(k, 0) < pol.max_fixups
        )
        if owing:
            self.tc.add(
                "GS104",
                f"{lab} started while fixups were owed for {', '.join(owing)}; fixups go first",
            )
        task = self.tc.tasks.get(t, {})
        if task.get("gate") == "human":
            self.tc.add("GS108", f"{lab} started a task that waits on a person")
        for d in map(str, task.get("after") or []):
            if d not in self.merged:
                self.tc.add("GS108", f"{lab} started before its dependency {d} merged")

    def start_fixup(self, lab: str, t: str) -> None:
        self.fixups[t] = self.fixups.get(t, 0) + 1
        if self.fixups[t] > self.tc.pol.max_fixups:
            self.tc.add("GS106", f"{lab} exceeds maxFixups {self.tc.pol.max_fixups}")
        pending = self.owed.get(t) or []
        if not pending:
            repeats = self.repeats.get(t) or []
            if repeats:
                self.tc.add("GS113", f"{lab} fixes {repeats.pop(0)} a second time on the same head")
            else:
                self.tc.add("GS104", f"{lab} started with no problem reported on the current head")
            return
        key = pending.pop(0)
        if key in self.fixed.setdefault(t, set()):
            self.tc.add("GS113", f"{lab} fixes {key} a second time on the same head")
        self.fixed[t].add(key)

    def on_end(self, e: dict[str, Any]) -> None:
        lab, r = e["label"], e.get("reply") or {}
        t = task_of(lab)
        if is_central(lab) and r.get("checkout_clean") is False:
            self.need_cleanup = True
        if lab.startswith("work:"):
            if r.get("status") == "pushed":
                self.pushed.add(t)
            elif r.get("status") == "no_change" or (
                r.get("status") == "covered" and r.get("covered_state") == "merged"
            ):
                self.merged.add(t)
        if lab.startswith("chaperone:"):
            self.report(t, r, e["seq"])

    def report(self, t: str, r: dict[str, Any], seq: int) -> None:
        v = r.get("verdict")
        # Settled once its final report is back: a terminal verdict, or the
        # coordinator stopped watching (watch budget spent, left in the queue).
        if v in TERMINAL or self.last_report.get(t) == seq:
            self.settled.add(t)
        if v == "merged":
            self.merged.add(t)
        if v != "needs_fix" or r.get("evidence_id") == "run-stale":
            return
        key = f"{r.get('problem')}@{r.get('head_sha')}"
        if key in self.fixed.get(t, set()):
            # Already fixed on this head: watching again is correct, fixing is not.
            self.repeats.setdefault(t, []).append(key)
            return
        self.owed.setdefault(t, []).append(key)

    def finish(self) -> None:
        for t, pending in self.owed.items():
            if pending and self.fixups.get(t, 0) < self.tc.pol.max_fixups:
                self.tc.add(
                    "GS104", f"a problem on {t} ({pending[0]}) was reported and never fixed"
                )


def check_trace(
    args: dict[str, Any], pol: Policy, sc: Scenario, run: dict[str, Any]
) -> list[Finding]:
    return TraceCheck(args, pol, sc, run).check()


# ---- running ------------------------------------------------------------------


def simulate(
    node: str, script: Path, args: dict[str, Any], sc: Scenario, work: Path
) -> dict[str, Any]:
    harness = work / "harness.mjs"
    if not harness.exists():
        body = HARNESS.replace("__MAX__", str(MAX_AGENTS)).replace(
            "__TIMEOUT_MS__", str(SIM_TIMEOUT_S * 1000 - 5000)
        )
        harness.write_text(body, encoding="utf-8")
    cfg = work / f"{sc.name}.json"
    cfg.write_text(json.dumps({"args": args, "scenario": sc.__dict__}), encoding="utf-8")
    try:
        res = RunningProcess.run(
            [node, str(harness), str(script), str(cfg)],
            stdout=PIPE,
            stderr=PIPE,
            text=True,
            timeout=SIM_TIMEOUT_S,
            check=False,
        )
    except TimeoutExpired:
        return {"error": "timeout: node did not exit", "trace": []}
    out = (res.stdout or "").strip().splitlines()
    if not out:
        return {
            "error": f"no result (the workflow deadlocked or crashed): {(res.stderr or '').strip()[-400:]}",
            "trace": [],
        }
    return json.loads(out[-1])


def lint(
    script: Path, args: dict[str, Any], only: list[str] | None = None
) -> tuple[list[Finding], list[str]]:
    src = script.read_text(encoding="utf-8")
    findings = static_findings(src)
    pol = Policy.of(args)
    node = shutil.which("node")
    if not node:
        raise SystemExit("node is required for the simulated run; install Node.js")
    ran: list[str] = []
    with tempfile.TemporaryDirectory(prefix="grind-lint-") as tmp:
        for sc in scenarios(args, pol):
            if only and sc.name not in only:
                continue
            ran.append(sc.name)
            findings += check_trace(args, pol, sc, simulate(node, script, args, sc, Path(tmp)))
    seen: set[str] = set()
    unique = [x for x in findings if not (x.text() in seen or seen.add(x.text()))]
    return unique, ran


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    ap.add_argument("script", nargs="?", type=Path)
    ap.add_argument("--args", type=Path, help="the JSON args the workflow will be launched with")
    ap.add_argument("--scenario", action="append", help="run only these scenarios")
    ap.add_argument("--list-scenarios", action="store_true")
    ap.add_argument("--json", action="store_true")
    ns = ap.parse_args(argv)
    if ns.list_scenarios:
        demo = {"tasks": [{"id": "a"}]}
        for sc in scenarios(demo, Policy.of(demo)):
            print(f"{sc.name}: {sc.about}")
        return 0
    if not ns.script or not ns.args:
        ap.print_usage(sys.stderr)
        return 2
    args = json.loads(ns.args.read_text(encoding="utf-8"))
    try:
        findings, ran = lint(ns.script, args, ns.scenario)
    except SystemExit as e:
        print(str(e), file=sys.stderr)
        return 2
    if ns.json:
        print(
            json.dumps(
                {"ok": not findings, "scenarios": ran, "findings": [x.__dict__ for x in findings]},
                indent=2,
            )
        )
    else:
        for x in findings:
            print(x.text())
        print(
            f"{'FAIL' if findings else 'OK'}: {len(findings)} finding(s) across {len(ran)} simulated scenario(s): {', '.join(ran)}"
        )
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
