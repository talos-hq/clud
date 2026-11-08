"""The chaperone's queue-follow watch budget (#2233).

A waiter that runs in the background costs nothing while it waits, so a short
deadline only buys another request at the chaperone's live context to be told
the queue has not moved. Measured over 24 h of local transcripts: 851 queue
follows, 589 of them at the old 540 s deadline, 564 ending in a timeout whose
median restart gap was 542 s — one session re-invoked the waiter 31 times. These tests execute
the production `grind-run.js` in the controlled host and read the prompts it
generates, so they cover the deadline the chaperone is actually given, not the
constant alone.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from tests import process

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / "crates/clud-bin/assets/workflows/grind-run.js"
PROBE = ROOT / "tests/fixtures/grind_workflow_probe.mjs"


def _probe(tmp_path: Path, script: dict[str, list[Any]], **extra: Any) -> dict:
    args = {
        "repo": str(tmp_path / "proj"),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "goals": [{"id": "g1", "title": "g1", "brief": "g1"}],
        **extra,
    }
    config = tmp_path / "probe.json"
    config.write_text(json.dumps({"args": args, "script": script}), encoding="utf-8")
    result = process.run(
        ["node", str(PROBE), str(WORKFLOW), str(config)],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    out = json.loads(result.stdout)
    assert "error" not in out, out
    return out


def _prompts(out: dict, prefix: str) -> list[dict]:
    return [p for p in out["prompts"] if p.get("prompt") and p.get("label", "").startswith(prefix)]


def test_a_background_queue_follow_waits_just_under_the_runner_cap(tmp_path: Path) -> None:
    out = _probe(tmp_path, {"chaperone:": [{"verdict": "merged"}]})
    (chaperone,) = _prompts(out, "chaperone:")
    prompt = chaperone["prompt"]
    assert "--until merged --timeout 3300" in prompt
    assert "--no-merge-wait --timeout 3300" in prompt
    assert "--timeout 540" not in prompt


def test_the_gate_still_chooses_a_shorter_deadline(tmp_path: Path) -> None:
    out = _probe(tmp_path, {"chaperone:": [{"verdict": "merged"}]}, queueWatchSeconds=900)
    (chaperone,) = _prompts(out, "chaperone:")
    assert "--until merged --timeout 900" in chaperone["prompt"]
    assert "--timeout 3300" not in chaperone["prompt"]


def test_the_last_wait_at_finish_keeps_a_short_tail(tmp_path: Path) -> None:
    """A run that is over reports a still-queued PR, it does not wait an hour."""
    queued = {"verdict": "queued"}
    out = _probe(
        tmp_path,
        {"chaperone:": [queued, queued, queued, {"verdict": "merged"}]},
    )
    labels = [p["label"] for p in _prompts(out, "chaperone:")]
    assert labels[-1] == "chaperone:g1#final", labels
    assert "--until merged --timeout 600" in _prompts(out, "chaperone:")[-1]["prompt"]
    assert "--until merged --timeout 3300" in _prompts(out, "chaperone:")[0]["prompt"]
