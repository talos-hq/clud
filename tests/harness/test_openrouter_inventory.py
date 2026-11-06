"""Measure internal role catalog overhead without removing any tools or agents."""

from __future__ import annotations

import json
import re
from copy import deepcopy
from pathlib import Path

import pytest

from tests.harness.harness import Harness

MODEL = "deepseek/deepseek-v4.1-flash-1017"
PROMPT = "Return the fixed inventory audit completion marker."
LEGACY = {
    "grind-gate": (
        "Internal publication-check role for an approved grind dependency gate. "
        "Executes only its exact assigned check and reports the observed result; "
        "never implements work or changes a repository."
    ),
    "grind-editor": (
        "Internal to the grind-run workflow; do not delegate to it (clud's hook "
        "refuses). /grind worker. Reads and writes only the files it is assigned; may"
        " investigate with read-only gh and web search. Cannot build, lint or test."
    ),
    "grind-planner": (
        "Internal to the grind-run workflow; do not delegate to it (clud's hook "
        "refuses). /grind planner. Splits one goal into worker tasks with disjoint "
        "files, names its dependencies and verify commands. Read-only git/gh shell; "
        "creates a worktree only in parallel mode. Never builds."
    ),
    "grind-integrator": (
        "Internal to the grind-run workflow; do not delegate to it (clud's hook "
        "refuses). /grind integrator. The only role that builds. Runs one at a time; "
        "rebases, lints, builds, tests, fixes until green, then pushes and opens the "
        "PR. No bosn, no direct containers, no new worktrees."
    ),
    "grind-lander": (
        "Internal to the grind-run workflow; do not delegate to it (clud's hook "
        "refuses). /grind lander. Watches a PR with pr_merge_watch, admin-merges it "
        "when green, or diagnoses the failure and hands it back to the integrator. "
        "Cannot edit files or build."
    ),
    "grind-prework": (
        "Internal to the grind-run workflow; do not delegate to it (clud's hook "
        "refuses). /grind prework. Records (does not make) the run's plan by posting "
        "the workflow's pre-built plan comment(s) on the meta issue. Read-only git/gh"
        " shell plus `gh issue comment` on the meta issue. Never edits a comment."
    ),
    "grind-reviewer": (
        "Internal to the grind-run workflow; do not delegate to it (clud's hook "
        "refuses). /grind reviewer. Reviews and corrects worker output against the "
        "goal by reading and editing; may investigate with read-only gh and web "
        "search. Cannot build, lint or test."
    ),
}


def _request(h: Harness, *, args: list[str] | None = None) -> dict:
    result = h.run_clud(
        PROMPT,
        {"audit_http": True, "default_text": "INVENTORY_AUDIT_COMPLETE"},
        model=MODEL,
        extra_args=args,
        timeout=60,
    )
    assert result.returncode == 0, result.stdout[-2000:] + result.stderr[-2000:]
    assert "INVENTORY_AUDIT_COMPLETE" in result.stdout
    requests = [row for row in result.requests if row.get("role")]
    assert len(requests) == 1, "inventory lookup made extra generation requests"
    assert requests[0]["model"] == MODEL
    return requests[0]


def _catalog(request: dict) -> dict[str, str]:
    texts = []
    for message in request["messages"]:
        content = message["content"]
        texts.extend(
            [content]
            if isinstance(content, str)
            else [block["text"] for block in content if block.get("type") == "text"]
        )
    # The project fixture has no other name:description bullet list.
    return dict(re.findall(r"^- ([\w-]+): (.+)$", "\n".join(texts), re.MULTILINE))


def _normalized_legacy(
    messages: list[dict], legacy: dict[str, str], compact: dict[str, str]
) -> list[dict]:
    result = deepcopy(messages)
    for message in result:
        content = message["content"]
        blocks = [{"text": content}] if isinstance(content, str) else content
        for block in blocks:
            if "text" in block:
                for role, description in legacy.items():
                    block["text"] = block["text"].replace(description, compact[role])
        if isinstance(content, str):
            message["content"] = blocks[0]["text"]
    return result


