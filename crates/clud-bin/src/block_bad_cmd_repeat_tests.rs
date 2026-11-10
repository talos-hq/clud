//! Decision table for the repeated-identical-call guard (#1674).

use super::*;
use serde_json::json;

const N: u32 = DEFAULT_LIMIT;

/// Feed `keys` through the decision table, `gap` seconds apart, and return
/// the 1-based indices of the denied calls.
fn denied(keys: &[&str], gap: u64, limit: u32) -> Vec<usize> {
    let mut state: Option<Streak> = None;
    let mut out = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        let (verdict, next) = decide(state.as_ref(), key, 1_000 + gap * index as u64, limit);
        if verdict != Verdict::Allow {
            out.push(index + 1);
        }
        state = Some(next);
    }
    out
}

#[test]
fn incident_294_identical_calls_are_denied_from_call_n_plus_one() {
    let keys = ["a"; 294];
    let expected: Vec<usize> = (N as usize + 1..=294).collect();
    assert_eq!(denied(&keys, 0, N), expected);
}

#[test]
fn incident_371_identical_calls_are_denied_from_call_n_plus_one() {
    let keys = ["a"; 371];
    assert_eq!(denied(&keys, 0, N).first(), Some(&(N as usize + 1)));
    assert_eq!(denied(&keys, 0, N).len(), 371 - N as usize);
}

#[test]
fn exactly_n_identical_calls_are_all_allowed() {
    assert!(denied(&["a"; N as usize], 0, N).is_empty());
}

#[test]
fn polling_loop_below_the_limit_is_never_denied() {
    // The longest sleep-paced poll seen in local transcripts was 138 calls.
    assert!(denied(&["poll"; 138], 2, N).is_empty());
}

#[test]
fn a_different_call_in_between_resets_the_streak() {
    let mut keys = vec!["a"; N as usize];
    keys.push("b");
    keys.extend(["a"; N as usize]);
    assert!(denied(&keys, 0, N).is_empty());
}

#[test]
fn interleaved_calls_never_build_a_streak() {
    let keys: Vec<&str> = (0..1_000)
        .map(|i| if i % 2 == 0 { "a" } else { "b" })
        .collect();
    assert!(denied(&keys, 0, N).is_empty());
}

#[test]
fn an_idle_gap_longer_than_the_reset_window_starts_a_new_streak() {
    // A `/loop` tick every 10 minutes repeats one call indefinitely.
    assert!(denied(&["tick"; 1_000], IDLE_RESET_SECS + 1, N).is_empty());
    // At the window it still counts.
    assert_eq!(
        denied(&["a"; N as usize + 1], IDLE_RESET_SECS, N),
        vec![N as usize + 1]
    );
}

#[test]
fn a_denied_call_keeps_counting_so_a_blind_retry_stays_denied() {
    assert_eq!(denied(&["a"; 5], 0, 3), vec![4, 5]);
}

#[test]
fn limit_zero_disables_the_guard() {
    assert!(denied(&["a"; 1_000], 0, 0).is_empty());
}

