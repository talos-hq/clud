"""Publication must survive skipped recovery builders without bypassing gates."""

import ast
import re
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent
PUBLICATION = (
    "publish-candidate-catalog",
    "verify-candidate-public",
    "verify-candidate-installer",
    "verify-prior-public",
    "promote-release",
    "publish-homebrew",
    "build-install-site",
    "deploy-install-site",
    "verify-promoted-public",
    "verify-published-installer",
)
PUBLIC_ACCEPTANCE = (
    "public-host",
    "public-nixos",
    "public-distros",
    "catalog-unit",
    "resolver-unit",
    "catalog-site",
)


def job(workflow, name):
    text = (ROOT / ".github/workflows" / workflow).read_text()
    jobs = dict(re.findall(r"(?ms)^  ([a-z][a-z0-9-]*):\n(.*?)(?=^  [a-z][a-z0-9-]*:\n|\Z)", text))
    body = jobs[name]
    needs = re.search(r"(?m)^    needs: (.+)$", body)
    dependencies = [v.strip() for v in needs[1].strip("[]").split(",")] if needs else []
    lines = body.splitlines()
    condition = "success()"
    for index, line in enumerate(lines):
        if line.startswith("    if: "):
            condition = line.removeprefix("    if: ")
            if condition in (">-", "|", ">"):
                parts = []
                for continuation in lines[index + 1 :]:
                    if not continuation.startswith("      "):
                        break
                    parts.append(continuation.strip())
                condition = " ".join(parts)
            break
    return dependencies, condition.removeprefix("${{").removesuffix("}}").strip()


def holds(
    condition,
    states,
    *,
    ancestor_skipped,
    cancelled=False,
    tag="v2.8.65",
    event="workflow_dispatch",
    labels=(),
):
    # GitHub adds success() when no status-check function is present. That
    # includes *ancestors*, not only the direct needs which may all be green.
    if not re.search(r"\b(?:always|success|failure|cancelled)\s*\(", condition):
        condition = "success() && (" + condition + ")"
    condition = re.sub(r"needs\.([a-z][a-z0-9-]*)\.result", lambda m: repr(states[m[1]]), condition)
    condition = condition.replace("inputs.release_tag", repr(tag)).replace(
        "inputs.resume_run_id", repr("123")
    )
    condition = condition.replace("github.event_name", repr(event))
    condition = re.sub(
        r"contains\(github\.event\.pull_request\.labels\.\*\.name, '([^']+)'\)",
        lambda m: repr(m[1] in labels),
        condition,
    )
    condition = condition.replace("always()", "True").replace("cancelled()", repr(cancelled))
    condition = condition.replace(
        "success()",
        repr(
            not ancestor_skipped and not cancelled and all(v == "success" for v in states.values())
        ),
    )
    condition = condition.replace("&&", " and ").replace("||", " or ")
    condition = re.sub(r"!(?!=)", "not ", condition)

    def evaluate(node):
        if isinstance(node, ast.Constant):
            return node.value
        if isinstance(node, ast.BoolOp):
            values = [bool(evaluate(value)) for value in node.values]
            return all(values) if isinstance(node.op, ast.And) else any(values)
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.Not):
            return not evaluate(node.operand)
        if isinstance(node, ast.Compare) and len(node.ops) == 1:
            left, right = evaluate(node.left), evaluate(node.comparators[0])
            if isinstance(node.ops[0], ast.Eq):
                return left == right
            if isinstance(node.ops[0], ast.NotEq):
                return left != right
        raise AssertionError(f"unsupported condition node: {ast.dump(node)}")

    return bool(evaluate(ast.parse(condition.strip(), mode="eval").body))


@pytest.mark.parametrize("name", PUBLICATION)
def test_recovery_publication_continues_after_skipped_builders(name):
    dependencies, condition = job("auto-release.yml", name)
    states = dict.fromkeys(dependencies, "success")
    assert holds(condition, states, ancestor_skipped=True), name
    assert holds(condition, states, ancestor_skipped=False), name
    assert not holds(condition, states, ancestor_skipped=True, cancelled=True), name
    for dependency in dependencies:
        for state in ("failure", "skipped", "cancelled"):
            failed = {**states, dependency: state}
            assert not holds(condition, failed, ancestor_skipped=True), (name, dependency, state)


@pytest.mark.parametrize("name", PUBLIC_ACCEPTANCE)
def test_called_acceptance_retains_eligibility_after_skipped_builders(name):
    dependencies, condition = job("installer-check.yml", name)
    states = dict.fromkeys(dependencies, "success")
    assert holds(condition, states, ancestor_skipped=True), name
    assert not holds(condition, states, ancestor_skipped=True, cancelled=True), name
    assert not holds(condition, states, ancestor_skipped=True, tag="", event="pull_request"), name
    expected_full_pr = name in ("catalog-unit", "resolver-unit", "catalog-site")
    assert (
        holds(
            condition,
            states,
            ancestor_skipped=True,
            tag="",
            event="pull_request",
            labels=("ci-full",),
        )
        is expected_full_pr
    )


@pytest.mark.parametrize("name", ["publish-release", "publish-pypi"])
def test_publishers_honor_cancellation_even_when_all_proofs_pass(name):
    dependencies, condition = job("auto-release.yml", name)
    states = dict.fromkeys(dependencies, "success")
    assert not holds(condition, states, ancestor_skipped=True, cancelled=True), name
