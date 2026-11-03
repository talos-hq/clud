# Benchmarks

Standalone, opt-in benchmarks live here rather than under `tests/`: they may
intentionally exceed the repository's 90-second pytest timeout and must never
be collected by default CI. See [idle_cpu](idle_cpu/README.md) for the
idle-session CPU harness used by #542.

The [Codex-via-Claude bridge benchmark](codex_bridge/README.md) measures the
bounded loopback request path and reports RSS growth for #630.

The [connector log inventory](connector_logs/README.md) is a read-only,
content-safe diagnostic that identifies which Claude transcripts and clud
bridge logs can be attributed to Codex, DeepSeek, or Kimi.

The [context spam scan](context_spam/README.md) measures Bash output volume
and CI-polling redundancy in local transcripts (#2312, #2315).

The [shell result budget replay](tool-result-cap.md) records the 20-task
captured-output replay through the production Codex translator.

The [Kimi real-key smoke](kimi_smoke.md) is the opt-in, manual check that a
real Moonshot key works direct and through `--unified` without leaking.

The command-gate replay (`clud-bench-gate-replay`, #1067) runs every Bash
command in local Claude Code transcripts through the command gate's own
classifier and prints counts only, never command text:

```
soldr cargo run -p clud --features bench --bin clud-bench-gate-replay -- [~/.claude/projects]
```

The recorded result and what it means for enabling the gate are in DD-056.

## Runbook: real-daemon survival check (Tier 3, #674)

**Never gating.** The reaper's daemon-survival suite is hermetic on purpose —
Tier 1 asserts the decision table against injected process facts, and Tier 2
drives a purpose-built [`daemon-stub`](../testbins/daemon-stub/) that
reproduces each *signal shape* rather than requiring a real sccache, docker or
soldr install on a CI runner.

That leaves exactly one risk the suite cannot cover: the stub's signal shape
drifting from what a real daemon actually does. This runbook is how you check
that by hand on a developer box that has the real thing installed. Run it when
you change the spare-list signals or their precedence, not on every PR.

1. Start the daemons you actually have, from a shell **inside** a clud session
   so they become descendants of the tracker's Job Object:

   ```
   sccache --start-server
   zccache --version          # soldr starts its cache daemon on demand
   docker info                # Docker Desktop's backend is already session 0
   FBuildWorker -mode=idle    # if FASTBuild is installed
   ```

2. Note each PID, then exercise a full session lifecycle — run several agent
   tool calls so real shells spawn and exit, then exit clud normally.

3. Assert survival:

   ```
   sccache --show-stats       # must still answer; the server was not killed
   ```

   and confirm each noted PID is still alive.

4. Read the reap log for the session that just ended and check the *reason*,
   not only the outcome:

   ```
   cat ~/.clud/state/sessions/<pid>__<epoch>/reap.jsonl
   ```

   A daemon spared as `outside_job_object`, `service_session`,
   `declared_daemon` or `listening_endpoint` is spared for a signal the code
   actually evaluated. A daemon that never appears in the log at all was
   spared by accident — most likely because the plan produced nothing — and
   that will regress the next time the planner changes. Treat its absence as
   a finding.

5. Repeat for the hard-kill path: `taskkill /F /PID <clud-pid>` instead of a
   clean exit. The daemons must still survive.

If a real daemon's signal shape turns out to differ from the stub's, fix the
stub *and* add the corresponding row to the Tier 1 table in
`job_orphan_reaper`'s `lifecycle_tests` — that is where the rule belongs.