#[test]
fn call_key_is_tool_scoped_and_ignores_object_key_order() {
    let one = json!({"command": "gh pr checks", "timeout": 5});
    let two: Value = serde_json::from_str(r#"{"timeout":5,"command":"gh pr checks"}"#).unwrap();
    assert_eq!(call_key("Bash", Some(&one)), call_key("Bash", Some(&two)));
    assert_ne!(
        call_key("Bash", Some(&one)),
        call_key("PowerShell", Some(&one))
    );
    assert_ne!(
        call_key("Bash", Some(&one)),
        call_key("Bash", Some(&json!({"command": "gh pr checks 1"})))
    );
    assert_eq!(call_key("Bash", Some(&one)).len(), 16);
}

#[test]
fn limit_resolution_env_then_settings_then_default() {
    let settings = json!({"hooks": {"repeat_call_limit": 40}});
    assert_eq!(limit_from(Some("7"), Some(&settings)), 7);
    assert_eq!(limit_from(Some("0"), Some(&settings)), 0);
    assert_eq!(limit_from(None, Some(&settings)), 40);
    assert_eq!(limit_from(Some("junk"), None), DEFAULT_LIMIT);
    let bad = json!({"hooks": {"repeat_call_limit": "x"}});
    assert_eq!(limit_from(None, Some(&bad)), DEFAULT_LIMIT);
    assert_eq!(limit_from(None, None), DEFAULT_LIMIT);
}

#[test]
fn opt_out_token_is_whole_word() {
    assert!(opts_out("CLUD_ALLOW_REPEAT=1 gh pr checks"));
    assert!(opts_out("export CLUD_ALLOW_REPEAT=1; ls"));
    assert!(!opts_out("XCLUD_ALLOW_REPEAT=1 ls"));
    assert!(!opts_out("CLUD_ALLOW_REPEAT=10 ls"));
    assert!(!opts_out("ls"));
}

#[test]
fn check_at_persists_only_hashes_and_counts_per_session() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..3 {
        assert_eq!(
            check_at(
                tmp.path(),
                "sess-1",
                "k",
                Past {
                    is_query: false,
                    poll_key: "k",
                    poll_limit: POLL_DENY_AT,
                },
                10 + i,
                3,
            )
            .0,
            Verdict::Allow
        );
    }
    assert_eq!(
        check_at(
            tmp.path(),
            "sess-1",
            "k",
            Past {
                is_query: false,
                poll_key: "k",
                poll_limit: POLL_DENY_AT,
            },
            14,
            3,
        )
        .0,
        Verdict::Deny { count: 4 }
    );
    // Another session has its own streak.
    assert_eq!(
        check_at(
            tmp.path(),
            "sess-2",
            "k",
            Past {
                is_query: false,
                poll_key: "k",
                poll_limit: POLL_DENY_AT,
            },
            14,
            3,
        )
        .0,
        Verdict::Allow
    );
    let dir = tmp.path().join(DIR_NAME);
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
    assert_eq!(files.len(), 2);
    for file in files {
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(!text.contains("sess-"), "session id must not be stored");
        let streak: Streak = serde_json::from_str(&text).unwrap();
        assert_eq!(streak.key, "k");
    }
}

