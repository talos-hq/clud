"""A proven Clippy job or Dylint leg is a no-op (DD-226).

`ci-lint gate` refuses a required job skipped at the job level unless it can
re-derive the skip itself, so a proven job still starts (DD-225). DD-226 makes
that start the whole cost: the attestation is decided in the first step,
before checkout, disk cleanup or setup, and every later step is gated on it.

These tests walk each reusable workflow's real step list with a small
evaluator for the `if:` expressions it uses, under the inputs ci.yml passes,
and assert which steps would execute.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

import pytest
import yaml

from ci import attest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
DYLINT_PASSES = ("host", *attest.DYLINT_CROSS_TARGETS)
# The only steps a proven call may execute: the decision and the combined
# reduction flag. Neither may use an action (checkout, cache, setup).
DECISION_STEPS = {"Proven by a prooft attestation", "Skip unchanged inputs"}

_TOKEN = re.compile(r"\s+|'(?:[^']|'')*'|&&|\|\||==|!=|!|\(|\)|,|[A-Za-z_][A-Za-z0-9_.\-]*")


def _load(name: str) -> dict[str, Any]:
    return yaml.safe_load((WORKFLOWS / name).read_text(encoding="utf-8"))


def _call_inputs(doc: dict[str, Any], given: dict[str, Any]) -> dict[str, Any]:
    trigger = doc.get("on") or doc.get(True)
    declared = trigger["workflow_call"]["inputs"]
    values = {name: spec.get("default", "") for name, spec in declared.items()}
    values.update(given)
    return values


class _Expr:
    """Evaluate the GitHub expression subset these workflows' `if:` use."""

    def __init__(self, context: dict[str, Any]) -> None:
        self.context = context

    def value(self, name: str) -> Any:
        if name in ("true", "false"):
            return name == "true"
        if name in self.context:
            return self.context[name]
        if name.startswith(("steps.", "needs.")):
            return ""
        raise KeyError(f"unknown expression name {name!r}")

    def truthy(self, expression: str | bool | None) -> bool:
        if expression is None:
            return True  # implicit success()
        if isinstance(expression, bool):
            return expression
        text = expression.strip()
        if text.startswith("${{") and text.endswith("}}"):
            text = text[3:-2]
        parts: list[str] = []
        names: dict[str, Any] = {}
        for token in _TOKEN.findall(text):
            if token.isspace():
                continue
            if token.startswith("'"):
                parts.append(repr(token[1:-1].replace("''", "'")))
            elif token in {"&&", "||", "!", "==", "!=", "(", ")", ","}:
                parts.append({"&&": " and ", "||": " or ", "!": " not "}.get(token, token))
            elif token in {"success", "failure", "always", "cancelled", "contains", "format"}:
                parts.append(f"_{token}")
            else:
                key = f"v{len(names)}"
                names[key] = self.value(token)
                parts.append(key)
        functions = {
            "_success": lambda: True,
            "_failure": lambda: False,
            "_always": lambda: True,
            "_cancelled": lambda: False,
            "_contains": lambda hay, needle: str(needle) in str(hay),
            "_format": lambda fmt, *args: fmt.format(*args),
        }
        return bool(eval("".join(parts), {"__builtins__": {}}, {**functions, **names}))


def _executed(doc: dict[str, Any], job: str, context: dict[str, Any]) -> list[dict[str, Any]]:
    expr = _Expr(context)
    return [step for step in doc["jobs"][job]["steps"] if expr.truthy(step.get("if"))]


def _clippy_context(*, proven: bool) -> dict[str, Any]:
    doc = _load("_build-target.yml")
    caller = _load("ci.yml")["jobs"]["lint-linux-x64"]["with"]
    given = {k: v for k, v in caller.items() if isinstance(v, bool)}
    given.update(attested=proven, target=caller["target"], strategy=caller["strategy"])
    context: dict[str, Any] = {f"inputs.{k}": v for k, v in _call_inputs(doc, given).items()}
    context.update(
        {
            "runner.os": "Linux",
            "github.event_name": "pull_request",
            "github.ref": "refs/pull/1/merge",
            "github.workflow_ref": "talos-hq/clud/.github/workflows/ci.yml@refs/pull/1/merge",
            "github.repository": "talos-hq/clud",
            "steps.attested.outputs.proven": "true" if proven else "",
            "steps.skip.outputs.reduced": "true" if proven else "",
            "steps.pass-key.outputs.eligible": "true",
        }
    )
    return context


