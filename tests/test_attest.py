"""prooft attestation for the hosted Clippy and Dylint lanes (DD-225).

Three layers, each with its own failure mode:

- the policy (`ci-attestations.yml`) and the local producer (`ci/attest.py`)
  agree on lanes, and each lane runs the commands its hosted job runs;
- prooft, driven through clud's real policy in a scratch repository, stamps
  only a passing lane and rejects a changed tree, a rebase, an edited
  trailer, stale evidence and untrusted heads (the fallback: CI runs it);
- ci.yml consumes exactly the plan outputs the policy produces, reduces a
  job in place instead of skipping it, and fails open to a full run.
"""

from __future__ import annotations

import json
import re
import shutil
import sys
from pathlib import Path

import pytest
from prooft import Repo
from prooft.attestation import Trailers
from prooft.policy import Policy
from prooft.repo import LaneFailed

from ci import attest
from tests import process

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
CI_YML = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
BUILD_YML = (WORKFLOWS / "_build-target.yml").read_text(encoding="utf-8")
DYLINT_YML = (WORKFLOWS / "_dylint.yml").read_text(encoding="utf-8")
POLICY_TEXT = attest.POLICY.read_text(encoding="utf-8")
CLIPPY_JOB = "ci.yml:lint-linux-x64"
DYLINT_PASSES = ("host", *attest.DYLINT_CROSS_TARGETS)


def _policy() -> Policy:
    return Policy.parse(POLICY_TEXT)


# ── policy <-> producer <-> workflows ───────────────────────────────────────


def test_policy_lanes_are_exactly_the_producer_lanes() -> None:
    assert set(_policy().lanes) == set(attest.lanes())


def test_policy_covers_only_clippy_and_the_dylint_legs() -> None:
    jobs = _policy().jobs
    assert set(jobs) == {CLIPPY_JOB, *(f"_dylint.yml:dylint-{p}" for p in DYLINT_PASSES)}
    assert jobs[CLIPPY_JOB] == (
        "rust/x86_64-unknown-linux-gnu/clippy",
        "rust/x86_64-unknown-linux-gnu/doctest",
    )


def test_dylint_legs_match_the_workflow_matrix() -> None:
    matrix = re.search(r"pass: \[([^\]]+)\]", DYLINT_YML)
    assert matrix is not None
    assert tuple(p.strip() for p in matrix.group(1).split(",")) == DYLINT_PASSES


def test_clippy_lane_runs_the_hosted_xbuild_commands() -> None:
    steps = attest.lanes("PY")["clippy"]
    assert [s.argv[1:] for s in steps] == [
        ("-m", "ci.xbuild", verb, "--target", attest.LINUX, "--strategy", "soldr")
        for verb in ("clippy", "doctest")
    ]
    for verb in ("clippy", "doctest"):
        assert f"-m ci.xbuild {verb} --target ${{{{ inputs.target }}}}" in BUILD_YML
    # The hosted job is the Linux x64 soldr lint-only call with doc-tests.
    clippy_job = CI_YML.split("\n  lint-linux-x64:\n", 1)[1].split("\n  build-linux-x64:\n", 1)[0]
    for line in (
        "target: x86_64-unknown-linux-gnu",
        "strategy: soldr",
        "doctest: true",
        "compile: false",
    ):
        assert line in clippy_job


def test_dylint_lanes_run_the_hosted_commands() -> None:
    assert f"SOLDR_DYLINT_TOOLCHAIN: {attest.DYLINT_TOOLCHAIN}" in DYLINT_YML
    table = attest.lanes()
    for name, steps in table.items():
        if not name.startswith("dylint"):
            continue
        for step in steps:
            command = " ".join(step.argv)
            if step.argv[:2] == ("soldr", "dylint"):
                assert step.unset == ("RUSTUP_TOOLCHAIN",)
                assert f"env -u RUSTUP_TOOLCHAIN {command}" in DYLINT_YML, command
            else:
                assert command in DYLINT_YML, command
    for target in attest.DYLINT_CROSS_TARGETS:
        assert all(("RUSTFLAGS", "-D warnings") in s.env for s in table[f"dylint-{target}"])


