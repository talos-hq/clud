"""Execute CI OK's independent run-attempt guard against hosted job results."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from tests import process

WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"


def _guard() -> str:
    gate = WORKFLOW.read_text(encoding="utf-8").split("\n  ci-ok:\n", 1)[1]
    step = gate.split("- name: Reject failed jobs in this run attempt\n", 1)[1]
    config, script = step.split("          script: |\n", 1)
    assert "if: always()" in config
    assert "retries: 3" in config
    assert "superseded" not in config
    lines = script.splitlines()
    end = next(i for i, line in enumerate(lines) if not line.startswith("            "))
    return "\n".join(line[12:] for line in lines[:end])


@pytest.mark.parametrize("conclusion", [
    "failure", "cancelled", "timed_out", "startup_failure", "action_required",
    "success", "skipped", None, "empty", "api-error",
])
def test_attempt_guard_rejects_bad_jobs_outside_needs(conclusion: str | None) -> None:
    # Put the extra job past page one's boundary; it has no needs entry.
    jobs = [{"name": "selected", "conclusion": "success"}] * 100
    jobs.append({"name": "unlisted", "conclusion": conclusion, "html_url": "job-url"})
    if conclusion == "empty":
        jobs = []
    harness = """
const assert = require('node:assert/strict');
const failures = [];
const endpoint = {};
const github = {
  rest: {actions: {listJobsForWorkflowRunAttempt: endpoint}},
  paginate: async (method, params) => {
    assert.equal(method, endpoint);
    assert.deepEqual(params, {
      owner: 'talos-hq', repo: 'clud', run_id: 42, attempt_number: 2, per_page: 100,
    });
    if (CONCLUSION === 'api-error') throw new Error('API unavailable');
    return JOBS;
  },
};
const context = {repo: {owner: 'talos-hq', repo: 'clud'}, runId: 42};
const core = {setFailed: message => failures.push(message)};
process.env.GITHUB_RUN_ATTEMPT = '2';
(async () => { SCRIPT })().catch(error => failures.push(error.message)).then(() => {
  assert.equal(failures.length > 0, !['success', 'skipped', null].includes(CONCLUSION));
});
"""
    harness = harness.replace("CONCLUSION", json.dumps(conclusion))
    harness = harness.replace("JOBS", json.dumps(jobs))
    result = process.run(
        ["node", "-e", harness.replace("SCRIPT", _guard())],
        capture_output=True, text=True, timeout=15,
    )
    assert result.returncode == 0, result.stderr
