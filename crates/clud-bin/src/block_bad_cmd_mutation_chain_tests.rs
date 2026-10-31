//! zackees/clud#1979: fixtures are the issue's transcript examples.

use super::tests::temp_env;
use super::*;

fn reason(command: &str) -> Option<String> {
    let mut reason = None;
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        reason = evaluate_command(command, None, false, &[]).reason;
    });
    reason
}

#[test]
fn a_mutation_chained_with_a_watch_or_another_mutation_is_refused() {
    for command in [
        "git push -q -u origin fix/x 2>&1 | grep -v '^remote'; gh pr create --repo o/r --title t --body b; clud tool run github/pr_merge_watch.py 12",
        "git push -q -u origin fix/y 2>&1 | tail -1; gh pr create --repo o/r --title t --body b",
        "cd /tmp && gh pr create --base master --head lint/x --title t --body b; clud tool run github/pr_merge_watch.py 4",
        "git commit -q --allow-empty -m retry && git push -q 2>&1|tail -1; timeout 1700 clud tool run github/pr_merge_watch.py 4812 2>&1 | tail -3",
        "for r in bouf design; do gh pr merge 1 -R o/$r --squash --admin --delete-branch 2>&1|tail -1; done",
        "git push origin HEAD && gh pr create --fill && gh pr merge --auto --squash",
        "git push && \"$CLUD_EXE\" tool run github/run_watch.py 99",
        "for n in 1 2; do gh api -X POST repos/o/r/issues/9/sub_issues -F sub_issue_id=$n; done; gh pr merge 3",
        "gh issue comment 9 --body x; gh pr merge 3",
        "gh api -X POST repos/o/r/issues/1/labels -f labels[]=x && git push",
        "gh issue close 4; clud tool run github/pr_merge_watch.py 5",
    ] {
        let reason = reason(command).unwrap_or_else(|| panic!("{command} should be refused"));
        assert!(reason.contains("[mutation-chain]"), "{command}: {reason}");
        assert!(reason.contains("pr_merge_watch"), "{command}: {reason}");
        // #2429: name every allowed shape, so agents stop splitting batches
        // the rule admits.
        assert!(reason.contains("gh pr create ...` (unpiped"), "{reason}");
        assert!(reason.contains("`gh api` writes"), "{reason}");
        assert!(reason.contains("metadata writes"), "{reason}");
        assert!(!reason.contains("one allowed pair"), "{reason}");
    }
}

#[test]
fn read_only_chains_single_mutations_and_push_then_create_are_allowed() {
    for command in [
        "gh pr view 1 && gh pr checks 1",
        "git status; git log -3",
        "git add -A && git commit -qm x && git push -u origin HEAD",
        "git push -u origin HEAD && gh pr create --fill",
        "gh pr merge 5 --squash --delete-branch && git fetch origin",
        "gh api repos/o/r/actions/runs/1 --jq .status",
        "gh issue create --title t --body \"$(cat <<'EOF'\nrun gh pr merge 1; then gh pr checks --watch\nEOF\n)\"",
        "gh pr create --title t --body 'git push && gh pr merge 3 && clud tool run github/pr_merge_watch.py 3'",
        "clud tool run github/pr_merge_watch.py 12 --repo o/r --timeout 3500",
        // #2354: one idempotent `gh api` write repeated by a loop.
        "for n in 2368 2372; do gh api -X POST repos/o/r/issues/2315/sub_issues -F sub_issue_id=$n; done",
        // #2354: batches of independent issue-metadata writes.
        "gh api -X POST repos/o/r/issues/1/labels -f labels[]=x && gh api --method DELETE repos/o/r/issues/1/labels/y",
        "for n in 1 2; do gh api -X POST repos/o/r/issues/$n/labels -f labels[]=x; gh issue close $n; done",
        "for n in 11 12 13; do gh api -X POST repos/o/r/issues/9/sub_issues -F sub_issue_id=$n; done; gh issue comment 9 --body done",
        "gh issue edit 3 --add-label x && gh issue close 4",
    ] {
        assert_eq!(reason(command), None, "{command}");
    }
}

#[test]
fn the_override_admits_one_chained_command() {
    let mut reason = Some(String::new());
    temp_env(
        BAD_CMD_OVERRIDE_ENV,
        "mutation-chain:release script",
        || {
            reason = evaluate_command("git push && git push --tags", None, false, &[]).reason;
        },
    );
    assert_eq!(reason, None);
}