def _dylint_context(leg: str, attested_passes: str) -> dict[str, Any]:
    doc = _load("_dylint.yml")
    decision = doc["jobs"]["dylint"]["steps"][0]["env"]["ATTESTED"]
    proven = _Expr({"matrix.pass": leg, "inputs.attested_passes": attested_passes}).truthy(decision)
    context: dict[str, Any] = {
        f"inputs.{k}": v for k, v in _call_inputs(doc, {"attested_passes": attested_passes}).items()
    }
    context.update(
        {
            "matrix.pass": leg,
            "github.event_name": "pull_request",
            "github.ref": "refs/pull/1/merge",
            "steps.attested.outputs.proven": "true" if proven else "",
            "steps.skip.outputs.reduced": "true" if proven else "",
        }
    )
    return context


def _names(steps: list[dict[str, Any]]) -> set[str]:
    return {step["name"] for step in steps}


def test_proven_clippy_job_runs_only_its_decision_steps() -> None:
    steps = _executed(_load("_build-target.yml"), "build", _clippy_context(proven=True))
    assert _names(steps) == DECISION_STEPS
    assert not any("uses" in step for step in steps)
    assert steps[0]["name"] == "Proven by a prooft attestation"


def test_unproven_clippy_job_still_checks_out_and_lints() -> None:
    names = _names(_executed(_load("_build-target.yml"), "build", _clippy_context(proven=False)))
    assert {"Checkout source", "Look up pass marker", "Skip unchanged inputs"} <= names


@pytest.mark.parametrize("leg", DYLINT_PASSES)
def test_proven_dylint_leg_runs_only_its_decision_steps(leg: str) -> None:
    doc = _load("_dylint.yml")
    steps = _executed(doc, "dylint", _dylint_context(leg, f",{leg},"))
    assert _names(steps) == DECISION_STEPS
    assert not any("uses" in step for step in steps)
    # The decision is the first step: nothing (disk cleanup, checkout) runs
    # ahead of it on a proven leg.
    assert doc["jobs"]["dylint"]["steps"][0]["name"] == "Proven by a prooft attestation"


@pytest.mark.parametrize("leg", DYLINT_PASSES)
def test_another_legs_proof_does_not_reduce_this_leg(leg: str) -> None:
    others = ",".join(p for p in DYLINT_PASSES if p != leg)
    names = _names(_executed(_load("_dylint.yml"), "dylint", _dylint_context(leg, f",{others},")))
    assert {"Checkout source", "Free runner disk", "Set up soldr"} <= names


def test_the_attested_decision_matches_the_lint_only_definition() -> None:
    """The early decision restates the pass key's lint-only test; keep them equal."""
    text = (WORKFLOWS / "_build-target.yml").read_text(encoding="utf-8")
    lint_only = re.search(r"LINT_ONLY: \$\{\{ (.+) \}\}", text)
    attested = re.search(r"ATTESTED: \$\{\{ inputs\.attested && (.+) \}\}", text)
    assert lint_only is not None
    assert attested is not None
    assert attested.group(1) == lint_only.group(1)


@pytest.mark.parametrize(
    "proof", ["", ",host,", ",host,x86_64-pc-windows-msvc,aarch64-apple-darwin,crates,"]
)
def test_workspace_proofs_never_reduce_hosted_crate_checks(proof: str) -> None:
    doc = _load("_dylint.yml")
    names = _names(_executed(doc, "dylint", _dylint_context("crates", proof)))
    assert {
        "Checkout source",
        "Set up soldr",
        "Prepare verified Dylint prebuilts",
        "Check dylint formatting",
        "Test dylint crates",
    } <= names


def test_hosted_crate_checks_discover_all_libraries_and_share_cached_test_artifacts() -> None:
    steps = _load("_dylint.yml")["jobs"]["dylint"]["steps"]
    checks = {step["name"]: step for step in steps if "name" in step}
    for name in ("Check dylint formatting", "Test dylint crates"):
        script = checks[name]["run"]
        assert '[ "$DYLINT_PASS" = "crates" ]' in script
        assert "for manifest in dylints/*/Cargo.toml; do" in script
        assert '(cd "$(dirname "$manifest")" && soldr cargo ' in script
        assert "--no-cache" not in script
    env = checks["Test dylint crates"]["env"]
    assert env["RUSTUP_TOOLCHAIN"] == attest.DYLINT_TOOLCHAIN
    assert env["CARGO_TARGET_DIR"] == "${{ github.workspace }}/target/dylint-tests"
    assert 'test -f "$DYLINT_DRIVER_PATH/nightly-2026-08-15/dylint-driver"' in checks[
        "Test dylint crates"
    ]["run"]
