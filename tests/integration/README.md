# Python integration tests

## `test_mock_agents.py` — `TestInterruptReporting`

`test_cmd_wrapper_ctrl_break_exit_does_not_prompt_or_orphan_children` exercises
Windows cmd wrappers for Claude and Codex: Ctrl-Break must exit with status 130,
without a batch-job prompt or surviving children.

### #2272 timeout evidence (2026-11-09 UTC)

A bounded review of the latest 100 `ci.yml` runs covered run creation times
2026-11-07 16:42:00 through 2026-11-09 02:21:19 UTC, after diagnostic commit
`5136b2ad` (#2334, 2026-10-29). Of these, **10 runs executed Windows integration**
(9 PR runs and one scheduled main run); **all 10 passed their integration jobs**.
The other 90 runs had no executed Windows integration job. There were 11 passing
jobs: 10 x64 and one ARM job in the scheduled run. This counts runs separately
from jobs and does not imply that every workflow or Windows unit job passed.

| Run examined | Windows integration result |
| --- | --- |
| [42423974936](https://github.com/talos-hq/clud/actions/runs/42423974936) | x64 passed |
| [42427260360](https://github.com/talos-hq/clud/actions/runs/42427260360) | x64 passed |
| [42430083793](https://github.com/talos-hq/clud/actions/runs/42430083793) | x64 passed |
| [42486777439](https://github.com/talos-hq/clud/actions/runs/42486777439) | x64 + ARM passed |
| [42529886109](https://github.com/talos-hq/clud/actions/runs/42529886109) | x64 passed |
| [42535941443](https://github.com/talos-hq/clud/actions/runs/42535941443) | x64 passed |
| [42540022083](https://github.com/talos-hq/clud/actions/runs/42540022083) | x64 passed |
| [42542546400](https://github.com/talos-hq/clud/actions/runs/42542546400) | x64 passed |
| [42544919576](https://github.com/talos-hq/clud/actions/runs/42544919576) | x64 passed |
| [42547738994](https://github.com/talos-hq/clud/actions/runs/42547738994) | x64 passed |

Evidence came from `gh run list --workflow ci.yml --limit 100`, each run's jobs,
and `gh run view --log-failed --job` plus full logs for the 11 integration jobs.
All failed-log outputs were empty; full logs explicitly reported both
`backend_args0-claude` and `backend_args1-codex` as `PASSED` in every job.
**Zero recurrences** of the client timeout were observed in this sample.

The #2334 hook remains present: on `communicate(timeout=30)` expiry it snapshots
the process tree and watched PIDs, stdout/stderr tails, child PID log and mock
tree log before cleanup. Its presence was verified for every examined integration
run's head by source inspection through the GitHub contents API.
No timeout diagnostic dump was emitted, so this sample identifies no concrete
cause. The 30-second timeout, exit-code, prompt and no-orphan assertions remain
unchanged. Ten passing runs are below the approximately 20-run closure threshold;
this is partial evidence for #2272, not proof that the intermittent failure is fixed.
