use super::*;

const PREAMBLE: &str = "Execute the brief at /tmp/do-goals/brief.md exactly. Target: ";

/// A ~6,600-character brief: long prose with hard constraints scattered in it.
fn long_brief() -> String {
    let filler = "This paragraph explains background context at length and carries
background colour, it is narrative about the history of the feature. ";
    let mut brief = String::new();
    brief.push_str("Implement the parser fix in crates/clud-bin/src/args.rs.\n");
    brief.push_str(&filler.repeat(20));
    brief.push_str("\nDo not touch ~/dev/clud or ~/dev/clud4.\n");
    brief.push_str(&filler.repeat(20));
    brief.push_str("\nNever push to main; open a PR instead.\n");
    brief.push_str(&filler.repeat(20));
    brief.push_str("\nAcceptance: tests/test_mcp_validation.py passes.\n");
    brief.push_str(&filler.repeat(5));
    brief
}

fn assert_constraints_survive(brief: &str, condition: &str) {
    for token in path_tokens(brief) {
        assert!(condition.contains(&token), "path token {token:?} lost");
    }
    let lower = condition.to_lowercase();
    assert!(lower.contains("do not touch ~/dev/clud or ~/dev/clud4"));
    assert!(lower.contains("never push to main"));
    assert!(lower.contains("acceptance: tests/test_mcp_validation.py passes"));
}

#[test]
fn a_brief_that_fits_is_quoted_whole_without_calling_the_model() {
    let brief = "Fix the typo in README.md. Never push.";
    let goal = compact_goal(PREAMBLE, brief, |_| panic!("model must not run")).unwrap();
    assert_eq!(goal.kind, CompactKind::Verbatim);
    assert_eq!(goal.condition, format!("{PREAMBLE}{brief}"));
}

#[test]
fn path_tokens_and_constraints_are_extracted() {
    let brief = long_brief();
    assert!(brief.chars().count() > 6_000);
    let paths = path_tokens(&brief);
    for want in [
        "crates/clud-bin/src/args.rs",
        "~/dev/clud",
        "~/dev/clud4",
        "tests/test_mcp_validation.py",
    ] {
        assert!(
            paths.iter().any(|p| p == want),
            "{want} missing from {paths:?}"
        );
    }
    let constraints = hard_constraints(&brief);
    assert!(constraints.iter().any(|c| c.contains("Never push to main")));
    assert!(constraints.iter().any(|c| c.starts_with("Do not touch")));
    assert!(constraints.iter().all(|c| !c.contains("narrative")));
}

#[test]
fn a_model_answer_that_keeps_every_constraint_is_the_goal() {
    let brief = long_brief();
    let answer = "Fix the parser. Implement the parser fix in crates/clud-bin/src/args.rs. \
Do not touch ~/dev/clud or ~/dev/clud4. Never push to main. open a PR instead. \
Acceptance: tests/test_mcp_validation.py passes.";
    let mut seen_prompt = String::new();
    let goal = compact_goal(PREAMBLE, &brief, |prompt| {
        seen_prompt = prompt.to_string();
        Ok(answer.to_string())
    })
    .unwrap();
    assert_eq!(goal.kind, CompactKind::Model);
    assert!(goal.condition.chars().count() <= CLAUDE_GOAL_CONDITION_LIMIT);
    assert!(seen_prompt.contains("verbatim"));
    assert!(seen_prompt.contains("- Never push to main"));
    assert_constraints_survive(&brief, &goal.condition);
}

#[test]
fn a_model_answer_that_drops_a_constraint_is_rejected_for_the_verbatim_list() {
    let brief = long_brief();
    // Paraphrases "never push" away.
    let lossy = "Fix crates/clud-bin/src/args.rs, leave ~/dev/clud alone.";
    let goal = compact_goal(PREAMBLE, &brief, |_| Ok(lossy.to_string())).unwrap();
    assert_eq!(goal.kind, CompactKind::Constraints);
    assert!(goal.condition.chars().count() <= CLAUDE_GOAL_CONDITION_LIMIT);
    assert_constraints_survive(&brief, &goal.condition);
}

#[test]
fn an_unavailable_model_still_yields_every_constraint() {
    let brief = long_brief();
    let goal = compact_goal(PREAMBLE, &brief, |_| Err("no model".into())).unwrap();
    assert_eq!(goal.kind, CompactKind::Constraints);
    assert_constraints_survive(&brief, &goal.condition);
}

#[test]
fn an_overlong_model_answer_is_rejected() {
    let brief = long_brief();
    let constraints = hard_constraints(&brief).join(" ");
    let huge = format!("{constraints} {}", "z".repeat(5_000));
    let goal = compact_goal(PREAMBLE, &brief, |_| Ok(huge)).unwrap();
    assert_eq!(goal.kind, CompactKind::Constraints);
}

#[test]
fn constraints_that_cannot_fit_fail_with_the_clause_quoted() {
    let mut brief = String::new();
    for n in 0..60 {
        brief.push_str(&format!(
            "Never modify the generated file crates/gen/out_{n:02}.rs under any circumstance.\n"
        ));
    }
    let err = compact_goal(PREAMBLE, &brief, |_| Err("no model".into())).unwrap_err();
    let CompactError::ConstraintDoesNotFit { clause, limit } = &err;
    assert_eq!(*limit, CLAUDE_GOAL_CONDITION_LIMIT);
    assert!(clause.starts_with("Never modify the generated file crates/gen/out_"));
    let message = err.to_string();
    assert!(message.contains(&format!("\"{clause}\"")), "{message}");
}

#[test]
fn the_brief_and_goal_are_recorded() {
    let home = tempfile::tempdir().unwrap();
    let dir = session_record_dir(home.path(), "clud-42-1700000000");
    let goal = CompactGoal {
        condition: "the goal".into(),
        kind: CompactKind::Model,
    };
    let brief_path = record(&dir, "the brief", &goal).unwrap();
    assert_eq!(std::fs::read_to_string(brief_path).unwrap(), "the brief");
    assert_eq!(
        std::fs::read_to_string(dir.join("goal.txt")).unwrap(),
        "the goal"
    );
}

#[test]
fn the_record_is_keyed_by_the_session_id() {
    let home = tempfile::tempdir().unwrap();
    let dir = session_record_dir(home.path(), "clud-42-1700");
    assert!(dir.ends_with(".clud/state/do-goals/clud-42-1700"));
}

/// Exercises the real `claude -p` model call; run with `--ignored` on a host
/// with an authenticated Claude Code.
#[test]
#[ignore = "calls the real claude -p model"]
fn the_real_model_compacts_a_long_brief_and_keeps_constraints() {
    let brief = long_brief();
    let goal = compact_goal(PREAMBLE, &brief, run_model).unwrap();
    eprintln!(
        "kind={:?} chars={}",
        goal.kind,
        goal.condition.chars().count()
    );
    assert!(goal.condition.chars().count() <= CLAUDE_GOAL_CONDITION_LIMIT);
    assert_constraints_survive(&brief, &goal.condition);
}
