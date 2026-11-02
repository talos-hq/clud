//! Decision table and command parsing for the per-file shell read-hop guard
//! (#2233).

use super::*;

const LIMIT: u32 = DEFAULT_LIMIT;

/// Feed one path through the table `n` times, `gap` seconds apart, and return
/// the 1-based indices of the denied hops.
fn denied(n: u32, gap: u64, limit: u32) -> Vec<u32> {
    let path = "/w/a.rs".to_string();
    let mut state: Option<State> = None;
    let mut out = Vec::new();
    for index in 0..n {
        let now = 1_000 + gap * u64::from(index);
        let (verdict, next) = decide(state.as_ref(), std::slice::from_ref(&path), now, limit);
        if verdict != Verdict::Allow {
            out.push(index + 1);
        }
        state = Some(next);
    }
    out
}

#[test]
fn hops_up_to_the_limit_are_allowed_and_n_plus_one_is_denied() {
    assert!(denied(LIMIT, 0, LIMIT).is_empty());
    assert_eq!(denied(LIMIT + 1, 0, LIMIT), vec![LIMIT + 1]);
}

#[test]
fn the_measured_hunt_sizes_are_denied_early() {
    // 562 file-sessions/day hopped one file 8+ times; the first denied hop is
    // the 7th, which is where a hunt is provably unmapped.
    assert_eq!(LIMIT + 1, 7);
    assert_eq!(denied(12, 0, LIMIT), vec![7, 8, 9, 10, 11, 12]);
}

#[test]
fn a_different_range_of_the_same_file_is_the_same_hop_count() {
    let mut state: Option<State> = None;
    let mut denied_at = None;
    for index in 0..LIMIT + 1 {
        // Each call names the same file in a differently spelled command.
        let command = format!("sed -n {},{index}p crates/a.rs", index * 10);
        let targets: Vec<String> = hop_targets(&command)
            .into_iter()
            .map(|_| "/w/crates/a.rs".to_string())
            .collect();
        let (verdict, next) = decide(state.as_ref(), &targets, 1_000, LIMIT);
        if verdict != Verdict::Allow {
            denied_at = Some(index + 1);
        }
        state = Some(next);
    }
    assert_eq!(denied_at, Some(LIMIT + 1));
}

#[test]
fn an_idle_gap_longer_than_the_window_starts_a_new_count() {
    assert!(denied(LIMIT * 3, RESET_SECS + 1, LIMIT).is_empty());
}

#[test]
fn a_gap_within_the_window_keeps_counting() {
    assert_eq!(denied(LIMIT + 1, RESET_SECS, LIMIT), vec![LIMIT + 1]);
}

#[test]
fn a_zero_limit_disables_the_guard() {
    assert!(denied(50, 0, 0).is_empty());
}