def _audit_inventory(production: dict, legacy: dict, descriptions: dict[str, str]) -> dict:
    compact_catalog, legacy_catalog = _catalog(production), _catalog(legacy)
    assert compact_catalog.keys() == legacy_catalog.keys(), "agent or skill types disappeared"
    assert LEGACY.keys() <= compact_catalog.keys(), "internal workflow entrypoints disappeared"
    for name, description in LEGACY.items():
        assert legacy_catalog[name].startswith(description), "legacy control was overwritten"
        assert (
            compact_catalog[name] == descriptions[name] + legacy_catalog[name][len(description) :]
        ), "role metadata changed beyond its description"
    for name, description in legacy_catalog.items():
        if name not in LEGACY:
            assert compact_catalog[name] == description, "unrelated metadata changed"
    assert production["tools"] == legacy["tools"], "available tools changed"
    assert _normalized_legacy(legacy["messages"], LEGACY, descriptions) == production["messages"], (
        "context changed beyond internal catalog descriptions"
    )
    before, after = legacy["context_bytes"], production["context_bytes"]
    for field in ("tools", "system", "tools_fingerprint", "system_fingerprint"):
        assert before[field] == after[field], f"{field} changed beyond catalog descriptions"
    savings = before["request"] - after["request"]
    assert savings >= 1000, "verbose internal metadata still reaches every model request"
    assert before["messages"] - after["messages"] == savings
    return {
        "legacy_request_bytes": before["request"],
        "production_request_bytes": after["request"],
        "saved_bytes_per_request": savings,
        "tool_count": len(production["tools"]),
        "catalog_names": sorted(compact_catalog),
    }


def _adversaries(production: dict):
    bad = deepcopy(production)
    bad["tools"].pop()
    yield "missing tool", bad
    bad = deepcopy(production)
    bad["context_bytes"]["tools_fingerprint"] = "different schema"
    yield "changed tool schema", bad
    bad = deepcopy(production)
    bad["messages"].append({"role": "user", "content": "unrelated context"})
    yield "extra context", bad
    description = _catalog(production)["grind-editor"]
    bad = deepcopy(production)
    bad["messages"] = _normalized_legacy(
        bad["messages"], {"worker": description}, {"worker": description + " changed tools"}
    )
    yield "changed role tool catalog", bad
    bad = deepcopy(production)
    bad["messages"] = _normalized_legacy(
        bad["messages"], {"worker": "- grind-editor:"}, {"worker": "- missing-worker:"}
    )
    yield "missing workflow role", bad


def test_openrouter_compact_role_catalog_preserves_capabilities(harness: Harness) -> None:
    h = harness
    root = Path(__file__).resolve().parents[2]
    guidance = (root / "CLAUDE.md").read_text(encoding="utf-8")
    (h.repo / "CLAUDE.md").write_text(guidance, encoding="utf-8")
    production = _request(h)
    originals, descriptions = {}, {}
    for role, description in LEGACY.items():
        path = h.config / "agents" / f"{role}.md"
        originals[path] = path.read_text(encoding="utf-8")
        descriptions[role] = re.search(r"^description: (.*)$", originals[path], re.MULTILINE)[1]
        restored, count = re.subn(
            r"^description:.*$",
            f"description: {description}",
            originals[path],
            count=1,
            flags=re.MULTILINE,
        )
        assert count == 1
        # Own the control copy so clud's startup refresh cannot overwrite it.
        path.write_text(
            restored.replace("managed-by: clud", "fixture-owned legacy catalog"), encoding="utf-8"
        )
    try:
        h.new_session()
        legacy = _request(h)
    finally:
        for path, body in originals.items():
            path.write_text(body, encoding="utf-8")
    report = _audit_inventory(production, legacy, descriptions)
    rejected = []
    for name, corrupted in _adversaries(production):
        with pytest.raises(AssertionError):
            _audit_inventory(corrupted, legacy, descriptions)
        rejected.append(name)
    report["rejected_adversaries"] = rejected
    assert (h.repo / "CLAUDE.md").read_text(encoding="utf-8") == guidance
    h.new_session()
    worker = _request(h, args=["--agent", "grind-editor"])
    assert "You are the /grind editor." in worker["system"], "workflow role instructions were lost"
    report["role_entrypoint_verified"] = True
    (h.logs / "inventory-audit.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report))