#[test]
fn state_failures_fail_open() {
    let tmp = tempfile::tempdir().unwrap();
    // A file where the state directory should be: writes fail, calls allow.
    let blocked = tmp.path().join("state");
    std::fs::write(&blocked, b"not a dir").unwrap();
    for i in 0..10 {
        assert_eq!(
            check_at(
                &blocked,
                "s",
                "k",
                Past {
                    is_query: false,
                    poll_key: "k",
                    poll_limit: POLL_DENY_AT,
                },
                i,
                2,
            )
            .0,
            Verdict::Allow
        );
    }
    // A corrupt streak file is treated as no streak.
    let ok = tmp.path().join("ok");
    let path = session_file(&ok, "s");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{garbage").unwrap();
    assert_eq!(
        check_at(
            &ok,
            "s",
            "k",
            Past {
                is_query: false,
                poll_key: "k",
                poll_limit: POLL_DENY_AT,
            },
            1,
            1,
        )
        .0,
        Verdict::Allow
    );
    // No session id: allow.
    assert_eq!(
        check_at(
            &ok,
            "",
            "k",
            Past {
                is_query: false,
                poll_key: "k",
                poll_limit: POLL_DENY_AT,
            },
            1,
            0,
        )
        .0,
        Verdict::Allow
    );
}

#[test]
fn deny_message_tells_the_model_to_stop_and_names_the_override() {
    let message = deny_message("Bash", 201, 200);
    assert!(message.contains("Stop repeating"));
    assert!(message.contains("fresh"));
    assert!(message.contains(ALLOW_REPEAT_TOKEN));
    // #1894: the guard must not teach a sleep-paced poll.
    assert!(!message.contains("sleep"), "{message}");
    assert!(message.contains("run_in_background"), "{message}");
    assert!(message.contains("pr_merge_watch"), "{message}");
}

// ---- #2631: the patient poll the burst guard deliberately ignores ---------

#[test]
fn poll_count_survives_gaps_the_burst_guard_resets() {
    // One identical query every 10 minutes for a day: `decide` starts a new
    // streak at every call, while the ledger keeps counting.
    let mut previous: Option<Streak> = None;
    for index in 0..12_u64 {
        let now = 1_000 + 600 * index;
        let (verdict, mut streak) = decide(previous.as_ref(), "k", now, DEFAULT_LIMIT);
        assert_eq!(verdict, Verdict::Allow);
        let polls = ledger_for_count(previous.as_ref(), "k", "k");
        let (steady, steady_last) = poll_count(&polls, "k", now);
        streak.steady = steady;
        streak.steady_last = steady_last;
        streak.polls = ledger_after(&polls, "k", steady, now);
        assert_eq!(steady, u32::try_from(index).unwrap() + 1);
        previous = Some(streak);
    }
}

#[test]
fn poll_count_counts_one_query_and_expires_it() {
    let previous = Streak {
        key: "exact".to_string(),
        count: 1,
        last: 1_000,
        steady: 0,
        steady_last: 0,
        polls: vec![PollEntry {
            key: "k".to_string(),
            count: 5,
            last: 1_000,
        }],
    };
    assert_eq!(poll_count(&previous.polls, "k", 1_600).0, 6);
    // Another query has its own count — it neither resets nor inherits this one.
    assert_eq!(poll_count(&previous.polls, "other", 1_600).0, 1);
    // Outside the window the count starts over rather than carrying forward.
    assert_eq!(
        poll_count(&previous.polls, "k", 1_000 + POLL_WINDOW_SECS + 1).0,
        1
    );
}

/// A session file written before the ledger existed holds one slot; the count
/// it carries survives the upgrade rather than restarting at one.
#[test]
fn a_legacy_streak_seeds_the_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let now = 5_000_u64;
    let legacy = Streak {
        key: "exact".to_string(),
        count: 94,
        last: now - 1,
        steady: 94,
        steady_last: now - 1,
        polls: Vec::new(),
    };
    let path = session_file(tmp.path(), "s");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let (_, note) = check_at(
        tmp.path(),
        "s",
        "exact",
        Past {
            is_query: true,
            poll_key: "poll",
            poll_limit: POLL_DENY_AT,
        },
        now,
        DEFAULT_LIMIT,
    );
    assert_eq!(note, None);
    let streak: Streak = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(streak.polls.len(), 1);
    assert_eq!(streak.polls[0].key, "poll");
    assert_eq!(streak.polls[0].count, 95);
    // And the migrated count keeps climbing under a different exact call.
    let (verdict, _) = check_at(
        tmp.path(),
        "s",
        "exact-2",
        Past {
            is_query: true,
            poll_key: "poll",
            poll_limit: POLL_DENY_AT,
        },
        now + 1,
        DEFAULT_LIMIT,
    );
    assert_eq!(verdict, Verdict::Allow);
    let streak: Streak = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(streak.polls[0].count, 96);
}

/// The single-slot migration must not leak into a *fresh* poll key: a file
/// that changed (or appeared) starts a new count, or the freshness stamp in
/// the key would never re-arm and the poll guard would refuse a live read.
#[test]
fn a_fresh_poll_key_never_inherits_the_count_of_another() {
    let tmp = tempfile::tempdir().unwrap();
    let now = 5_000_u64;
    let exact = "exact";
    let mut past = Past {
        is_query: true,
        poll_key: "file:v1",
        poll_limit: POLL_DENY_AT,
    };
    // Drive the first key up to the refusal point.
    for step in 0..(POLL_DENY_AT + 1) {
        let (verdict, _) = check_at(
            tmp.path(),
            "s",
            exact,
            past,
            now + u64::from(step),
            DEFAULT_LIMIT,
        );
        if step < POLL_DENY_AT {
            assert_eq!(verdict, Verdict::Allow, "step {step}");
        }
    }
    let (verdict, _) = check_at(tmp.path(), "s", exact, past, now + 100, DEFAULT_LIMIT);
    assert!(matches!(verdict, Verdict::PollDeny { .. }), "{verdict:?}");
    // The file changed: a new key, and the same exact command is allowed again.
    past.poll_key = "file:v2";
    let (verdict, note) = check_at(tmp.path(), "s", exact, past, now + 101, DEFAULT_LIMIT);
    assert_eq!(verdict, Verdict::Allow);
    assert_eq!(note, None);
    let streak: Streak =
        serde_json::from_slice(&std::fs::read(session_file(tmp.path(), "s")).unwrap()).unwrap();
    let fresh = streak
        .polls
        .iter()
        .find(|entry| entry.key == "file:v2")
        .unwrap();
    assert_eq!(fresh.count, 1, "a new file fingerprint starts at one");
    // The old key keeps its own count, still refused.
    past.poll_key = "file:v1";
    let (verdict, _) = check_at(tmp.path(), "s", exact, past, now + 102, DEFAULT_LIMIT);
    assert!(matches!(verdict, Verdict::PollDeny { .. }), "{verdict:?}");
}

