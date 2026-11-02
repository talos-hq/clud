//! Unit tests for the shell AST and evaluator (#1962).
//!
//! The tables mirror the issue: the #1961 worktree/clone variants, the
//! evaluation cases, and the structural context the scanner's rules read.

use super::*;

fn posix_env() -> BaseEnv {
    BaseEnv::new("/work").with_var("HOME", "/home/u")
}

fn posix(text: &str) -> Script {
    parse(text, Dialect::Posix, &posix_env())
}

fn pwsh(text: &str) -> Script {
    let base = BaseEnv::new(r"C:\work").with_var("USERPROFILE", r"C:\Users\u");
    parse(text, Dialect::PowerShell, &base)
}

fn cmd(text: &str) -> Script {
    parse(text, Dialect::Cmd, &BaseEnv::new(r"C:\work"))
}

/// Every invocation whose (wrapper-stripped) program is `name`.
fn calls<'a>(script: &'a Script, name: &str) -> Vec<&'a Invocation> {
    script
        .invocations
        .iter()
        .filter(|inv| inv.effective_program().as_deref() == Some(name))
        .collect()
}

fn only<'a>(script: &'a Script, name: &str) -> &'a Invocation {
    let found = calls(script, name);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one `{name}` in {:#?}",
        script.invocations
    );
    found[0]
}

fn argv(inv: &Invocation) -> Vec<String> {
    inv.effective_argv()
        .iter()
        .map(|value| value.text().to_string())
        .collect()
}

fn lit(value: &Value) -> &str {
    value
        .as_literal()
        .unwrap_or_else(|| panic!("expected a literal, got {value:?}"))
}

// ---- #1961 variants: every shell-code way of creating a worktree or clone.

#[test]
fn variant_01_plain_worktree_add() {
    let script = posix("git worktree add /x/v01 -b v01");
    let git = only(&script, "git");
    assert_eq!(argv(git), ["git", "worktree", "add", "/x/v01", "-b", "v01"]);
    assert_eq!(lit(&git.cwd), "/work");
}

#[test]
fn variant_02_dash_capital_c() {
    let git = only(&posix("git -C /repo worktree add ../v02 -b v02"), "git").clone();
    assert_eq!(
        argv(&git),
        ["git", "-C", "/repo", "worktree", "add", "../v02", "-b", "v02"]
    );
}

#[test]
fn variant_03_subshell_cd_and_pipeline() {
    let script = posix("(cd /repo && git worktree add ../v03 -b v03 2>&1 | tail -1)");
    let git = only(&script, "git");
    assert_eq!(lit(&git.cwd), "/repo");
    assert_eq!(git.origin, Origin::Subshell);
    let slot = git.pipeline.expect("git is the first pipeline stage");
    assert_eq!((slot.stage, slot.stages), (0, 2));
    let tail = only(&script, "tail");
    assert_eq!(tail.pipeline.map(|s| (s.id, s.stage)), Some((slot.id, 1)));
}

#[test]
fn variant_04_dash_lower_c_option() {
    let script = posix("(cd /repo && git -c core.quotepath=off worktree add ../v04 -b v04)");
    let git = only(&script, "git");
    assert_eq!(argv(git)[1..3], ["-c", "core.quotepath=off"]);
    assert_eq!(lit(&git.cwd), "/repo");
}

#[test]
fn variant_05_alias_keeps_paths_visible() {
    let git = only(&posix("(cd /repo && git wa ../v05 -b v05)"), "git").clone();
    assert_eq!(argv(&git), ["git", "wa", "../v05", "-b", "v05"]);
    assert_eq!(lit(&git.cwd), "/repo");
}

#[test]
fn variant_06_absolute_binary_is_still_git() {
    let script = posix("(cd /repo && /run/current-system/sw/bin/git worktree add ../v06 -b v06)");
    let git = only(&script, "git");
    assert_eq!(lit(&git.words[0]), "/run/current-system/sw/bin/git");
    assert_eq!(git.program().as_deref(), Some("git"));
}