# ── the producer's lane runner ───────────────────────────────────────────────


def test_exec_lane_stops_at_the_first_failure(tmp_path: Path) -> None:
    marker = tmp_path / "ran"
    steps = (
        attest.Step((sys.executable, "-c", "raise SystemExit(3)")),
        attest.Step((sys.executable, "-c", f"open({str(marker)!r}, 'w').close()")),
    )
    assert attest.exec_lane("t", steps) == 3
    assert not marker.exists()


def test_exec_lane_sets_and_unsets_environment(tmp_path: Path) -> None:
    out = tmp_path / "env.json"
    probe = (
        "import json, os; "
        f"json.dump({{k: os.environ.get(k) for k in ('A', 'B')}}, open({str(out)!r}, 'w'))"
    )
    step = attest.Step((sys.executable, "-c", probe), env=(("A", "set"),), unset=("B",))
    assert attest.exec_lane("t", (step,), environ={"B": "drop", "PATH": "/usr/bin:/bin"}) == 0
    assert json.loads(out.read_text(encoding="utf-8")) == {"A": "set", "B": None}


def test_lanes_never_inherit_soldr_porcelain_or_replay_verdicts() -> None:
    """Porcelain mode made a local Dylint pass lint nothing and still exit 0,
    and soldr's verdict cache then replayed that hollow pass."""
    for steps in attest.lanes().values():
        for step in steps:
            env = step.environment({"SOLDR_PORCELAIN": "1", "PATH": "/bin"})
            assert "SOLDR_PORCELAIN" not in env, step.argv
            assert env["SOLDR_VERDICTS"] == "off", step.argv


def test_unknown_lane_is_a_usage_error() -> None:
    assert attest.main(["no-such-lane"]) == 2
    assert attest.main(["--exec", "no-such-lane"]) == 2


# ── prooft on clud's policy: creation, binding, fallback ───────────────────

pytestmark_git = pytest.mark.skipif(shutil.which("git") is None, reason="git not installed")


def _git(repo: Path, *args: str) -> str:
    return process.run(["git", *args], cwd=repo, capture_output=True, text=True, check=True).stdout


def _commit(repo: Path, path: str, text: str, message: str) -> str:
    target = repo / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")
    _git(repo, "add", path)
    _git(repo, "commit", "-q", "-m", message)
    return _git(repo, "rev-parse", "HEAD").strip()