// ---- #2233: one query, many dressings -------------------------------

/// The three command shapes one session actually emitted 193 times while it
/// waited on a PR that did not exist yet: a bare chained sleep, and the same
/// sleep behind a read of a *different* background-task output file each time.
const FLAPPED: [&str; 3] = [
    "sleep 60; gh pr list --head fix/2002-false-green-matrix --state all --json number,state,url",
    "cat /home/n/.clud/tmp/claude-1000/x/tasks/b36l7m4nq.output; sleep 55; gh pr list --head fix/2002-false-green-matrix --state all --json number,state,url",
    "cat /home/n/.clud/tmp/claude-1000/x/tasks/b1yx2mbil.output; sleep 50; gh pr list --state all --json url,number,state --head fix/2002-false-green-matrix",
];

fn flapped_key(command: &str) -> String {
    poll_key("Bash", Some(&json!({"command": command})))
}

fn flapped_scan(state: &std::path::Path, command: &str, now: u64) -> (Verdict, Option<u32>) {
    let exact = call_key("Bash", Some(&json!({"command": command})));
    let poll = flapped_key(command);
    check_at(
        state,
        "s",
        &exact,
        Past {
            is_query: is_query(command),
            poll_key: &poll,
            poll_limit: POLL_DENY_AT,
        },
        now,
        DEFAULT_LIMIT,
    )
}

#[test]
fn poll_key_ignores_the_sleep_and_task_output_dressing() {
    let keys: Vec<String> = FLAPPED.iter().map(|cmd| flapped_key(cmd)).collect();
    assert_eq!(keys[0], keys[1], "{keys:?}");
    assert_eq!(keys[0], keys[2], "{keys:?}");
    // A different branch is a different query.
    let other = flapped_key("sleep 60; gh pr list --head fix/1989-racy-stat-guard --state all");
    assert_ne!(keys[0], other);
    // A read and a write are never the same key, and a command with nothing
    // but noise falls back to the exact call key.
    assert_ne!(
        flapped_key("gh pr view 1 --json state"),
        flapped_key("gh pr merge 1 --squash")
    );
    assert_eq!(
        flapped_key("sleep 60"),
        call_key("Bash", Some(&json!({"command": "sleep 60"})))
    );
    assert_eq!(poll_core("sleep 60; echo waiting; true"), "");
}

#[test]
fn an_alternating_poll_dressing_still_counts_as_one_query() {
    // 2026-11-08: the single-slot steady counter reset on every flip, so 193
    // calls of one query produced no note and no refusal. The ledger keeps a
    // count per query, so the flip no longer erases it.
    let tmp = tempfile::tempdir().unwrap();
    let mut notes = Vec::new();
    for index in 0..30_u64 {
        let (verdict, note) = flapped_scan(
            tmp.path(),
            FLAPPED[index as usize % FLAPPED.len()],
            1_000 + 300 * index,
        );
        assert_eq!(verdict, Verdict::Allow);
        if let Some(count) = note {
            notes.push(count);
        }
    }
    assert_eq!(notes, vec![POLL_NOTE_AT, POLL_NOTE_AGAIN_AT]);
}

#[test]
fn the_poll_guard_refuses_an_alternating_poll_past_its_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut refusals = 0;
    for index in 0..=u64::from(POLL_DENY_AT) + 2 {
        let (verdict, _) = flapped_scan(
            tmp.path(),
            FLAPPED[index as usize % FLAPPED.len()],
            1_000 + 300 * index,
        );
        if let Verdict::PollDeny { .. } = verdict {
            refusals += 1;
        }
    }
    assert_eq!(refusals, 3);
}

