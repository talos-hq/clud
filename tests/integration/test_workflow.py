"""Integration tests for `clud workflow` against the mock agent (#2471).

Every `agent()` call is a real `clud -p ... --output-format stream-json`
child that launches the mock backend; `MOCK_AGENT_WORKFLOW_RESPONSES` scripts
each reply by prompt substring.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from tests import process

pytestmark = pytest.mark.integration

_TIMEOUT = 75

REVIEW = """export const meta = {
  name: 'review-changes',
  description: 'Review then verify',
  phases: [{ title: 'Review' }, { title: 'Verify' }],
}
const results = await pipeline(
  ['bugs', 'perf'],
  d => agent(`review ${d}`, {label: `review:${d}`, phase: 'Review', schema: {type: 'object'}}),
  r => parallel(r.findings.map(f => () => agent(`verify ${f}`, {phase: 'Verify'})))
)
return { verdicts: results.flat(), mode: args.mode }
"""


def _run(clud: Path, *args: str, env: dict[str, str], cwd: Path) -> process.CompletedProcess[str]:
    return process.run(
        [str(clud), *args],
        capture_output=True,
        text=True,
        timeout=_TIMEOUT,
        env=env,
        cwd=cwd,
    )


def _setup(tmp_path: Path, mock_env: dict[str, str], table: dict) -> tuple[dict[str, str], Path]:
    responses = tmp_path / "responses.json"
    responses.write_text(json.dumps(table))
    env = dict(mock_env)
    env["MOCK_AGENT_WORKFLOW_RESPONSES"] = str(responses)
    return env, responses


def test_workflow_pipeline_parallel_schema_end_to_end(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    env, responses = _setup(
        tmp_path,
        mock_env,
        {
            "review bugs": {"result": "r", "structured_output": {"findings": ["a", "b"]}},
            "review perf": {"result": "r", "structured_output": {"findings": ["c"]}},
            "verify a": {"result": "real"},
            "verify b": {"result": "noise"},
            "verify c": {"result": "real"},
        },
    )
    script = tmp_path / "review.js"
    script.write_text(REVIEW)

    result = _run(
        clud_binary, "workflow", str(script), "--args", '{"mode":"x"}', env=env, cwd=tmp_path
    )

    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == {"verdicts": ["real", "noise", "real"], "mode": "x"}
    assert "[workflow] batch finished" in result.stderr
    assert "| done    |" in result.stderr

    calls = [json.loads(line) for line in Path(f"{responses}.calls").read_text().splitlines()]
    assert len(calls) == 5
    schema_calls = [c for c in calls if c["schema"]]
    assert len(schema_calls) == 2
    assert all(json.loads(c["schema"]) == {"type": "object"} for c in schema_calls)

    done = (tmp_path / "WORKFLOW_DONE.md").read_text()
    assert "Workflow: review-changes" in done
    assert "Outcome: ok" in done
    runs = list((tmp_path / ".clud" / "workflow").iterdir())
    assert len(runs) == 1
    saved = json.loads((runs[0] / "result.json").read_text())
    assert saved["ok"] is True
    assert saved["result"]["mode"] == "x"
    assert len(json.loads((runs[0] / "agents.json").read_text())) == 5


def test_workflow_agent_error_fails_run_and_is_recorded(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    env, _ = _setup(tmp_path, mock_env, {"explode": {"result": "rate limited", "is_error": True}})
    script = tmp_path / "boom.js"
    script.write_text(
        "export const meta = { name: 'boom', description: 'fails' }\nawait agent('explode')\n"
    )

    result = _run(clud_binary, "workflow", str(script), env=env, cwd=tmp_path)

    assert result.returncode == 1, result.stderr
    assert "rate limited" in result.stderr
    assert "| FAILED  |" in result.stderr
    assert "Outcome: FAILED: rate limited" in (tmp_path / "WORKFLOW_DONE.md").read_text()


def test_workflow_rejects_script_without_meta(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    script = tmp_path / "bad.js"
    script.write_text("return 1\n")
    result = _run(clud_binary, "workflow", str(script), env=mock_env, cwd=tmp_path)
    assert result.returncode == 1
    assert "export const meta" in result.stderr
