//! Detection, decision table and messages for the raw CI-log budget (#2233).

use super::*;

const LIMIT: u32 = DEFAULT_LIMIT;

fn run_log(run: &str) -> RawLog {
    RawLog {
        repo: Some("talos-hq/clud".to_string()),
        run: Some(run.to_string()),
        job: None,
    }
}

/// Feed one run per entry through the table, `gap` seconds apart.
fn walk(runs: &[&str], gap: u64, limit: u32) -> Vec<Verdict> {
    let mut state: Option<State> = None;
    let mut out = Vec::new();
    for (index, run) in runs.iter().enumerate() {
        let now = 1_000 + gap * u64::try_from(index).unwrap_or_default();
        let (verdict, next) = decide(state.as_ref(), &run_log(run), now, limit);
        out.push(verdict);
        state = Some(next);
    }
    out
}

fn refused(history: &[Verdict]) -> Vec<usize> {
    history
        .iter()
        .enumerate()
        .filter(|(_, verdict)| matches!(verdict, Verdict::Deny { .. }))
        .map(|(index, _)| index + 1)
        .collect()
}

fn nudged(history: &[Verdict]) -> Vec<usize> {
    history
        .iter()
        .enumerate()
        .filter(|(_, verdict)| matches!(verdict, Verdict::Nudge { .. }))
        .map(|(index, _)| index + 1)
        .collect()
}

#[test]
fn detects_the_log_failed_form() {
    let log = detect("gh run view 41639349424 -R talos-hq/clud --log-failed").unwrap();
    assert_eq!(log.repo.as_deref(), Some("talos-hq/clud"));
    assert_eq!(log.run.as_deref(), Some("41639349424"));
    assert_eq!(log.job, None);
}

#[test]
fn a_job_id_is_never_mistaken_for_the_run_id() {
    let log =
        detect("gh run view --job 125931081356 41639349424 --log 2>&1 | grep -i error | head")
            .unwrap();
    assert_eq!(log.run.as_deref(), Some("41639349424"));
    assert_eq!(log.job.as_deref(), Some("125931081356"));
}

#[test]
fn detects_the_long_repo_flag_and_equals_spelling() {
    assert_eq!(
        detect("gh run view 42 --repo=forge-ci/soldr --log")
            .unwrap()
            .repo
            .as_deref(),
        Some("forge-ci/soldr")
    );
    assert_eq!(
        detect("gh run view 42 --repo forge-ci/soldr --log")
            .unwrap()
            .repo
            .as_deref(),
        Some("forge-ci/soldr")
    );
}

#[test]
fn detects_a_run_level_api_log_path() {
    let log =
        detect("gh api repos/talos-hq/clud/actions/runs/41639349424/logs | head -40").unwrap();
    assert_eq!(log.repo.as_deref(), Some("talos-hq/clud"));
    assert_eq!(log.run.as_deref(), Some("41639349424"));
}

#[test]
fn detects_a_stage_after_a_semicolon() {
    assert!(detect("cd ~/dev/clud; gh run view 7 -R a/b --log-failed").is_some());
}

#[test]
fn json_list_and_download_are_not_log_reads() {
    for command in [
        "gh run view 42 -R forge-ci/soldr --json jobs -q '.jobs[]'",
        "gh run list -R forge-ci/soldr -L 5",
        "gh run download 42 -R forge-ci/soldr",
        "gh pr checks 2593 -R talos-hq/clud",
    ] {
        assert!(detect(command).is_none(), "{command}");
    }
}

#[test]
fn a_redirect_to_a_file_is_already_out_of_the_transcript() {
    assert!(
        detect("gh run view 42 --log-failed > /tmp/run.log; grep -n error /tmp/run.log").is_none()
    );
    assert!(detect("gh run view 42 --log-failed >> /tmp/run.log").is_none());
    // descriptor duplication is not a redirect
    assert!(detect("gh run view 42 --log-failed 2>&1 | head").is_some());
}

#[test]
fn job_level_api_paths_carry_no_run_id_and_are_left_alone() {
    assert!(detect("gh api repos/talos-hq/clud/actions/jobs/125931081356/logs").is_none());
}

#[test]
fn the_first_run_is_silent_and_the_last_allowed_run_is_nudged() {
    let history = walk(&["1", "2"], 0, LIMIT);
    assert_eq!(history[0], Verdict::Allow);
    assert_eq!(nudged(&history), vec![2]);
    assert!(refused(&history).is_empty());
}

#[test]
fn one_refusal_per_new_run_past_the_limit_and_every_retry_passes() {
    let history = walk(&["1", "2", "3", "3", "4", "4"], 0, LIMIT);
    assert_eq!(refused(&history), vec![3, 5]);
    assert_eq!(history[2], Verdict::Deny { count: LIMIT + 1 });
    assert_eq!(history[3], Verdict::Allow);
    assert_eq!(history[5], Verdict::Allow);
}