#[test]
fn the_ledger_is_bounded_and_forgets_expired_queries() {
    let tmp = tempfile::tempdir().unwrap();
    for index in 0..(MAX_POLL_KEYS as u64 + 6) {
        let key = format!("q{index}");
        check_at(
            tmp.path(),
            "s",
            &key,
            Past {
                is_query: true,
                poll_key: &key,
                poll_limit: POLL_DENY_AT,
            },
            1_000 + index,
            DEFAULT_LIMIT,
        );
    }
    let dir = tmp.path().join(DIR_NAME);
    let path = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let streak: Streak = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(streak.polls.len(), MAX_POLL_KEYS);
    // A query older than the window starts over rather than carrying a count.
    let stale = Streak {
        polls: vec![PollEntry {
            key: "q".to_string(),
            count: 40,
            last: 1_000,
        }],
        ..streak
    };
    assert_eq!(
        poll_count(&stale.polls, "q", 1_000 + POLL_WINDOW_SECS + 1).0,
        1
    );
}

#[test]
fn the_note_fires_once_at_six_and_once_at_twenty_four() {
    let tmp = tempfile::tempdir().unwrap();
    let mut notes = Vec::new();
    for index in 0..30_u64 {
        let (verdict, note) = check_at(
            tmp.path(),
            "s",
            "k",
            Past {
                is_query: true,
                poll_key: "k",
                poll_limit: POLL_DENY_AT,
            },
            1_000 + 300 * index,
            200,
        );
        assert_eq!(verdict, Verdict::Allow);
        if let Some(count) = note {
            notes.push(count);
        }
    }
    assert_eq!(notes, vec![POLL_NOTE_AT, POLL_NOTE_AGAIN_AT]);
}

#[test]
fn a_non_query_repeat_never_produces_the_poll_note() {
    let tmp = tempfile::tempdir().unwrap();
    for index in 0..30_u64 {
        let (_, note) = check_at(
            tmp.path(),
            "s",
            "k",
            Past {
                is_query: false,
                poll_key: "k",
                poll_limit: POLL_DENY_AT,
            },
            1_000 + 300 * index,
            200,
        );
        assert_eq!(note, None);
    }
}

#[test]
fn is_query_recognises_read_only_state_queries_only() {
    for command in [
        "gh issue view 42 --repo o/r --json comments",
        "for n in 1 2; do gh issue view $n --repo o/r; done",
        "cd ~/dev/x && gh pr checks 12",
        "git ls-remote origin",
        "curl -fsS https://example.test/health",
        "systemctl is-active twp-catchup",
        "ssh root@host 'systemctl is-active x'",
        "docker ps -q",
    ] {
        assert!(is_query(command), "{command}");
    }
    for command in [
        "soldr cargo test -p clud",
        "gh pr create --fill",
        "gh issue edit 42 --add-label bug",
        "gh api -X POST repos/o/r/issues",
        "curl -X POST -d @body.json https://example.test/hook",
        "git commit -m wip",
        "",
    ] {
        assert!(!is_query(command), "{command}");
    }
}

#[test]
fn poll_message_names_the_blocking_waiter_and_no_sleep_loop() {
    let message = poll_message(POLL_NOTE_AT);
    assert!(message.starts_with("[clud poll-note]"));
    assert!(message.contains("wait/until.py"));
    assert!(message.contains("6 times"));
    assert!(!message.contains("sleep "), "{message}");
}

// ---- #2631 follow-up: the two notes are advisory, the third step stops ------

/// A patient poll every five minutes never trips the burst guard, so the poll
/// guard is the only thing that can end it. This is the measured shape from
/// 2026-11-05: one session, 127 identical `gh issue view` polls in 24 h.
#[test]
fn the_poll_guard_refuses_the_identical_query_past_its_own_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut first_denied = None;
    let mut denials = 0;
    for index in 0..=POLL_DENY_AT as u64 + 4 {
        let (verdict, note) = check_at(
            tmp.path(),
            "s",
            "k",
            Past {
                is_query: true,
                poll_key: "k",
                poll_limit: POLL_DENY_AT,
            },
            1_000 + 300 * index,
            DEFAULT_LIMIT,
        );
        if let Verdict::PollDeny { count } = verdict {
            denials += 1;
            first_denied.get_or_insert(count);
            assert_eq!(note, None, "a refusal carries no advisory note");
        } else {
            assert_eq!(first_denied, None, "the refusal is sticky, not one-shot");
        }
    }
    assert_eq!(first_denied, Some(POLL_DENY_AT + 1));
    assert_eq!(denials, 5);
}

