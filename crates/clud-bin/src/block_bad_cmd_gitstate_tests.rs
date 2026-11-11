//! Decision table and command parsing for the raw `git status` guard (#2233).

use super::*;
use tempfile::tempdir;

const LIMIT: u32 = DEFAULT_LIMIT;

/// A checkout-shaped directory: a tempdir with an empty `.git`.
fn checkout() -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".git")).expect("git dir");
    let root = dir.path().to_path_buf();
    (dir, root)
}

/// Feed one probe through the table `n` times, `gap` seconds apart, and return
/// the 1-based indices that were refused.
fn refused(n: u32, gap: u64, limit: u32) -> Vec<u32> {
    let (_dir, root) = checkout();
    let bare = Bare { root };
    let mut state: Option<State> = None;
    let mut out = Vec::new();
    for index in 0..n {
        let now = 1_000 + gap * u64::from(index);
        let (verdict, state_now) = decide(state.as_ref(), &bare, now, limit);
        if verdict != Verdict::Allow {
            out.push(index + 1);
        }
        state = Some(state_now);
    }
    out
}

#[test]
fn probes_up_to_the_limit_are_allowed_and_the_next_one_is_refused_once() {
    assert!(refused(LIMIT, 0, LIMIT).is_empty());
    let refused_once = refused(LIMIT + 4, 0, LIMIT);
    assert_eq!(refused_once, vec![LIMIT + 1], "one refusal per checkout");
}

#[test]
fn a_retry_after_the_refusal_passes() {
    let (_dir, root) = checkout();
    let bare = Bare { root };
    let mut state: Option<State> = None;
    let mut verdicts = Vec::new();
    for index in 0..(LIMIT + 3) {
        let (verdict, next) = decide(state.as_ref(), &bare, 1_000 + u64::from(index), LIMIT);
        verdicts.push(verdict != Verdict::Allow);
        state = Some(next);
    }
    assert_eq!(
        verdicts,
        vec![false, false, true, false, false],
        "the refusal is once; the retry and everything after it pass"
    );
}

#[test]
fn a_probe_after_the_reset_window_starts_a_new_count() {
    let (_dir, root) = checkout();
    let bare = Bare { root };
    let limit = 1;
    let (_, state) = decide(None, &bare, 1_000, limit);
    assert_eq!(
        state.repos.values().next().map(|probe| probe.calls),
        Some(1)
    );
    let (verdict, state) = decide(Some(&state), &bare, 1_000 + RESET_SECS + 1, limit);
    assert_eq!(verdict, Verdict::Allow);
    assert_eq!(
        state.repos.values().next().map(|probe| probe.calls),
        Some(1),
        "the count restarted"
    );
}

#[test]
fn each_checkout_is_counted_on_its_own() {
    let (_a_dir, a) = checkout();
    let (_b_dir, b) = checkout();
    let a = Bare { root: a };
    let b = Bare { root: b };
    let mut state: Option<State> = None;
    for index in 0..LIMIT {
        let (verdict, next) = decide(state.as_ref(), &a, 1_000 + u64::from(index), LIMIT);
        assert_eq!(verdict, Verdict::Allow);
        state = Some(next);
    }
    let (verdict, state_now) = decide(state.as_ref(), &a, 1_100, LIMIT);
    assert_ne!(verdict, Verdict::Allow, "the third read in one checkout");
    state = Some(state_now);
    let (verdict, _) = decide(state.as_ref(), &b, 1_200, LIMIT);
    assert_eq!(
        verdict,
        Verdict::Allow,
        "another checkout starts with a clean count"
    );
}

#[test]
fn a_zero_limit_disables_the_guard() {
    assert!(refused(10, 0, 0).is_empty());
}

#[test]
fn plain_status_forms_are_detected() {
    let (_dir, root) = checkout();
    let cwd = root.join("sub");
    std::fs::create_dir_all(&cwd).expect("subdir");
    for command in [
        "git status",
        "git status -s",
        "git status --short",
        "git status -sb",
        "git status --short --branch",
        "git status -s -b",
    ] {
        let bare = detect(command, &cwd).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(bare.root, root, "{command}");
    }
    // `-C` names the checkout instead of the cwd.
    let with_dir = format!("git -C {} status --short", root.display());
    assert_eq!(
        detect(&with_dir, Path::new("/")).map(|bare| bare.root),
        Some(root)
    );
    // `--no-pager` is not part of the question.
    assert!(detect("git --no-pager status -s", &cwd).is_some());
}

#[test]
fn chains_scripts_and_targeted_forms_pass_untouched() {
    let (_dir, root) = checkout();
    let cwd = root.clone();
    for command in [
        "git status --short && git add -A",
        "git status -s | head -20",
        "git status -s > /tmp/out.txt",
        "git fetch -q && git status -sb",
        "cd /tmp && git status",
        "GIT_DIR=/tmp/x git status",
        "git status --porcelain",
        "git status --porcelain=v2",
        "git status -s -- examples/",
        "git status -s --ignored",
        "git status -s --untracked-files=all",
        "git diff",
        "git log --oneline -3",
        "echo git status",
        "git status -s $(date)",
    ] {
        assert!(detect(command, &cwd).is_none(), "{command} must pass");
    }
}

#[test]
fn a_directory_outside_a_checkout_fails_open() {
    let dir = tempdir().expect("tempdir");
    assert!(detect("git status", dir.path()).is_none());
}

#[test]
fn the_allow_token_is_matched_as_a_whole_word() {
    assert!(opts_out("git status CLUD_ALLOW_GIT_STATE=1"));
    assert!(!opts_out("git status"));
    assert!(!opts_out("git status CLUD_ALLOW_GIT_STATE=10"));
}

#[test]
fn check_at_persists_and_refuses_once() {
    let state_dir = tempdir().expect("state");
    let (_dir, root) = checkout();
    let bare = Bare { root: root.clone() };
    for index in 0..LIMIT {
        assert_eq!(
            check_at(
                state_dir.path(),
                "session-a",
                &bare,
                1_000 + u64::from(index),
                LIMIT
            ),
            Verdict::Allow
        );
    }
    let denied = check_at(state_dir.path(), "session-a", &bare, 1_100, LIMIT);
    assert!(matches!(denied, Verdict::Deny { count, .. } if count == LIMIT + 1));
    assert_eq!(
        check_at(state_dir.path(), "session-a", &bare, 1_200, LIMIT),
        Verdict::Allow,
        "the retry passes and survives a reload"
    );
    // Another session has its own count.
    assert_eq!(
        check_at(state_dir.path(), "session-b", &bare, 1_200, LIMIT),
        Verdict::Allow
    );
}

#[test]
fn the_denial_names_the_bounded_call_and_the_override() {
    let message = deny_message(Path::new("/w/repo"), 3, LIMIT);
    assert!(message.contains("tool run git/state.py"), "{message}");
    assert!(message.contains("/w/repo"), "{message}");
    assert!(message.contains(LIMIT_ENV), "{message}");
}