#[test]
fn rereading_the_same_run_never_counts_as_a_new_run() {
    let history = walk(&["7", "7", "7", "7"], 0, LIMIT);
    assert!(refused(&history).is_empty());
    assert!(nudged(&history).is_empty());
}

#[test]
fn a_zero_limit_disables_the_guard() {
    let history = walk(&["1", "2", "3", "4", "5"], 0, 0);
    assert!(refused(&history).is_empty());
    assert!(nudged(&history).is_empty());
    assert_eq!(history[0], Verdict::Allow);
}

#[test]
fn an_idle_gap_longer_than_the_window_starts_a_new_count() {
    let mut state = State::default();
    for run in ["1", "2", "3"] {
        let (_, next) = decide(Some(&state), &run_log(run), 1_000, LIMIT);
        state = next;
    }
    let (verdict, _) = decide(Some(&state), &run_log("4"), 1_000 + RESET_SECS + 1, LIMIT);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn the_limit_env_is_read_defensively() {
    assert_eq!(limit_from(None), DEFAULT_LIMIT);
    assert_eq!(limit_from(Some("5")), 5);
    assert_eq!(limit_from(Some(" 0 ")), 0);
    assert_eq!(limit_from(Some("nonsense")), DEFAULT_LIMIT);
    assert_eq!(limit_from(Some("-3")), DEFAULT_LIMIT);
}

#[test]
fn the_allow_token_must_be_a_whole_word() {
    assert!(opts_out("CLUD_ALLOW_CI_LOG=1 gh run view 42 --log"));
    assert!(!opts_out("MYCLUD_ALLOW_CI_LOG=1 gh run view 42 --log"));
    assert!(!opts_out("gh run view 42 --log"));
}

#[test]
fn the_tracked_run_set_is_bounded() {
    let mut state = State::default();
    let mut now = 1_000;
    for run in 0..60 {
        now += 1;
        let (_, next) = decide(Some(&state), &run_log(&run.to_string()), now, 1);
        state = next;
    }
    assert!(state.runs.len() <= MAX_RUNS, "{}", state.runs.len());
}

#[test]
fn a_refusal_survives_a_serialized_round_trip() {
    let (_, state) = decide(None, &run_log("1"), 1_000, LIMIT);
    let (_, state) = decide(Some(&state), &run_log("2"), 1_001, LIMIT);
    assert_eq!(state.runs.len(), 2);
    let text = serde_json::to_string(&state).unwrap();
    let back: State = serde_json::from_str(&text).unwrap();
    assert_eq!(back, state);
    let (verdict, _) = decide(Some(&back), &run_log("3"), 1_002, LIMIT);
    assert_eq!(verdict, Verdict::Deny { count: LIMIT + 1 });
}

#[test]
fn only_shell_tools_are_inspected() {
    assert!(shell_tool("Bash"));
    assert!(shell_tool("Shell"));
    assert!(!shell_tool("Read"));
    assert!(!shell_tool("mcp__clud__board_read"));
}

#[test]
fn the_denial_names_the_exact_bounded_command() {
    let log = detect("gh run view 41639349424 -R talos-hq/clud --log-failed").unwrap();
    let message = deny_message(&log, 3, LIMIT);
    assert!(message.contains(
        "\"$CLUD_EXE\" tool run github/ci_failure_report.py -- --repo talos-hq/clud \
         --run 41639349424 --job <job-id>"
    ));
    assert!(message.contains("gh run view 41639349424 -R talos-hq/clud --json jobs"));
    assert!(message.contains(LIMIT_ENV));
}

#[test]
fn the_denial_uses_the_job_id_it_was_given() {
    let log = detect("gh run view 42 --job 125931081356 -R a/b --log-failed").unwrap();
    let message = deny_message(&log, 3, LIMIT);
    assert!(message.contains("--run 42 --job 125931081356"));
    assert!(!message.contains("--json jobs"));
}

#[test]
fn the_note_stays_short_and_points_at_the_tool() {
    let message = nudge_message(&run_log("9"), 2, LIMIT);
    assert!(message.contains("github/ci_failure_report.py"));
    assert!(message.len() < 800, "{}", message.len());
}

fn temp_state_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("clud-cilog-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_session_ledger_persists_across_calls() {
    let dir = temp_state_dir("ledger");
    assert_eq!(
        check_at(&dir, "s1", &run_log("1"), 10, LIMIT),
        Verdict::Allow
    );
    assert_eq!(
        check_at(&dir, "s1", &run_log("2"), 11, LIMIT),
        Verdict::Nudge { count: 2 }
    );
    assert_eq!(
        check_at(&dir, "s1", &run_log("3"), 12, LIMIT),
        Verdict::Deny { count: 3 }
    );
    assert_eq!(
        check_at(&dir, "s1", &run_log("3"), 13, LIMIT),
        Verdict::Allow
    );
    // a different session starts clean
    assert_eq!(
        check_at(&dir, "s2", &run_log("3"), 14, LIMIT),
        Verdict::Allow
    );
    let _ = std::fs::remove_dir_all(dir);
}