/// The poll guard is read-only by construction: a repeated build, write or
/// test command is the burst guard's business and is never refused here.
#[test]
fn the_poll_guard_never_refuses_a_command_that_cannot_read_state() {
    let tmp = tempfile::tempdir().unwrap();
    for index in 0..(POLL_DENY_AT as u64 * 3) {
        let (verdict, note) = check_at(
            tmp.path(),
            "s",
            "k",
            Past {
                is_query: false,
                poll_key: "k",
                poll_limit: POLL_DENY_AT,
            },
            1_000 + 300 * index,
            10_000,
        );
        assert_eq!(verdict, Verdict::Allow);
        assert_eq!(note, None);
    }
}

#[test]
fn poll_limit_zero_disables_only_the_poll_guard() {
    let tmp = tempfile::tempdir().unwrap();
    for index in 0..(POLL_DENY_AT as u64 * 2) {
        let (verdict, _) = check_at(
            tmp.path(),
            "s",
            "k",
            Past {
                is_query: true,
                poll_key: "k",
                poll_limit: 0,
            },
            1_000 + 300 * index,
            DEFAULT_LIMIT,
        );
        assert_eq!(verdict, Verdict::Allow);
    }
    // The burst guard still denies: the same call, back to back, one second
    // apart, starting from the last state the poll loop left behind.
    let base = 1_000 + 300 * u64::from(POLL_DENY_AT * 2 - 1);
    for step in 1..=u64::from(DEFAULT_LIMIT) + 1 {
        let (verdict, _) = check_at(
            tmp.path(),
            "s",
            "k",
            Past {
                is_query: true,
                poll_key: "k",
                poll_limit: 0,
            },
            base + step,
            DEFAULT_LIMIT,
        );
        if let Verdict::Deny { .. } = verdict {
            return;
        }
    }
    panic!("the burst guard stopped denying when the poll limit was disabled");
}

#[test]
fn poll_limit_resolution_env_then_settings_then_default() {
    let settings = json!({"hooks": {"poll_call_limit": 40}});
    assert_eq!(poll_limit_from(Some("7"), Some(&settings)), 7);
    assert_eq!(poll_limit_from(Some("0"), Some(&settings)), 0);
    assert_eq!(poll_limit_from(None, Some(&settings)), 40);
    assert_eq!(poll_limit_from(Some("junk"), None), POLL_DENY_AT);
    let bad = json!({"hooks": {"poll_call_limit": "x"}});
    assert_eq!(poll_limit_from(None, Some(&bad)), POLL_DENY_AT);
    assert_eq!(poll_limit_from(None, None), POLL_DENY_AT);
}

#[test]
fn poll_deny_message_names_the_waiter_and_the_deliberate_override() {
    let message = poll_deny_message(POLL_DENY_AT + 1, POLL_DENY_AT);
    assert!(message.starts_with("[clud poll-guard]"));
    assert!(message.contains("wait/until.py"));
    assert!(message.contains("--interval"));
    assert!(message.contains("CLUD_ALLOW_REPEAT=1"));
    assert!(message.contains("97 times"));
    assert!(!message.contains("sleep "), "{message}");
}

/// The 2026-11-09/10 measurement: one session polled `uv run --project ci
/// errors` **783 times** under 80 presentations (`| grep -v "ERR  429" |
/// tail -10`, `timeout 170`, `cut -c1-160`, a wrapping `cd`). 626 MB of that
/// day's 1.7 GB exposure, and no two calls shared an exact key.
const CI_POLL_DRESSINGS: &[&str] = &[
    "cd /home/niteris/dev/twp2 && uv run --project ci errors --since 15m 2>&1 | grep -v \"ERR  429\" | tail -10",
    "uv run --project ci errors --since 15m 2>&1 | grep -v \"ERR  429\" | tail -10",
    "uv run --project ci errors --since 15m 2>&1 | grep -v \"ERR  429\" | tail -8",
    "timeout 170 uv run --project ci errors --since 15m 2>&1 | grep -v \"ERR  429\" | sed -n '4,20p'",
    "(cd /home/niteris/dev/twp2 && uv run --project ci errors --since 15m 2>&1 | cut -c1-160 | tail -n 13)",
];