#[test]
fn two_files_get_independent_counts() {
    let a = "/w/a.rs".to_string();
    let b = "/w/b.py".to_string();
    let mut state: Option<State> = None;
    for _ in 0..LIMIT {
        let (verdict, next) = decide(state.as_ref(), std::slice::from_ref(&a), 1_000, LIMIT);
        assert_eq!(verdict, Verdict::Allow);
        state = Some(next);
    }
    let (verdict, _) = decide(state.as_ref(), std::slice::from_ref(&b), 1_000, LIMIT);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn the_state_map_is_bounded() {
    let state = State::default();
    let targets: Vec<String> = (0..MAX_TRACKED * 3)
        .map(|index| format!("/w/f{index}.rs"))
        .collect();
    let mut current = Some(state);
    for (index, target) in targets.iter().enumerate() {
        let (_, next) = decide(
            current.as_ref(),
            std::slice::from_ref(target),
            1_000 + index as u64,
            LIMIT,
        );
        current = Some(next);
    }
    let state = current.unwrap_or_default();
    assert_eq!(state.files.len(), MAX_TRACKED);
    // The newest entries survive; the oldest were dropped.
    assert!(state.files.contains_key(&targets[targets.len() - 1]));
    assert!(!state.files.contains_key(&targets[0]));
}

#[test]
fn only_read_verbs_contribute_targets() {
    assert_eq!(
        hop_targets("sed -n 140,413p crates/a.rs"),
        vec!["crates/a.rs"]
    );
    assert_eq!(
        hop_targets("cat src/main.py | head -40"),
        vec!["src/main.py"]
    );
    assert_eq!(hop_targets("nl -ba README.md"), vec!["README.md"]);
    assert!(hop_targets("git add crates/a.rs").is_empty());
    assert!(hop_targets("cargo build -p clud").is_empty());
    assert!(hop_targets("grep -rn 'x' crates/a.rs").is_empty());
    assert!(hop_targets("gh pr view 1 -R o/r").is_empty());
}

#[test]
fn a_hunt_counts_every_read_file_in_the_command_once() {
    assert_eq!(
        hop_targets("sed -n 28,80p a.nix; sed -n 2780,2815p b.nix; cat c.py"),
        vec!["a.nix", "b.nix", "c.py"]
    );
    // The same file twice in one command is one hop, not two.
    assert_eq!(
        hop_targets("sed -n 1,20p a.rs; sed -n 40,60p a.rs"),
        vec!["a.rs"]
    );
}

#[test]
fn search_and_redirect_tokens_are_not_targets() {
    assert!(hop_targets("cat a.rs > /tmp/out.rs").contains(&"a.rs".to_string()));
    assert!(hop_targets("sed -n 1,5p a.rs 2>/dev/null").contains(&"a.rs".to_string()));
    assert!(hop_targets("cat *.rs").is_empty());
    assert!(hop_targets("cat -n a.rs").contains(&"a.rs".to_string()));
}

#[test]
fn in_place_edits_are_writes_not_reads() {
    // `sed -i` replaces a file in place; it puts no file content in context, so
    // counting it as a hop would deny the seventh *edit* of a fix loop with a
    // message about outlining the file.
    assert!(hop_targets("sed -i 's/foo/bar/' crates/a.rs").is_empty());
    assert!(hop_targets("sed -i.bak -e 's/foo/bar/' crates/a.rs").is_empty());
    assert!(hop_targets("sed -ibak 's/foo/bar/' crates/a.rs").is_empty());
    assert!(hop_targets("sed -ni 's/foo/bar/p' crates/a.rs").is_empty());
    assert!(hop_targets("sed --in-place=.orig 's/foo/bar/' crates/a.rs").is_empty());
    // …while a plain `sed -n` slice of the same file is still a read.
    assert_eq!(
        hop_targets("sed -n 's/foo/bar/' crates/a.rs"),
        vec!["crates/a.rs"]
    );
}

#[test]
fn heredoc_and_redirect_writes_are_not_reads() {
    // `cat > file <<EOF` writes; the file is the *destination*, not a source.
    assert!(hop_targets("cat > notes.md <<'EOF'").is_empty());
    assert!(hop_targets("cat >> src/a.py <<EOF").is_empty());
    assert!(hop_targets("cat <<'EOF' > docs/a.md").is_empty());
    // A read that also redirects still reads its inputs.
    assert_eq!(
        hop_targets("cat a.rs > /tmp/out.rs"),
        vec!["a.rs".to_string()]
    );
    assert_eq!(
        hop_targets("awk '{print $1}' a.py > out.py"),
        vec!["a.py".to_string()]
    );
}

#[test]
fn counting_lines_is_not_a_content_read() {
    // `wc -l` emits a count per file, not file content; outlining would not
    // answer it, so it must not consume hop budget.
    assert!(hop_targets("wc -l crates/a.rs").is_empty());
    assert!(hop_targets("wc -l src/*.rs | tail -1").is_empty());
}

#[test]
fn a_tilde_path_resolves_against_home() {
    assert_eq!(
        expand_tilde("~/dev/nixos/system/a.nix", Some(Path::new("/home/u"))),
        Some(PathBuf::from("/home/u/dev/nixos/system/a.nix"))
    );
    assert_eq!(expand_tilde("dev/a.nix", Some(Path::new("/home/u"))), None);
    assert_eq!(expand_tilde("~/a.nix", None), None);
}

#[test]
fn equivalent_spellings_share_one_count() {
    let cwd = Path::new("/work");
    assert_eq!(
        resolve_target("a.rs", cwd, None),
        resolve_target("./a.rs", cwd, None)
    );
    assert_eq!(
        resolve_target("a.rs", cwd, None),
        Some(PathBuf::from("/work/a.rs"))
    );
    assert_eq!(
        resolve_target("/work/./a.rs", cwd, None),
        Some(PathBuf::from("/work/a.rs"))
    );
}

#[test]
fn non_source_extensions_are_ignored() {
    assert!(hop_targets("cat build.log").is_empty());
    assert!(hop_targets("tail -5 /var/log/x.txt").is_empty());
    assert!(hop_targets("head -20 Cargo.lock").is_empty());
}

#[test]
fn the_allow_token_opts_out_only_as_a_whole_word() {
    assert!(opts_out("CLUD_ALLOW_FILE_HOPS=1 sed -n 1,5p a.rs"));
    assert!(!opts_out("XCLUD_ALLOW_FILE_HOPS=1x sed -n 1,5p a.rs"));
    assert!(!opts_out("sed -n 1,5p a.rs"));
}

#[test]
fn the_limit_env_is_read_defensively() {
    assert_eq!(limit_from(None), DEFAULT_LIMIT);
    assert_eq!(limit_from(Some("3")), 3);
    assert_eq!(limit_from(Some(" 0 ")), 0);
    assert_eq!(limit_from(Some("nope")), DEFAULT_LIMIT);
}

#[test]
fn only_shell_tools_are_inspected() {
    assert!(shell_tool("Bash"));
    assert!(shell_tool("shell"));
    assert!(!shell_tool("Read"));
    assert!(!shell_tool("Edit"));
}

#[test]
fn the_denial_names_the_file_the_limit_and_the_outline() {
    let message = deny_message("/w/crates/a.rs", 7, LIMIT);
    assert!(message.contains("/w/crates/a.rs"));
    assert!(message.contains("7 times"));
    assert!(message.contains("code/outline.py"));
    assert!(message.contains(ALLOW_TOKEN));
}
