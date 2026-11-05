//! Detection, decision table and persistence for the bounded process-scan
//! guard (#2233).

use super::*;

const LIMIT: u32 = DEFAULT_LIMIT;

fn dump_of(command: &str) -> Option<Dump> {
    dumps(command).into_iter().next()
}

fn pipeline_of(command: &str) -> String {
    dump_of(command)
        .expect("command should be detected")
        .pipeline
}

/// Feed one command through the table `n` times, `gap` seconds apart.
fn walk(command: &str, n: u32, gap: u64, limit: u32) -> Vec<Verdict> {
    let dump = dump_of(command).expect("command should be detected");
    let mut state: Option<State> = None;
    let mut out = Vec::new();
    for index in 0..n {
        let now = 1_000 + gap * u64::from(index);
        let (verdict, next) = decide(state.as_ref(), &dump, now, limit);
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

// ---- detection: the measured shapes --------------------------------

#[test]
fn detects_the_measured_pgrep_argv_dump() {
    // The 25,686 B call from the probe's 24 h window.
    let command = "pgrep -af 'local.gate|local_gate' | grep -v pgrep | head";
    assert_eq!(
        dump_of(command),
        Some(Dump {
            kind: Kind::Pgrep,
            pipeline: command.to_string(),
        })
    );
}

#[test]
fn detects_the_measured_ps_argv_dump() {
    let command = "ps -eo pid,etime,args | grep -E 'local.gate|local_gate' | grep -v grep | head";
    assert_eq!(dump_of(command).map(|dump| dump.kind), Some(Kind::Ps));
}

#[test]
fn detects_ps_aux_and_ps_ef() {
    for command in [
        "ps aux | grep -i soldr",
        "ps -ef | grep cargo",
        "ps ax | grep gate",
    ] {
        assert_eq!(
            dump_of(command).map(|dump| dump.kind),
            Some(Kind::Ps),
            "{command} should be a ps dump"
        );
    }
}

#[test]
fn detects_an_argv_dump_in_a_compound_command() {
    let command = "git rev-parse HEAD; pgrep -af zccache; ls";
    assert_eq!(
        pipeline_of(command),
        "pgrep -af zccache",
        "only the dump pipeline is named"
    );
}

// ---- detection: everything that must pass untouched -----------------

#[test]
fn a_width_bounded_pipeline_is_not_counted() {
    for command in [
        "pgrep -af zccache | cut -c1-160",
        "ps -eo pid,etime,args | grep gate | cut -c1-200",
        "ps --width 140 -ef | grep crate",
        "ps aux | cut -c1-120 | grep cargo",
        "pgrep -af x | head -c 4000",
        "ps -eo pid,args | colrm 161",
    ] {
        assert!(dumps(command).is_empty(), "{command} is already bounded");
    }
}

#[test]
fn a_pid_only_pgrep_is_not_an_argv_dump() {
    for command in ["pgrep -f zourcache", "pgrep -x soldr", "pgrep -c cargo"] {
        assert!(dumps(command).is_empty(), "{command} prints pids only");
    }
}

#[test]
fn a_command_substitution_is_not_transcript_output() {
    // The shell consumes the output; it never reaches the transcript.
    assert!(dumps("kill $(pgrep -af soldr)").is_empty());
    assert!(dumps("pids=`pgrep -af soldr`; echo \"$pids\"").is_empty());
}

#[test]
fn a_redirected_dump_is_a_write_not_output() {
    assert!(dumps("pgrep -af soldr > /tmp/pids.txt").is_empty());
    assert!(dumps("ps aux | grep cargo > /tmp/ps.log").is_empty());
}

#[test]
fn a_ps_that_selects_columns_without_args_is_not_counted() {
    assert!(dumps("ps -p 1234 -o pid,etime").is_empty());
    assert!(dumps("ps -eo pid,etime").is_empty());
}

#[test]
fn an_empty_command_has_no_dump() {
    assert!(dumps("").is_empty());
    assert!(dumps("   \n  ").is_empty());
}

#[test]
fn trailing_stderr_redirect_keeps_the_pipeline_together() {
    let command = "pgrep -af gate 2>&1 | grep -v pgrep";
    assert_eq!(pipeline_of(command), command);
}

#[test]
fn quoted_separators_do_not_split_a_pipeline() {
    let command = "ps aux | grep -E 'gate;ci-lint' | head";
    assert_eq!(pipeline_of(command), command);
}

#[test]
fn a_dump_after_an_ampersand_separator_is_found() {
    assert_eq!(
        pipeline_of("sleep 1 && ps aux | grep gate"),
        "ps aux | grep gate"
    );
}

// ---- the decision table -------------------------------------------

#[test]
fn early_dumps_are_silent_and_none_are_refused() {
    let history = walk("pgrep -af gate", LIMIT, 0, LIMIT);
    assert!(refused(&history).is_empty());
}

#[test]
fn the_limit_is_refused_once_and_the_retry_runs() {
    let history = walk("pgrep -af gate", LIMIT + 5, 0, LIMIT);
    let at = usize::try_from(LIMIT).unwrap();
    assert_eq!(refused(&history), vec![at + 1]);
    assert_eq!(
        history[at],
        Verdict::Deny {
            dump: dump_of("pgrep -af gate").unwrap(),
            count: LIMIT + 1
        }
    );
    assert_eq!(history[at + 1], Verdict::Allow, "the retry always runs");
}

#[test]
fn a_different_dump_is_still_counted_and_refused_once() {
    let first = dump_of("pgrep -af gate").unwrap();
    let second = dump_of("ps aux | grep cargo").unwrap();
    let mut state: Option<State> = None;
    for index in 0..LIMIT {
        let (_, next) = decide(state.as_ref(), &first, 1_000 + u64::from(index), LIMIT);
        state = Some(next);
    }
    let (verdict, _) = decide(state.as_ref(), &second, 2_000, LIMIT);
    assert!(matches!(verdict, Verdict::Deny { .. }));
}

#[test]
fn an_hour_of_quiet_starts_a_new_count() {
    let dump = dump_of("pgrep -af gate").unwrap();
    let mut state: Option<State> = None;
    for index in 0..LIMIT + 1 {
        let (_, next) = decide(state.as_ref(), &dump, 1_000 + u64::from(index), LIMIT);
        state = Some(next);
    }
    let (verdict, _) = decide(state.as_ref(), &dump, 1_000 + RESET_SECS + 10, LIMIT);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn a_zero_limit_disables_the_guard() {
    let history = walk("pgrep -af gate", LIMIT + 5, 0, 0);
    assert!(refused(&history).is_empty());
}

#[test]
fn limit_from_reads_the_environment_and_falls_back() {
    assert_eq!(limit_from(Some("7")), 7);
    assert_eq!(limit_from(Some("0")), 0);
    assert_eq!(limit_from(Some("nonsense")), DEFAULT_LIMIT);
    assert_eq!(limit_from(None), DEFAULT_LIMIT);
}

#[test]
fn refused_signatures_are_trimmed_from_the_state() {
    let mut state = State::default();
    for index in 0..40 {
        state.refused.insert(format!("sig{index}"), 1_000 + index);
    }
    trim(&mut state, 2_000);
    assert!(state.refused.len() <= 16);
    assert!(
        state.refused.contains_key("sig39"),
        "newest refusals survive"
    );
}

// ---- the message and the escape token ------------------------------

#[test]
fn the_refusal_names_the_exact_bounded_pipeline() {
    let dump = dump_of("pgrep -af 'local.gate|local_gate' | head").unwrap();
    let message = deny_message(&dump, 4, LIMIT);
    assert!(
        message.contains("pgrep -af 'local.gate|local_gate' | head | cut -c1-160"),
        "{message}"
    );
    assert!(message.contains(ALLOW_TOKEN), "{message}");
    assert!(message.contains(LIMIT_ENV), "{message}");
}

#[test]
fn the_bounded_form_keeps_every_process_and_the_pid_columns() {
    let dump = dump_of("ps -eo pid,etime,args | grep gate").unwrap();
    assert_eq!(
        dump.bounded(),
        "ps -eo pid,etime,args | grep gate | cut -c1-160"
    );
}

#[test]
fn the_override_token_is_matched_as_a_whole_word() {
    assert!(opts_out("CLUD_ALLOW_PROC_SCAN=1 pgrep -af gate"));
    assert!(opts_out("pgrep -af gate CLUD_ALLOW_PROC_SCAN=1"));
    assert!(!opts_out("MYCLUD_ALLOW_PROC_SCAN=1 pgrep -af gate"));
    assert!(!opts_out("pgrep -af gate"));
}

#[test]
fn only_shell_tools_are_considered() {
    assert!(applies_to("Bash"));
    assert!(applies_to("Shell"));
    assert!(!applies_to("Read"));
    assert!(!applies_to("mcp__clud__board_read"));
}

// ---- persistence ---------------------------------------------------

#[test]
fn state_is_per_session_and_survives_a_reload() {
    let dir = std::env::temp_dir().join(format!(
        "clud-proc-scan-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    let dump = dump_of("pgrep -af gate").unwrap();
    for index in 0..LIMIT {
        assert_eq!(
            record(&dir, "session-a", &dump, 1_000 + u64::from(index), LIMIT),
            Verdict::Allow
        );
    }
    assert!(
        matches!(
            record(&dir, "session-a", &dump, 2_000, LIMIT),
            Verdict::Deny { .. }
        ),
        "the state file survives between hook calls"
    );
    assert_eq!(
        record(&dir, "session-a", &dump, 2_001, LIMIT),
        Verdict::Allow,
        "the retry of a refused dump runs"
    );
    assert_eq!(
        record(&dir, "session-b", &dump, 2_002, LIMIT),
        Verdict::Allow,
        "another session has its own count"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_state_dir_never_blocks() {
    let dump = dump_of("pgrep -af gate").unwrap();
    let dir = Path::new("/proc/self/definitely-not-writable");
    assert_eq!(
        record(dir, "session-a", &dump, 1_000, LIMIT),
        Verdict::Allow
    );
}
