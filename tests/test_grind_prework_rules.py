"""Static guards for /grind prework (#1408) and problem reporting (#1411).

The harness tests (`tests/harness/test_grind_prework.py`,
`tests/harness/test_grind_problems.py`) drive the real Claude Code and are
opt-in. These read the bundled assets and sources directly, so the contract
they pin holds in every `bash test` run.
"""

from __future__ import annotations

import re
from collections.abc import Iterator
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = "crates/clud-bin/assets/workflows/grind-run.js"
ROLES = ("planner", "editor", "reviewer", "integrator", "lander")


def _read(rel: str) -> str:
    return (ROOT / rel).read_text(encoding="utf-8")


def _skill(name: str) -> str:
    return _read(f"crates/clud-bin/assets/skills/{name}/SKILL.md")


def test_prework_role_is_registered_in_every_place() -> None:
    # CLAUDE.md: a new grind-* role needs its agent file, a claude_files.rs
    # entry and a policy in block_bad_cmd_grind_caps.rs.
    agent = _read("crates/clud-bin/assets/agents/grind-prework.md")
    assert "\nname: grind-prework\n" in agent
    tools = next(line for line in agent.splitlines() if line.startswith("tools: "))
    assert "Write" not in tools, tools
    assert "Edit" not in tools, tools
    assert '"agents/grind-prework.md"' in _read("crates/clud-bin/src/claude_files.rs")
    assert 'name: "grind-prework"' in _read("crates/clud-bin/src/skills.rs")
    caps = _read("crates/clud-bin/src/block_bad_cmd_grind_caps.rs")
    assert 'PREWORK: &str = "grind-prework"' in caps
    assert "        PREWORK => {" in caps
    listed = re.search(r"fn is_grind_role.*?matches!\((.*?)\)", caps, re.DOTALL)
    assert listed, "is_grind_role not found"
    for role in ("PLANNER", "EDITOR", "REVIEWER", "INTEGRATOR", "LANDER", "PREWORK"):
        assert role in listed.group(1), role


def _agent_calls(src: str) -> Iterator[tuple[str, str, str]]:
    """(role, label, prompt source) for each `agent(` call in the workflow."""
    for match in re.finditer(r"\bagent\(", src):
        end = src.find("opts(", match.end())
        assert end != -1, src[match.start() : match.start() + 300]
        head = re.match(r"opts\('(\w+)',\s*(\S+?),", src[end:])
        assert head, src[end : end + 120]
        yield head.group(1), head.group(2), src[match.end() : end]


def test_every_agent_after_prework_is_told_the_plan_url() -> None:
    # P4: every planner, worker, reviewer, integrator and lander prompt
    # carries the plan comment URL, through ctx(g) or PLAN_URL directly. Only
    # the plan-only classify pass and prework itself run before it exists.
    calls = list(_agent_calls(_read(WORKFLOW)))
    assert {role for role, _, _ in calls} >= {"prework", *ROLES}, calls
    assert "${ctx(g)}" in _read(WORKFLOW).split("const handoffOf = ")[1].split("\n")[0]
    for role, label, prompt in calls:
        # A chaperone's prompt starts with its handoff, which holds ctx(g).
        if role in ("prework", "chaperone") or label == "'classify'":
            continue
        assert "ctx(" in prompt or "PLAN_URL" in prompt, (role, label)


def test_plan_bodies_are_shell_inert_and_capped() -> None:
    src = _read(WORKFLOW)
    # A backtick fence reads as command substitution to clud's command hook,
    # even inside quotes, so the body uses a tilde fence and \u escapes.
    assert "'\\n~~~json\\n' + inert(" in src
    assert "```json" not in src
    assert "const PLAN_LIMIT = 65536" in src
    assert "b.length >= PLAN_LIMIT" in src
    assert "stopping before any worker" in src
    assert "part=${k}/${n}" in src


def test_prework_skill_pipes_the_body_from_printf() -> None:
    # A heredoc body is parsed line by line by clud's hook, and a backtick
    # reads as a substitution even in quotes; `printf '%s' '<body>' |` is the
    # posting shape every hook layer (and the prework caps) accepts.
    text = _skill("grind-prework")
    assert "printf '%s' '<the body, exactly as given>' | gh issue comment <meta>" in text
    assert "--body-file -" in text
    assert "Do not use a heredoc" in text
    assert "~~~json" in text
    assert "Never edit or delete any comment" in text
    assert "never file issues" in text


def test_router_owns_the_status_comment_and_problem_filing() -> None:
    text = _skill("grind")
    assert "<!-- grind:v1 status run=<run-id> -->" in text
    assert "gh api -X PATCH repos/<o>/<r>/issues/comments/<id>" in text
    assert "Refs #<meta>" in text
    assert "--label grind:followup" not in text
    assert "gh label create" not in text
    assert "NEVER attach a follow-up as a sub-issue" in text
    assert "Problems not filed" in text
    assert "stopped: 'prework'" in text


def test_roles_return_problems_instead_of_filing_them() -> None:
    for role in ROLES:
        agent = _read(f"crates/clud-bin/assets/agents/grind-{role}.md")
        assert "never run `gh issue create`" in agent, role
        assert "`problems`" in agent, role
    src = _read(WORKFLOW)
    # PLAN, WORK, REVIEW, INTEG, LAND, PREWORK and CLASSIFY all accept it.
    assert src.count("problems: PROBLEMS") >= 7, src.count("problems: PROBLEMS")


def test_followups_wait_for_their_feature_pr() -> None:
    for name in ("grind-intake", "grind-cron", "clud-issue-triage"):
        text = _skill(name)
        assert "grind:followup" in text, name
        assert "stage=bugs" in text, name
        assert "feature-pr=" in text, name
        assert "MERGED" in text, name
    for name in ("grind-intake", "grind-cron"):
        assert "--remove-label" not in _skill(name), name