#[test]
fn a_project_runner_read_is_a_query() {
    // `uv run <verb>` executes anything, so only a read verb is a query.
    for command in CI_POLL_DRESSINGS {
        assert!(is_query(command), "{command}");
    }
    assert!(is_query("uv run --project ci errors --since 15m"));
    assert!(is_query("timeout 300 systemctl is-active obs-rust"));
    assert!(!is_query("uv run --project ci test"));
    assert!(!is_query("uv run --project ci deploy"));
    assert!(!is_query("uv run pytest -q"));
}

#[test]
fn poll_ledger_collapses_a_pipeline_dressing_into_one_query() {
    let keys: Vec<String> = CI_POLL_DRESSINGS.iter().map(|c| flapped_key(c)).collect();
    for key in &keys[1..] {
        assert_eq!(&keys[0], key, "{keys:?}");
    }
    // A different time window is a different query.
    assert_ne!(
        keys[0],
        flapped_key("uv run --project ci errors --since 1h | tail -10")
    );
    // A lone `grep -r` IS the query: presentation stripping must not merge two
    // different searches into one count.
    assert_ne!(
        flapped_key("grep -rn render-logs src/"),
        flapped_key("grep -rn presign-url src/")
    );
}

#[test]
fn the_measured_ci_poll_is_refused_past_its_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut refusals: Vec<usize> = Vec::new();
    for index in 0..=u64::from(POLL_DENY_AT) + 3 {
        let command = CI_POLL_DRESSINGS[index as usize % CI_POLL_DRESSINGS.len()];
        let (verdict, _) = flapped_scan(tmp.path(), command, 1_000 + 300 * index);
        if let Verdict::PollDeny { count } = verdict {
            assert!(count > POLL_DENY_AT, "call {}: {count}", index + 1);
            refusals.push(index as usize + 1);
        }
    }
    // Every call past the limit is refused, and the refusals start there: the
    // 783 measured calls would have stopped at 97.
    assert_eq!(
        refusals,
        (POLL_DENY_AT as usize + 1..=POLL_DENY_AT as usize + 4).collect::<Vec<usize>>(),
        "{refusals:?}"
    );
}

// ---- #2233: a local file is state too -------------------------------
//
// 2026-11-10 probe: `ls <pool>/NEXT_MESSAGE.md 2>/dev/null; grep
// manager_session_id <pool>/state.json` ran 169 times in one session with 163
// byte-identical answers, and 156 sessions re-read one `.worker-pool/state.json`
// through a second `head` slice. Neither counted as a query.

fn file_poll_key(command: &str) -> String {
    poll_key("Bash", Some(&json!({ "command": command })))
}

#[test]
fn one_local_file_polled_through_its_dressings_shares_one_key() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::fs::write(&state, r#"{"manager_session_id":"abc","workers":2}"#).unwrap();
    let path = state.display();
    let keys: Vec<String> = [
        format!("cat {path} 2>&1 | head -60"),
        format!("cat {path} 2>&1 | head -c 4000"),
        format!("cat {path} 2>&1 | head -80"),
        format!("ls {path} 2>/dev/null; grep manager_session_id {path}"),
        format!("jq -c '{{m:.manager_session_id}}' {path}"),
    ]
    .iter()
    .map(|command| file_poll_key(command))
    .collect();
    for key in &keys[1..] {
        assert_eq!(&keys[0], key, "{keys:?}");
    }
    // A different file is a different poll.
    let other = dir.path().join("im-state.json");
    std::fs::write(&other, "{}").unwrap();
    assert_ne!(
        keys[0],
        file_poll_key(&format!("cat {} | head -20", other.display()))
    );
    // Every one of them is a read-only query, so the steady counter runs.
    assert!(poll_query(&format!("cat {path} 2>&1 | head -60")));
    assert!(local_file_poll(&format!("cat {path} 2>&1 | head -60")).is_some());
}