#[test]
fn variant_07_literal_for_loop_unrolls() {
    let script =
        posix("(cd /repo && for n in 07a 07b; do git worktree add ../v$n-loop -b v$n; done)");
    let gits = calls(&script, "git");
    assert_eq!(gits.len(), 2, "{:#?}", script.invocations);
    assert_eq!(argv(gits[0])[3], "../v07a-loop");
    assert_eq!(argv(gits[1])[3], "../v07b-loop");
    assert!(gits
        .iter()
        .all(|git| git.loop_kind == Some(LoopKind::ForEach) && lit(&git.cwd) == "/repo"));
}

#[test]
fn variant_08_bash_dash_c_is_parsed_as_code() {
    let script = posix("(cd /repo && bash -c 'git worktree add ../v08 -b v08')");
    let git = only(&script, "git");
    assert_eq!(git.origin, Origin::NestedShell);
    assert_eq!(lit(&git.cwd), "/repo");
    assert_eq!(argv(git)[3], "../v08");
}

#[test]
fn variant_09_python_subprocess_is_not_shell_code() {
    let script = posix(
        r#"python -c 'import subprocess; subprocess.run(["git","-C","/repo","worktree","add","../v09"])'"#,
    );
    assert!(calls(&script, "git").is_empty());
    assert_eq!(only(&script, "python").words.len(), 3);
}

#[test]
fn variant_10_program_from_variables() {
    let git = only(
        &posix("G=git; W=worktree; $G -C /repo $W add ../v10 -b v10"),
        "git",
    )
    .clone();
    assert_eq!(
        argv(&git),
        ["git", "-C", "/repo", "worktree", "add", "../v10", "-b", "v10"]
    );
}

#[test]
fn variant_11_git_dir_option() {
    let git = only(
        &posix("git --git-dir=/repo/.git worktree add /x/v11 -b v11"),
        "git",
    )
    .clone();
    assert_eq!(argv(&git)[1], "--git-dir=/repo/.git");
}

#[test]
fn variant_13_dash_capital_c_clone() {
    let git = only(&posix("git -C /x clone -q --bare repo v13.git"), "git").clone();
    assert_eq!(
        argv(&git),
        ["git", "-C", "/x", "clone", "-q", "--bare", "repo", "v13.git"]
    );
}

#[test]
fn variant_15_copy_then_worktree() {
    let script = posix("cp -a /repo /x/v15 && git -C /x/v15 worktree add ../v15b -b v15");
    assert_eq!(argv(only(&script, "cp")), ["cp", "-a", "/repo", "/x/v15"]);
    assert_eq!(argv(only(&script, "git"))[2], "/x/v15");
}

#[test]
fn variant_16_init() {
    assert_eq!(
        argv(only(&posix("git init -q /x/v16"), "git")),
        ["git", "init", "-q", "/x/v16"]
    );
}

#[test]
fn env_wrapper_with_absolute_git_is_unwrapped() {
    let script = posix("env -i /usr/bin/git clone https://h/r /x/xx");
    let git = only(&script, "git");
    assert_eq!(git.program().as_deref(), Some("env"));
    assert_eq!(argv(git), ["/usr/bin/git", "clone", "https://h/r", "/x/xx"]);
}

#[test]
fn heredoc_body_is_data_not_commands() {
    let script = posix("cat <<EOF\ngit worktree add /not/real\nEOF");
    assert!(
        calls(&script, "git").is_empty(),
        "{:#?}",
        script.invocations
    );
    assert_eq!(calls(&script, "cat").len(), 1);
}

#[test]
fn heredoc_fed_to_a_shell_is_code() {
    let script = posix("bash <<'EOF'\nrm -rf /tmp/x\nEOF");
    let rm = only(&script, "rm");
    assert_eq!(rm.origin, Origin::NestedShell);
    assert_eq!(argv(rm), ["rm", "-rf", "/tmp/x"]);
}

// ---- Evaluation table from the issue.