@pytest.fixture
def repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    config = tmp_path / "gitconfig"
    config.write_text(
        "[user]\n\tname = clud-test\n\temail = clud-test@example.invalid\n"
        "[commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("GIT_CONFIG_GLOBAL", str(config))
    monkeypatch.setenv("GIT_CONFIG_NOSYSTEM", "1")
    monkeypatch.setenv("GIT_CEILING_DIRECTORIES", str(tmp_path))
    root = tmp_path / "repo"
    root.mkdir()
    _git(root, "init", "-q")
    _commit(root, "ci-attestations.yml", POLICY_TEXT, "base: policy")
    _git(root, "branch", "base")
    _commit(root, "src/lib.rs", "pub fn f() {}\n", "feature")
    return root


PASS = (sys.executable, "-c", "pass")


def _plan(root: Path, **kwargs: object) -> dict[str, bool]:
    plan = Repo(root).plan("base", "HEAD", **kwargs)  # type: ignore[arg-type]
    return {job.job: job.skip for job in plan.jobs}


@pytestmark_git
def test_a_passing_lane_stamps_its_gates_and_reduces_its_job(repo: Path) -> None:
    Repo(repo).lane("clippy").run(list(PASS))
    gates = {a.gate for a in Trailers.parse(_git(repo, "log", "-1", "--format=%B")).attestations}
    assert gates == {
        "rust/x86_64-unknown-linux-gnu/clippy",
        "rust/x86_64-unknown-linux-gnu/doctest",
    }
    plan = _plan(repo)
    assert plan[CLIPPY_JOB] is True
    assert not any(skip for job, skip in plan.items() if job != CLIPPY_JOB)


@pytestmark_git
def test_one_dylint_leg_reduces_only_itself(repo: Path) -> None:
    Repo(repo).lane("dylint-aarch64-apple-darwin").run(list(PASS))
    plan = _plan(repo)
    assert plan["_dylint.yml:dylint-aarch64-apple-darwin"] is True
    assert plan["_dylint.yml:dylint-host"] is False
    assert plan["_dylint.yml:dylint-x86_64-pc-windows-msvc"] is False


@pytestmark_git
def test_a_failing_lane_stamps_nothing(repo: Path) -> None:
    before = _git(repo, "rev-parse", "HEAD")
    with pytest.raises(LaneFailed):
        Repo(repo).lane("clippy").run([sys.executable, "-c", "raise SystemExit(1)"])
    assert _git(repo, "rev-parse", "HEAD") == before
    assert _plan(repo)[CLIPPY_JOB] is False


@pytestmark_git
def test_a_dirty_tree_is_refused(repo: Path) -> None:
    (repo / "src" / "lib.rs").write_text("pub fn g() {}\n", encoding="utf-8")
    with pytest.raises(RuntimeError, match="dirty"):
        Repo(repo).lane("clippy").run(list(PASS))


@pytestmark_git
def test_a_changed_tree_invalidates_the_proof(repo: Path) -> None:
    Repo(repo).lane("clippy").run(list(PASS))
    (repo / "src" / "lib.rs").write_text("pub fn g() {}\n", encoding="utf-8")
    _git(repo, "commit", "-q", "-a", "--amend", "--no-edit")
    assert str(Repo(repo).verify().state("rust/x86_64-unknown-linux-gnu/clippy")) == "wrong-tree"
    assert _plan(repo)[CLIPPY_JOB] is False


@pytestmark_git
def test_new_parents_invalidate_the_proof(repo: Path) -> None:
    Repo(repo).lane("clippy").run(list(PASS))
    message = _git(repo, "log", "-1", "--format=%B")
    tree = _git(repo, "rev-parse", "HEAD^{tree}").strip()
    _git(repo, "checkout", "-q", "base")
    other = _commit(repo, "README.md", "moved main\n", "main moved")
    rebased = process.run(
        ["git", "commit-tree", tree, "-p", other, "-F", "-"],
        cwd=repo,
        input=message,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    _git(repo, "checkout", "-q", rebased)
    state = Repo(repo).verify().state("rust/x86_64-unknown-linux-gnu/clippy")
    assert str(state) == "wrong-parents"


@pytestmark_git
def test_an_edited_trailer_is_rejected(repo: Path) -> None:
    Repo(repo).lane("clippy").run(list(PASS))
    message = _git(repo, "log", "-1", "--format=%B").replace('"secs":0', '"secs":1')
    message = re.sub(r'"at":(\d+)', lambda m: f'"at":{int(m.group(1)) + 1}', message)
    _git(repo, "commit", "-q", "--amend", "-m", message)
    assert str(Repo(repo).verify().state("rust/x86_64-unknown-linux-gnu/clippy")) == "bad-stamp"
    assert _plan(repo)[CLIPPY_JOB] is False


@pytestmark_git
def test_stale_evidence_runs_hosted(repo: Path) -> None:
    Repo(repo).lane("clippy").run(list(PASS))
    at = max(a.at for a in Trailers.parse(_git(repo, "log", "-1", "--format=%B")).attestations)
    plan = Repo(repo).plan("base", "HEAD", now=at + 25 * 3600)
    assert not plan.skips(CLIPPY_JOB)


@pytestmark_git
@pytest.mark.parametrize(
    ("kwargs", "surface"),
    [
        ({"fork": True}, None),
        ({"title": "fix it [ci-full]"}, None),
        ({}, ".github/workflows/ci.yml"),
        ({}, "ci-attestations.yml"),
    ],
)
def test_untrusted_heads_reduce_nothing(
    repo: Path, kwargs: dict[str, object], surface: str | None
) -> None:
    if surface is not None:
        _commit(
            repo,
            surface,
            POLICY_TEXT + "# edited\n" if surface.endswith("attestations.yml") else "x\n",
            "touch",
        )
    Repo(repo).lane("clippy").run(list(PASS))
    assert _plan(repo, **kwargs)[CLIPPY_JOB] is False


@pytestmark_git
def test_the_base_policy_judges_not_the_head(repo: Path) -> None:
    """The base's policy decides which gates a job needs, whatever the head says."""
    Repo(repo).lane("clippy").run(list(PASS))
    # Base policy still demands both gates; both were stamped, so it skips.
    assert _plan(repo)[CLIPPY_JOB] is True
    # Remove the doctest trailer: the base policy now refuses the reduction.
    message = "\n".join(
        line
        for line in _git(repo, "log", "-1", "--format=%B").splitlines()
        if "doctest" not in line
    )
    _git(repo, "commit", "-q", "--amend", "-m", message)
    assert _plan(repo)[CLIPPY_JOB] is False


# ── ci.yml: consumption, in-place reduction, fail-open ──────────────────────


def _static_block() -> str:
    return CI_YML.split("\n  static:\n", 1)[1].split("\n  static-checks:\n", 1)[0]


def test_ci_consumes_every_policy_job_output() -> None:
    static = _static_block()
    for job in _policy().jobs:
        name = job.split(":", 1)[1]
        for step in ("prooft", "prooft_queue"):
            assert f"steps.{step}.outputs['skip_{name}']" in static, (step, job)
    for leg in DYLINT_PASSES:
        either = (
            f"(steps.prooft.outputs['skip_dylint-{leg}'] || "
            f"steps.prooft_queue.outputs['skip_dylint-{leg}']) == 'true' && '{leg}'"
        )
        assert either in static, leg


def test_verifier_is_pinned_base_judged_same_repo_and_fail_open() -> None:
    step = (
        _static_block()
        .split("- name: Judge prooft attestations", 1)[1]
        .split("\n      - name:", 1)[0]
    )
    assert "if: github.event_name == 'pull_request'" in step
    assert "github.event.pull_request.head.repo.full_name == github.repository" in step
    assert "continue-on-error: true" in step
    pin = re.search(r"prooft==([0-9.]+) --hash=sha256:([0-9a-f]{64})", step)
    assert pin is not None
    assert f'"prooft=={pin.group(1)}"' in (ROOT / "pyproject.toml").read_text(encoding="utf-8")
    assert "--require-hashes" in step
    assert "--no-deps" in step
    assert "python -I -c" in step
    assert '--base "$BASE_SHA" --head "$HEAD_SHA"' in step
    assert '--title "$PR_TITLE"' in step


def test_reduction_is_in_place_and_only_for_the_lint_only_call() -> None:
    clippy_job = CI_YML.split("\n  lint-linux-x64:\n", 1)[1].split("\n  build-linux-x64:\n", 1)[0]
    assert "attested: ${{ needs.static.outputs.attested_clippy == 'true' }}" in clippy_job
    assert "attested_passes: ${{ needs.static.outputs.attested_dylint }}" in CI_YML
    # DD-226: the attested decision is made before checkout (it needs only
    # the caller's inputs); tests/test_attest_noop.py walks the steps.
    assert "ATTESTED: ${{ inputs.attested && inputs.clippy && !inputs.compile" in BUILD_YML
    assert (
        "ATTESTED: ${{ contains(inputs.attested_passes, format(',{0},', matrix.pass)) }}"
        in DYLINT_YML
    )
    for text in (BUILD_YML, DYLINT_YML):
        # Every check is gated on the combined decision; nothing still keys
        # on the raw pass-marker hit, which would ignore the attestation.
        assert "steps.pass.outputs.cache-hit != 'true'" not in text
        assert "steps.skip.outputs.reduced != 'true'" in text
        assert "PROVEN: ${{ steps.attested.outputs.proven }}" in text
        assert "Ci-Attestation on this head (DD-225); skipping" in text
    # Compilation is never gated on the reduction.
    build_step = BUILD_YML.split("- name: Build binaries and test harnesses", 1)[1].split(
        "\n      - name:", 1
    )[0]
    assert "reduced" not in build_step


_DYLINT_CASES = [
    ({}, set()),
    ({"skip_dylint-host": "true"}, {"host"}),
    (
        {"skip_dylint-host": "false", "skip_dylint-aarch64-apple-darwin": "true"},
        {"aarch64-apple-darwin"},
    ),
    ({f"skip_dylint-{p}": "true" for p in DYLINT_PASSES}, set(DYLINT_PASSES)),
]


@pytest.mark.parametrize("judge", ["prooft", "prooft_queue"])
@pytest.mark.parametrize(("outputs", "expected"), _DYLINT_CASES)
def test_attested_dylint_expression_selects_only_proven_legs(
    judge: str, outputs: dict[str, str], expected: set[str]
) -> None:
    """Evaluate the format() output the way Actions does, then each leg's contains().

    Exactly one judge runs per event (PR or merge group); the other step's
    outputs are empty strings, which `||` passes over.
    """
    expr = _static_block().split("attested_dylint: >-", 1)[1].split("    steps:", 1)[0]
    legs = re.findall(
        r"\(steps\.prooft\.outputs\['skip_dylint-([^']+)'\] \|\| "
        r"steps\.prooft_queue\.outputs\['skip_dylint-\1'\]\) == 'true' && '([^']+)'",
        expr,
    )
    assert [leg for leg, _ in legs] == list(DYLINT_PASSES)
    steps = {"prooft": {}, "prooft_queue": {}}
    steps[judge] = outputs
    value = []
    for leg, name in legs:
        key = f"skip_dylint-{leg}"
        either = steps["prooft"].get(key, "") or steps["prooft_queue"].get(key, "")
        value.append(name if either == "true" else "")
    joined = "," + ",".join(value) + ","
    assert {leg for leg in DYLINT_PASSES if f",{leg}," in joined} == expected


def test_queue_judge_runs_only_in_merge_groups_from_the_base() -> None:
    step = (
        _static_block()
        .split("- name: Judge merge-queue attestation forwarding", 1)[1]
        .split("\n      - name:", 1)[0]
    )
    assert "if: github.event_name == 'merge_group'" in step
    assert "continue-on-error: true" in step
    # The shipped "off" costs no fetch or install: the base switch is read
    # first, and only on/shadow continues.
    gate = step.index('git show "$BASE_SHA:ci-attestations-queue.toml"')
    assert gate < step.index("git fetch") < step.index("pip install")
    # The judge and its policy come from the queue's base, never the PR.
    assert 'git archive "$BASE_SHA" ci' in step
    assert "python -I -c" in step
    assert "from ci.attest_queue import main" in step
    # The same hash-pinned prooft the PR judge installs.
    pin = re.search(r"prooft==([0-9.]+) --hash=sha256:([0-9a-f]{64})", step)
    pr_pin = re.search(r"prooft==([0-9.]+) --hash=sha256:([0-9a-f]{64})", _static_block())
    assert pin is not None
    assert pr_pin is not None
    assert pin.groups() == pr_pin.groups()
