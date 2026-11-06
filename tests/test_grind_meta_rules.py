"""Guard the /grind meta-of-metas rules in the bundled assets (#1412)."""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SKILLS = ROOT / "crates/clud-bin/assets/skills"
WORKFLOW = ROOT / "crates/clud-bin/assets/workflows/grind-run.js"


def _read(name: str) -> str:
    return (SKILLS / name / "SKILL.md").read_text(encoding="utf-8")


def test_router_regroups_with_replace_parent() -> None:
    text = _read("grind")
    assert "replace_parent" in text
    assert "## 2b. Regroup" in text


def test_router_keeps_old_body_in_details() -> None:
    text = _read("grind")
    assert "Previous content" in text
    assert "<details>" in text
    assert "no children after regrouping" in text


def test_router_keeps_grind_made_sub_metas() -> None:
    text = _read("grind")
    assert "<!-- grind:v1 -->" in text
    assert "label `grind:meta`" not in text


def test_router_records_undo() -> None:
    text = _read("grind")
    assert '"undo"' in text
    assert "reparent" in text
    assert "rewrite" in text


def test_router_nesting_guard() -> None:
    text = _read("grind")
    assert "8 levels" in text
    assert "100" in text


def test_router_one_feature_per_run() -> None:
    text = _read("grind")
    assert "deferred_groups" in text
    assert "ONE feature group" in text


def test_router_no_overlap_is_per_top_meta() -> None:
    text = _read("grind")
    assert "waiting_on_pr" in text
    assert "bugs-only" in text
    assert "different top meta" in text


def test_workflow_enforces_deferral_and_overlap() -> None:
    text = WORKFLOW.read_text(encoding="utf-8")
    assert "deferred_groups" in text
    assert "waiting_on_pr" in text
    assert "no overlap" in text


def test_router_feature_pick_defaults_to_dependency_order() -> None:
    text = _read("grind")
    assert "dependency order" in text
    assert '"(Recommended)"' in text
    assert "Keep as is (simple schedule)" in text


def test_router_no_overlap_covers_sub_meta_feature_prs() -> None:
    """A meta of metas names each feature branch after its sub-meta, so the
    check must look at `T`'s sub-issues, and only at PRs into `<main>`."""
    text = _read("grind")
    assert "--base <main> --search 'head:grind/meta-'" in text
    assert "one of `T`'s sub-issues" in text
    assert "move every feature group into `deferred_groups`" in text


def test_router_attaches_sub_issues_by_rest_id() -> None:
    text = _read("grind")
    assert "gh api repos/<o>/<r>/issues/<n> --jq .id" in text


def test_workflow_defers_children_of_groups_outside_stages() -> None:
    text = WORKFLOW.read_text(encoding="utf-8")
    assert "args.plan.deferred_groups : []).forEach" in text
    assert "const NO_FEATURE_LEFT = BUGS_ONLY ||" in text


def test_router_offers_to_close_an_abandoned_feature_pr() -> None:
    """A stalled run's feature PR must not silently force every later run to
    bugs-only (#1819): the router checks liveness and asks."""
    text = _read("grind")
    assert "clud grind-facts owner <n>" in text
    assert "Blocking feature PR" in text
    assert "A live PR is never offered for closing." in text


def test_feature_branch_gets_an_opening_commit_before_the_draft_pr() -> None:
    text = _read("grind")
    assert "GitHub refuses a\n   PR with no commits" in text
    assert "commit --allow-empty" in text


def test_workflow_maps_role_models_onto_each_agent() -> None:
    """#1810: the Models answers reach `agent().model`; the lander shares the
    integrator's model and prework the planner's."""
    text = WORKFLOW.read_text(encoding="utf-8")
    assert "const MODELS = args.models || {}" in text
    assert (
        "MODELS[role === 'lander' ? 'integrator' : role === 'prework' ? 'planner' : role]" in text
    )
    assert "role === 'chaperone' ? MODELS.chaperone || 'opus'" in text
    assert "if (m) o.model = m" in text


def test_router_warns_that_a_wire_id_escapes_a_gateway_pin() -> None:
    """#1810: under a direct gateway pin only tier aliases stay inside it."""
    text = " ".join(_read("grind").split())
    assert "the tier aliases `opus`, `sonnet` and `haiku` resolve to the pinned id" in text
    assert "billed outside the pin" in text


def test_finish_makes_the_status_comment_final() -> None:
    """#1812: a live OpenRouter run left `#1818: running` in its status
    comment, because only section 4 mentioned the final edit."""
    text = _read("grind")
    finish = text[text.index("## 5. Finish") :]
    flat = " ".join(finish.split())
    assert "Then make the status comment final" in flat
    assert "none still reads `running`" in flat