#[test]
fn a_changed_file_starts_a_fresh_poll_count() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::fs::write(&state, "{\"tick\":1}").unwrap();
    let command = format!("cat {}", state.display());
    let before = file_poll_key(&command);
    std::fs::write(&state, "{\"tick\":2,\"workers\":9}").unwrap();
    let after = file_poll_key(&command);
    assert_ne!(before, after, "an edited file must re-arm the poll count");
    // A file that appears after being polled for also re-arms.
    let absent = dir.path().join("NEXT_MESSAGE.md");
    let waiting = format!("ls {}", absent.display());
    let missing = file_poll_key(&waiting);
    std::fs::write(&absent, "next").unwrap();
    assert_ne!(missing, file_poll_key(&waiting));
}

#[test]
fn a_writer_is_never_a_file_poll() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("state.json");
    let target = dir.path().join("copy.json");
    std::fs::write(&source, "{}").unwrap();
    for command in [
        format!("cat {} > {}", source.display(), target.display()),
        format!("sed -i s/a/b/ {}", source.display()),
        format!("sort -o {} {}", target.display(), source.display()),
    ] {
        assert!(local_file_poll(&command).is_none(), "{command}");
        assert!(!is_query(&command), "{command}");
    }
    // Throwing output away still leaves a read.
    assert!(local_file_poll(&format!("cat {} 2>/dev/null", source.display())).is_some());
    assert!(local_file_poll(&format!("cat {} 2>&1", source.display())).is_some());
}

#[test]
fn unresolvable_and_mixed_commands_are_not_file_polls() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::fs::write(&state, "{}").unwrap();
    for command in [
        // A bare relative name: the hook has no shell cwd, so it stays exact.
        "cat state.json 2>&1 | head -60".to_string(),
        // A file read next to a remote query is counted by that query, not as
        // a file poll of this path.
        format!("cat {}; gh pr view 1 --json state", state.display()),
        // Neither is a read next to a deletion.
        format!("cat {} && rm {}", state.display(), state.display()),
        // Nor a program that is not a reader.
        format!("python -c 'print(1)' {}", state.display()),
    ] {
        assert!(local_file_poll(&command).is_none(), "{command}");
    }
    // A file read alone IS a poll, even without a verb the shell guard knows.
    let poll = format!("cat {}", state.display());
    assert!(poll_query(&poll));
    assert!(local_file_poll(&poll).is_some());
    // `~` expands against HOME the way the shell would.
    let expanded = resolvable_path("~/dev/obs-rust/.worker-pool/state.json").unwrap();
    assert!(expanded.is_absolute());
    assert!(expanded.ends_with("obs-rust/.worker-pool/state.json"));
    assert!(resolvable_path("-n").is_none());
    assert!(resolvable_path("state.json").is_none());
}

#[test]
fn a_file_poll_earns_the_poll_note_and_then_the_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::fs::write(&state, r#"{"workers":0}"#).unwrap();
    let command = format!("cat {} 2>&1 | head -60", state.display());
    let exact = call_key("Bash", Some(&json!({ "command": command })));
    let poll = file_poll_key(&command);
    // The same file under a second dressing continues the same count.
    let dressed = format!("cat {} 2>&1 | head -c 3000", state.display());
    let poll_dressed = file_poll_key(&dressed);
    assert_eq!(poll, poll_dressed, "the second dressing shares the count");
    let mut notes = Vec::new();
    let mut denied = 0;
    for index in 0..(POLL_DENY_AT + 3) {
        let poll = if index % 2 == 0 {
            poll.as_str()
        } else {
            poll_dressed.as_str()
        };
        let (verdict, note) = check_at(
            dir.path(),
            "s",
            &exact,
            Past {
                is_query: true,
                poll_key: poll,
                poll_limit: POLL_DENY_AT,
            },
            1_000 + 30 * u64::from(index),
            DEFAULT_LIMIT,
        );
        if matches!(verdict, Verdict::PollDeny { .. }) {
            denied += 1;
        }
        if let Some(count) = note {
            notes.push(count);
        }
    }
    assert_eq!(notes, vec![POLL_NOTE_AT, POLL_NOTE_AGAIN_AT]);
    assert_eq!(denied, 3, "every call past the limit is refused");
}
