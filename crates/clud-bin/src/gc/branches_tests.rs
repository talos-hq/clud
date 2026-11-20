//! Rendering and nudge tests for `clud gc branches` (DD-230). The verdict and
//! the real-git retire are tested in `daemon::gc_service::repo_branch*`.

use std::path::PathBuf;

use super::*;
use crate::daemon::repo_branch::RepoBranchVerdict;
use crate::daemon::repo_branch_probe::BranchRow;

fn row(name: &str, checkout: BranchCheckout, action: BranchAction, reason: &str) -> BranchRow {
    BranchRow {
        name: name.to_string(),
        tip: "abc".to_string(),
        checkout,
        remote_tip: None,
        verdict: RepoBranchVerdict {
            action,
            reason: reason.to_string(),
        },
    }
}

fn report(rows: Vec<BranchRow>, setting: Option<bool>) -> BranchReport {
    BranchReport {
        repo_root: PathBuf::from("/r"),
        default_branch: "main".to_string(),
        default_ref: "origin/main".to_string(),
        rows,
        remote_rows: Vec::new(),
        delete_branch_on_merge: setting,
        prs_available: true,
    }
}

#[test]
fn a_landed_current_branch_gets_the_retire_nudge() {
    let report = report(
        vec![row(
            "feat/x",
            BranchCheckout::Current,
            BranchAction::Retire,
            "merged via PR #2859",
        )],
        None,
    );
    let nudge = nudge_text(&report).expect("landed current branch is nudged");
    assert!(nudge.contains("`feat/x`"), "{nudge}");
    assert!(nudge.contains("merged via PR #2859"), "{nudge}");
    assert!(nudge.contains(RETIRE_COMMAND), "{nudge}");
    assert!(nudge.contains("`main`"), "{nudge}");
}

#[test]
fn follow_up_work_is_nudged_toward_a_new_pr() {
    let report = report(
        vec![row(
            "feat/x",
            BranchCheckout::Current,
            BranchAction::FollowUp,
            "commits after merged PR #5",
        )],
        None,
    );
    let nudge = nudge_text(&report).unwrap();
    assert!(nudge.contains("Open a new PR"), "{nudge}");
    assert!(!nudge.contains(RETIRE_COMMAND), "{nudge}");
}

#[test]
fn kept_and_non_current_branches_are_not_nudged() {
    let kept = report(
        vec![row(
            "feat/x",
            BranchCheckout::Current,
            BranchAction::Keep,
            "no PR",
        )],
        None,
    );
    assert_eq!(nudge_text(&kept), None);
    let elsewhere = report(
        vec![row(
            "feat/y",
            BranchCheckout::None,
            BranchAction::Retire,
            "merged via PR #1",
        )],
        None,
    );
    assert_eq!(nudge_text(&elsewhere), None);
}

#[test]
fn the_report_names_the_retire_command_and_warns_about_the_setting() {
    let report = report(
        vec![
            row("keep-me", BranchCheckout::None, BranchAction::Keep, "no PR"),
            row(
                "done",
                BranchCheckout::Current,
                BranchAction::Retire,
                "merged via PR #1",
            ),
        ],
        Some(false),
    );
    let text = render(&report, &[], false);
    let retire_at = text.find("retire     done").expect(&text);
    let keep_at = text.find("keep       keep-me").expect(&text);
    assert!(retire_at < keep_at, "retire rows sort first:\n{text}");
    assert!(text.contains("[current]"), "{text}");
    assert!(text.contains(RETIRE_COMMAND), "{text}");
    assert!(text.contains(ENABLE_SETTING_COMMAND), "{text}");
}

#[test]
fn no_setting_warning_when_enabled_or_unknown() {
    for setting in [Some(true), None] {
        let text = render(&report(Vec::new(), setting), &[], false);
        assert!(!text.contains("delete_branch_on_merge"), "{text}");
    }
}

#[test]
fn json_carries_every_row_and_the_setting() {
    let report = report(
        vec![row(
            "done",
            BranchCheckout::Current,
            BranchAction::Retire,
            "merged via PR #1",
        )],
        Some(false),
    );
    let doc = report_json(&report, &[]);
    assert_eq!(doc["delete_branch_on_merge"], false);
    assert_eq!(doc["branches"][0]["action"], "retire");
    assert_eq!(doc["branches"][0]["current"], true);
}