#[test]
fn eval_export_and_quoted_path_with_space() {
    let script = posix(r#"export R=/repo; git -C "$R" worktree add "$R-wt/feat x" -b x"#);
    let git = only(&script, "git");
    assert_eq!(argv(git)[2], "/repo");
    assert_eq!(argv(git)[5], "/repo-wt/feat x");
}

#[test]
fn eval_default_value_uses_home() {
    let script = posix(r#"D=${DEST:-$HOME/dev/extern}; git clone https://h/r "$D/r""#);
    assert_eq!(argv(only(&script, "git"))[3], "/home/u/dev/extern/r");
}

#[test]
fn eval_default_value_is_skipped_when_set() {
    let script = posix(r#"DEST=/d; D=${DEST:-/fallback}; echo "$D""#);
    assert_eq!(argv(only(&script, "echo"))[1], "/d");
}

#[test]
fn eval_tilde_expands_to_home() {
    let script = posix("git -C ~/dev/soldr worktree add ../soldr-extern/y");
    assert_eq!(argv(only(&script, "git"))[2], "/home/u/dev/soldr");
}

#[test]
fn eval_bash_dash_c_expands_outer_variables_first() {
    let script = posix(r#"R=/repo; bash -c "git -C $R worktree add ../q""#);
    let git = only(&script, "git");
    assert_eq!(git.origin, Origin::NestedShell);
    assert_eq!(argv(git)[2], "/repo");
}

#[test]
fn eval_command_substitution_is_unknown_but_cwd_is_known() {
    let script = posix(r#"R=$(git rev-parse --show-toplevel); git -C "$R" worktree add ../w"#);
    let gits = calls(&script, "git");
    assert_eq!(gits.len(), 2, "{:#?}", script.invocations);
    let inner = gits
        .iter()
        .find(|git| git.origin == Origin::Substitution)
        .expect("the substituted command is scanned too");
    assert_eq!(argv(inner)[1], "rev-parse");
    let outer = gits
        .iter()
        .find(|git| git.origin == Origin::Script)
        .expect("the outer git");
    match &outer.words[2] {
        Value::Unknown(unknown) => {
            assert_eq!(unknown.reason, UnknownReason::CommandSubstitution);
        }
        other => panic!("expected unknown, got {other:?}"),
    }
    assert_eq!(lit(&outer.cwd), "/work");
}

#[test]
fn eval_unknown_program_is_flagged_not_missed() {
    let script = posix("GIT=$(command -v git); $GIT clone https://h/r /x/r");
    let unknown = script
        .invocations
        .iter()
        .find(|inv| inv.program_is_unknown())
        .expect("the $GIT call is reported");
    assert_eq!(unknown.words[1], Value::literal("clone"));
    assert_eq!(unknown.words[3], Value::literal("/x/r"));
}

#[test]
fn eval_loop_over_command_output_is_unknown() {
    let script = posix("for b in $(git branch --format=x); do git worktree add ../wt-$b $b; done");
    let add = calls(&script, "git")
        .into_iter()
        .find(|git| git.origin == Origin::Script)
        .expect("loop body git");
    assert!(add.words[3].is_unknown(), "{:?}", add.words[3]);
    assert!(add.words[4].is_unknown());
    assert_eq!(add.loop_kind, Some(LoopKind::ForEach));
}

// ---- Unknown policy.

#[test]
fn special_parameters_are_unknown() {
    let script = posix("echo $1 $@ $? $RANDOM");
    let echo = only(&script, "echo");
    for word in &echo.words[1..] {
        match word {
            Value::Unknown(unknown) => {
                assert_eq!(unknown.reason, UnknownReason::SpecialParameter, "{word:?}");
            }
            other => panic!("expected unknown, got {other:?}"),
        }
    }
}

#[test]
fn unset_variable_is_unknown_with_its_source_text() {
    let script = posix(r#"rm -rf "$NOPE"/"#);
    let rm = only(&script, "rm");
    match &rm.words[2] {
        Value::Unknown(unknown) => {
            assert_eq!(unknown.reason, UnknownReason::UnsetVariable);
            assert_eq!(unknown.raw, "$NOPE/");
        }
        other => panic!("expected unknown, got {other:?}"),
    }
}

#[test]
fn only_exported_variables_reach_a_child_shell() {
    let local = posix("R=/x; bash -c 'echo $R'");
    assert!(only(&local, "echo").words[1].is_unknown());
    let exported = posix("export R=/x; bash -c 'echo $R'");
    assert_eq!(argv(only(&exported, "echo"))[1], "/x");
}

#[test]
fn branch_dependent_assignment_becomes_unknown() {
    let script = posix(r#"if test -d a; then R=/a; else R=/b; fi; git -C "$R" status"#);
    let git = calls(&script, "git")
        .into_iter()
        .find(|git| git.words.len() == 4)
        .expect("git -C $R status");
    match &git.words[2] {
        Value::Unknown(unknown) => assert_eq!(unknown.reason, UnknownReason::RuntimeValue),
        other => panic!("expected unknown, got {other:?}"),
    }
}

#[test]
fn branch_assignment_agreeing_on_both_paths_stays_known() {
    let script = posix(r#"if test -d a; then R=/a; else R=/a; fi; echo "$R""#);
    assert_eq!(argv(only(&script, "echo"))[1], "/a");
}

#[test]
fn read_makes_its_variable_unknown() {
    let script = posix("R=/a; read R; echo $R");
    assert!(only(&script, "echo").words[1].is_unknown());
}

// ---- Working directory tracking.

#[test]
fn subshell_cd_does_not_leak() {
    let script = posix("(cd /a); git status");
    assert_eq!(lit(&only(&script, "git").cwd), "/work");
}

#[test]
fn brace_group_cd_persists() {
    let script = posix("{ cd /a; }; git status");
    assert_eq!(lit(&only(&script, "git").cwd), "/a");
}

#[test]
fn cd_or_exit_keeps_the_new_directory() {
    let script = posix("cd /a || exit 1; git status");
    assert_eq!(lit(&only(&script, "git").cwd), "/a");
}

#[test]
fn bare_cd_goes_home_and_relative_cd_joins() {
    let script = posix("cd; cd dev/../src; pwd");
    assert_eq!(lit(&only(&script, "pwd").cwd), "/home/u/src");
}

#[test]
fn cd_to_unknown_makes_cwd_unknown() {
    let script = posix("cd \"$(mktemp -d)\"; git init");
    assert!(only(&script, "git").cwd.is_unknown());
}

#[test]
fn pushd_and_popd_are_tracked() {
    let script = posix("pushd /a; pwd; popd; ls");
    assert_eq!(lit(&only(&script, "pwd").cwd), "/a");
    assert_eq!(lit(&only(&script, "ls").cwd), "/work");
}

// ---- Structure the rules read.

#[test]
fn pipeline_slots_share_an_id() {
    let script = posix("a | b | c; d");
    let ids: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|name| only(&script, name).pipeline.expect("in a pipeline"))
        .collect();
    assert!(ids
        .iter()
        .all(|slot| slot.id == ids[0].id && slot.stages == 3));
    assert_eq!(
        ids.iter().map(|slot| slot.stage).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(only(&script, "d").pipeline, None);
}

#[test]
fn and_or_lists_are_not_pipelines() {
    let script = posix("a && b || c");
    assert!(script.invocations.iter().all(|inv| inv.pipeline.is_none()));
    assert_eq!(script.invocations.len(), 3);
}

#[test]
fn loop_kinds_are_reported() {
    let script = posix("while true; do sleep 30; gh pr checks 1; done");
    assert_eq!(only(&script, "sleep").loop_kind, Some(LoopKind::While));
    assert_eq!(only(&script, "gh").loop_kind, Some(LoopKind::While));
    assert_eq!(only(&script, "true").loop_kind, Some(LoopKind::While));
    let until = posix("until gh run view 1; do sleep 5; done");
    assert_eq!(only(&until, "gh").loop_kind, Some(LoopKind::Until));
    let forever = posix("for ((;;)); do sleep 1; done");
    assert_eq!(
        only(&forever, "sleep").loop_kind,
        Some(LoopKind::CStyle { infinite: true })
    );
    let counted = posix("for ((i=0; i<3; i++)); do sleep 1; done");
    assert_eq!(
        only(&counted, "sleep").loop_kind,
        Some(LoopKind::CStyle { infinite: false })
    );
}

#[test]
fn background_jobs_are_marked() {
    let script = posix("sleep 5 & echo hi");
    assert!(only(&script, "sleep").background);
    assert!(!only(&script, "echo").background);
}

#[test]
fn substitutions_are_scanned_with_depth() {
    let script = posix(r#"echo "x $(rm -rf /y)" `pwd` <(ls a)"#);
    let rm = only(&script, "rm");
    assert_eq!((rm.origin, rm.depth), (Origin::Substitution, 1));
    assert_eq!(only(&script, "pwd").origin, Origin::Substitution);
    assert_eq!(only(&script, "ls").origin, Origin::Substitution);
    assert!(only(&script, "echo").words[1].is_unknown());
}

#[test]
fn eval_text_is_parsed_as_code() {
    let script = posix(r#"eval "git -C /e status""#);
    let git = only(&script, "git");
    assert_eq!(git.origin, Origin::NestedShell);
    assert_eq!(argv(git)[2], "/e");
}

#[test]
fn function_bodies_are_scanned() {
    let script = posix(r#"f() { rm -rf "$1"; }; f /"#);
    let rm = only(&script, "rm");
    assert_eq!(rm.origin, Origin::FunctionBody);
    assert!(rm.words[2].is_unknown());
    assert_eq!(argv(only(&script, "f")), ["f", "/"]);
}

#[test]
fn comments_are_ignored() {
    let script = posix("echo ok #rm -rf /");
    assert_eq!(script.invocations.len(), 1);
    assert_eq!(argv(&script.invocations[0]), ["echo", "ok"]);
}

#[test]
fn quoting_and_escapes_resolve() {
    let script = posix(r#"x=$'a\tb'; echo "$x" a\ b \; 'q"q' "s'q""#);
    assert_eq!(
        argv(only(&script, "echo")),
        ["echo", "a\tb", "a b", ";", "q\"q", "s'q"]
    );
}

#[test]
fn prefix_assignments_are_command_env() {
    let script = posix("FOO=1 BAR=$HOME git status 2>/dev/null");
    let git = only(&script, "git");
    assert_eq!(argv(git), ["git", "status"]);
    assert_eq!(
        git.env,
        [
            ("FOO".to_string(), Value::literal("1")),
            ("BAR".to_string(), Value::literal("/home/u")),
        ]
    );
}

#[test]
fn wrappers_are_unwrapped_for_the_effective_command() {
    for text in [
        "sudo -u root git status",
        "command git status",
        "exec git status",
        "nohup git status",
        "time git status",
        "env FOO=1 -u BAR git status",
        "timeout 5 git status",
        "nice -n 5 git status",
        "xargs -n1 git status",
        "stdbuf -oL git status",
    ] {
        let script = posix(text);
        let inv = &script.invocations[0];
        assert_eq!(inv.effective_program().as_deref(), Some("git"), "{text}");
        assert_eq!(argv(inv), ["git", "status"], "{text}");
    }
}

#[test]
fn syntax_errors_are_reported_without_panicking() {
    let script = posix(r#"echo "unterminated"#);
    assert!(script.has_error);
    let script = posix("if then fi ) ( ;; && ||");
    assert!(script.has_error);
}

#[test]
fn empty_and_whitespace_only_input() {
    assert!(posix("").invocations.is_empty());
    assert!(posix("  \n\t ").invocations.is_empty());
}

// ---- PowerShell.

#[test]
fn pwsh_variable_and_dash_capital_c() {
    let script = pwsh(r"$R = 'C:\repo'; git -C $R worktree add ..\wt -b x");
    assert_eq!(
        argv(only(&script, "git")),
        ["git", "-C", r"C:\repo", "worktree", "add", r"..\wt", "-b", "x"]
    );
}

#[test]
fn pwsh_variable_names_are_case_insensitive() {
    let script = pwsh("$r = 'a'; echo $R");
    assert_eq!(argv(only(&script, "echo"))[1], "a");
}

#[test]
fn pwsh_set_location_tracks_cwd() {
    let script = pwsh(r"Set-Location C:\x; git status; sl -Path ..\y; git log");
    let gits = calls(&script, "git");
    assert_eq!(lit(&gits[0].cwd), r"C:\x");
    assert_eq!(lit(&gits[1].cwd), r"C:\y");
}

#[test]
fn pwsh_call_operator_with_absolute_git() {
    let script = pwsh(r"& 'C:\Program Files\Git\bin\git.exe' clone https://h/r C:\d");
    let git = only(&script, "git");
    assert_eq!(argv(git)[1..], ["clone", "https://h/r", r"C:\d"]);
}

#[test]
fn pwsh_env_variables_resolve_from_base() {
    let script = pwsh("git -C $env:USERPROFILE status");
    assert_eq!(argv(only(&script, "git"))[2], r"C:\Users\u");
}

#[test]
fn pwsh_loops_and_script_blocks() {
    let script = pwsh("while ($true) { Start-Sleep 5; gh pr checks 1 }");
    assert_eq!(
        only(&script, "start-sleep").loop_kind,
        Some(LoopKind::While)
    );
    assert_eq!(only(&script, "gh").loop_kind, Some(LoopKind::While));
    let script = pwsh("Get-ChildItem | ForEach-Object { Remove-Item $_ -Recurse }");
    let remove = only(&script, "remove-item");
    assert_eq!(remove.loop_kind, Some(LoopKind::ForEach));
    assert!(remove.words[1].is_unknown());
    assert!(only(&script, "get-childitem").pipeline.is_some());
}

#[test]
fn pwsh_chains_and_subexpressions() {
    let script = pwsh(r#"git status && echo "ok $(Get-Date)" || exit 1"#);
    assert_eq!(only(&script, "get-date").origin, Origin::Substitution);
    assert!(only(&script, "echo").words[1].is_unknown());
    assert_eq!(calls(&script, "exit").len(), 1);
}

#[test]
fn pwsh_foreach_over_literal_array_unrolls() {
    let script = pwsh(r#"foreach ($b in @('a','b')) { git worktree add "..\wt-$b" }"#);
    let gits = calls(&script, "git");
    assert_eq!(gits.len(), 2, "{:#?}", script.invocations);
    assert_eq!(argv(gits[0])[3], r"..\wt-a");
    assert_eq!(argv(gits[1])[3], r"..\wt-b");
}

#[test]
fn posix_launching_powershell_parses_the_command_as_powershell() {
    let script = posix(r#"pwsh -NoProfile -Command "Remove-Item C:\x -Recurse""#);
    let remove = only(&script, "remove-item");
    assert_eq!(remove.origin, Origin::NestedShell);
    assert_eq!(argv(remove), ["Remove-Item", r"C:\x", "-Recurse"]);
}

// ---- cmd.exe.

#[test]
fn cmd_cd_slash_d_and_chain() {
    let script = cmd(r"cd /d C:\repo && git worktree add ..\wt");
    let git = only(&script, "git");
    assert_eq!(lit(&git.cwd), r"C:\repo");
    assert_eq!(argv(git), ["git", "worktree", "add", r"..\wt"]);
}

#[test]
fn cmd_percent_expansion_happens_when_the_line_is_read() {
    // cmd expands `%R%` for the whole line before `set` runs, so on the
    // same line it is still unset; the next line sees the new value.
    let same_line = cmd(r"set R=C:\r & git -C %R% status");
    match &only(&same_line, "git").words[2] {
        Value::Unknown(unknown) => {
            assert_eq!(unknown.reason, UnknownReason::UnsetVariable);
            assert_eq!(unknown.raw, "%R%");
        }
        other => panic!("expected unknown, got {other:?}"),
    }
    let next_line = cmd("set R=C:\\r\r\ngit -C %R% status");
    assert_eq!(argv(only(&next_line, "git"))[2], r"C:\r");
}

#[test]
fn cmd_caret_escapes_an_operator() {
    let script = cmd("echo a ^& b");
    assert_eq!(script.invocations.len(), 1);
    assert_eq!(argv(&script.invocations[0]), ["echo", "a", "&", "b"]);
}

#[test]
fn cmd_pipeline_slots() {
    let script = cmd("dir | findstr x");
    assert_eq!(only(&script, "findstr").pipeline.map(|s| s.stage), Some(1));
}

#[test]
fn posix_launching_cmd_parses_the_command_as_cmd() {
    let script = posix(r#"cmd /c "git clone https://h/r C:\d""#);
    let git = only(&script, "git");
    assert_eq!(git.origin, Origin::NestedShell);
    assert_eq!(argv(git), ["git", "clone", "https://h/r", r"C:\d"]);
}

// ---- Non-functional requirements.

/// Parse plus evaluation of a typical command stays under 5 ms. Measured as
/// the best of several runs per command: the unit suite runs thousands of
/// tests in parallel, so a single sample measures the machine's load, not
/// the parser. The release-built hook is faster still.
#[test]
fn typical_commands_parse_within_the_latency_budget() {
    let commands = [
        "git status",
        "cd /repo && git worktree add ../wt -b feat/x origin/main",
        r#"R=$(git rev-parse --show-toplevel); for f in a b c; do rm -rf "$R/$f"; done"#,
        "cat <<'EOF' > notes.txt\nhello $(world)\nEOF\ngh pr create --title t --body-file notes.txt",
        "find . -name '*.rs' | xargs grep -n foo | head -20",
    ];
    let base = posix_env();
    let worst = commands
        .iter()
        .map(|command| {
            (0..25)
                .map(|_| {
                    let started = std::time::Instant::now();
                    let script = parse(command, Dialect::Posix, &base);
                    let elapsed = started.elapsed();
                    assert!(!script.invocations.is_empty());
                    elapsed
                })
                .min()
                .unwrap_or_default()
        })
        .max()
        .unwrap_or_default();
    assert!(
        worst < std::time::Duration::from_millis(5),
        "parse+evaluate took {worst:?} for a typical command, over the 5 ms budget"
    );
}

#[test]
fn the_module_never_spawns_processes_or_reads_files() {
    let sources = [
        include_str!("mod.rs"),
        include_str!("bash.rs"),
        include_str!("powershell.rs"),
        include_str!("cmd.rs"),
        include_str!("env.rs"),
        include_str!("paths.rs"),
        include_str!("words.rs"),
    ];
    let banned = [
        concat!("std::", "process"),
        concat!("running", "_process"),
        concat!("Command", "::new"),
        concat!("std::", "fs"),
        concat!("std::", "net"),
    ];
    for source in sources {
        for needle in banned {
            assert!(!source.contains(needle), "shell_ast must not use {needle}");
        }
    }
}

/// #1972: a heredoc body stays with the command that reads it, so a script
/// written to a file or fed to Python can be judged.
#[test]
fn a_heredoc_body_is_kept_on_the_command_that_reads_it() {
    let base = posix_env();
    let script = parse(
        "cat > w.sh <<'EOF'\nwhile true; do sleep 1; done\nEOF\necho done",
        Dialect::Posix,
        &base,
    );
    let cat = only(&script, "cat");
    assert_eq!(
        cat.heredoc.as_deref().map(str::trim),
        Some("while true; do sleep 1; done")
    );
    assert!(only(&script, "echo").heredoc.is_none());
    let fed = parse("python - <<'PY'\nimport time\nPY", Dialect::Posix, &base);
    assert!(only(&fed, "python")
        .heredoc
        .as_deref()
        .is_some_and(|body| body.contains("import time")));
}

/// `cat <<'EOF' > run.sh`: a redirect after the heredoc marker still
/// belongs to the command.
#[test]
fn a_redirect_after_the_heredoc_marker_is_the_commands() {
    let script = parse(
        "cat <<'EOF' > run.sh\nsleep 1\nEOF",
        Dialect::Posix,
        &posix_env(),
    );
    let cat = only(&script, "cat");
    assert!(cat
        .redirects
        .iter()
        .any(|redirect| redirect.writes_file() && redirect.target.as_literal() == Some("run.sh")));
    assert!(cat.heredoc.is_some());
}

#[test]
fn while_read_walks_lines() {
    let script = parse(
        "ls | while read f; do echo \"$f\"; done",
        Dialect::Posix,
        &BaseEnv::default(),
    );
    assert_eq!(only(&script, "echo").loop_kind, Some(LoopKind::ForEach));
    let script = parse(
        "while true; do sleep 5; done",
        Dialect::Posix,
        &BaseEnv::default(),
    );
    assert_eq!(only(&script, "sleep").loop_kind, Some(LoopKind::While));
}
