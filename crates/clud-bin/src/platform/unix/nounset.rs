//! Unix-only tests of `crate::shell::nounset`: they run a real bash.

#[cfg(test)]
mod tests {
    use crate::shell::nounset::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    #[test]
    fn generated_startup_file_restores_deletion_shim_after_shell_path_reset() {
        let tmp = TemporaryDirectory::new().unwrap();
        let shims = tmp.path().join("shims");
        std::fs::create_dir(&shims).unwrap();
        let mut env = stock_env();
        env.extend(env_overrides_at(tmp.path(), false, None));
        env.push((
            crate::shim_session::RM_SHIM_DIR_KEY.to_string(),
            shims.display().to_string(),
        ));
        let (code, output) = bash_under(env, "PATH=/usr/bin; printf '%s' \"$PATH\"");
        assert_eq!(code, 0, "{output}");
        assert!(output.starts_with("/usr/bin"), "{output}");
        let mut env = stock_env();
        env.extend(env_overrides_at(tmp.path(), false, None));
        env.push((
            crate::shim_session::RM_SHIM_DIR_KEY.to_string(),
            shims.display().to_string(),
        ));
        let (code, output) = bash_under(env, "printf '%s' \"$PATH\"");
        assert_eq!(code, 0, "{output}");
        assert!(output.starts_with(&shims.display().to_string()), "{output}");
    }

    #[test]
    fn rm_path_only_startup_keeps_nounset_off_after_login_path_reset() {
        let tmp = TemporaryDirectory::new().unwrap();
        let shims = tmp.path().join("shims");
        std::fs::create_dir(&shims).unwrap();
        let mut env = stock_env();
        env.extend(rm_path_overrides_at(tmp.path(), None));
        env.push((
            crate::shim_session::RM_SHIM_DIR_KEY.to_string(),
            shims.display().to_string(),
        ));
        let (code, output) = bash_under(env, "printf '%s|%s' \"$PATH\" \"$UNSET_FOR_TEST\"");
        assert_eq!(code, 0, "{output}");
        assert!(output.starts_with(&shims.display().to_string()), "{output}");
        assert!(output.ends_with('|'), "{output}");
    }

    #[test]
    fn chaining_through_another_clud_shim_does_not_recurse_forever() {
        // Two clud sessions with different state dirs (e.g. a nested session
        // whose CLUD_DAEMON_STATE_DIR points at a pytest fixture directory,
        // #1201's actual production case) generate two different, but
        // byte-identical, shim files. same_file() correctly says these are not
        // the same file, so the inner one's CLUD_PREV_BASH_ENV is chained to the
        // outer one -- but `.` runs in the same shell, so CLUD_PREV_BASH_ENV is
        // unchanged when the outer shim's own copy of this exact template
        // re-reads it, and it sources itself, and so on: true infinite
        // recursion, unbounded by canonicalization, because the two files are
        // genuinely distinct on disk. bash dies with SIGSEGV in push_source
        // (observed in production as a stack-overflowing coredump).
        let outer = TemporaryDirectory::new().unwrap();
        let outer_overrides = env_overrides_at(outer.path(), false, None);
        let outer_shim = outer_overrides
            .iter()
            .find(|(key, _)| key == BASH_ENV_KEY)
            .map(|(_, value)| value.clone())
            .unwrap();

        let inner = TemporaryDirectory::new().unwrap();
        let mut env = stock_env();
        env.extend(env_overrides_at(inner.path(), false, Some(outer_shim)));
        assert!(
            env.iter().any(|(key, _)| key == PREV_KEY),
            "a genuinely different file must still be chained"
        );

        let (code, output) = bash_under(env, "true");
        assert_eq!(
            code, 0,
            "bash must not recurse sourcing a distinct-but-identical shim: {output}"
        );
    }

    #[test]
    fn the_recursion_guard_does_not_leak_into_the_armed_shell() {
        // CLUD_NOUNSET_ARMED must not still be exported once the shim finishes:
        // a long-lived launcher process (or a persistent interactive shell) that
        // ever saw it set would leak it into every later, unrelated subprocess,
        // silently skipping nounset for all of them.
        let tmp = TemporaryDirectory::new().unwrap();
        let mut env = stock_env();
        env.extend(env_overrides_at(tmp.path(), false, None));

        let (code, output) = bash_under(env, r#"echo "[${CLUD_NOUNSET_ARMED:-}]""#);

        assert_eq!(code, 0, "the probe itself must not trip nounset: {output}");
        assert_eq!(
            output, "[]",
            "the guard must be unset again before the shim returns"
        );
    }

    #[test]
    fn a_symlink_spelling_does_not_source_recursively() {
        let tmp = TemporaryDirectory::new().unwrap();
        let first = env_overrides_at(tmp.path(), false, None);
        let ours = first
            .iter()
            .find(|(key, _)| key == BASH_ENV_KEY)
            .map(|(_, value)| value.clone())
            .unwrap();

        let link = tmp.path().join("alias.sh");
        std::os::unix::fs::symlink(&ours, &link).unwrap();

        let mut env = stock_env();
        env.extend(env_overrides_at(
            tmp.path(),
            false,
            Some(link.display().to_string()),
        ));

        assert!(!env.iter().any(|(key, _)| key == PREV_KEY));

        let (code, output) = bash_under(env, "true");
        assert_eq!(code, 0, "bash must not recurse sourcing the shim: {output}");
    }

    /// Run `bash -c <script>` under exactly `env`, returning `(exit_code, output)`.
    ///
    /// The repo routes all subprocess execution through `running_process`, so this
    /// does too. Streams are merged because every assertion below is about what
    /// the shell said, not which pipe it said it on.
    fn bash_under(env: Vec<(String, String)>, script: &str) -> (i32, String) {
        use running_process::{
            CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
        };
        use std::time::Duration;

        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec![
                "bash".to_string(),
                "-c".to_string(),
                script.to_string(),
            ]),
            cwd: None,
            env: Some(env),
            capture: true,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Null,
            nice: None,
            address_space_limit_bytes: None,
        });
        process.start().expect("bash must be runnable");

        let mut buf = Vec::<u8>::new();
        loop {
            match process.read_combined(Some(Duration::from_millis(100))) {
                ReadStatus::Line(event) => {
                    buf.extend_from_slice(&event.line);
                    buf.push(b'\n');
                }
                ReadStatus::Timeout => {
                    if process.returncode().is_some() {
                        break;
                    }
                }
                ReadStatus::Eof => break,
            }
        }
        // Exit is not end of output: an unterminated last line (`printf '%s'`)
        // is emitted only at pipe EOF, which can trail the exit code on a loaded
        // machine. `wait` runs running-process's bounded capture drain, so the
        // queue is complete once it returns.
        let code = process
            .wait(Some(Duration::from_secs(30)))
            .expect("bash must exit");
        for event in process.drain_combined() {
            buf.extend_from_slice(&event.line);
            buf.push(b'\n');
        }
        (code, String::from_utf8_lossy(&buf).trim().to_string())
    }

    /// The environment a bash inherits when clud has contributed nothing — the
    /// ambient one minus anything this module owns, so an outer clud session
    /// cannot arm the "unarmed" baseline.
    fn stock_env() -> Vec<(String, String)> {
        // Built explicitly rather than inherited.
        //
        // This used to be `std::env::vars()` minus the two keys we own. That reads
        // the whole process environment, which sibling tests in this binary mutate
        // (`HOME`, `USERPROFILE`, and others, from several files each holding its
        // own lock). Iterating it while another thread writes is a race by
        // construction, and it showed up as this file failing about 1 run in 8 of
        // the full suite while passing alone.
        //
        // Nothing here needs the ambient environment. A bash that runs `echo` and
        // sources a file needs a PATH and nothing else, so the fixture says so and
        // stops depending on what the rest of the suite is doing.
        vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
        )]
    }

    /// A real non-interactive bash must actually abort, not merely be handed a
    /// file with the right characters in it. `bash -c` is non-interactive, which
    /// is both the only mode that reads `BASH_ENV` and the mode every Bash tool
    /// call runs in — so this is the production path, not an approximation of it.
    ///
    /// The unset-then-set pair is the point: the same command under the same shell
    /// succeeds without our overrides and dies with them. Asserting only the
    /// failure would pass against a bash that was broken for some other reason.
    #[test]
    fn a_real_non_interactive_bash_aborts_on_an_unset_expansion() {
        // `rm -rf "$SP"/` from #1064, reduced to an echo. Under nounset the shell
        // dies before the command runs; without it, `$SP` becomes empty.
        const PROBE: &str = r#"echo "[${SP}]""#;

        let tmp = TemporaryDirectory::new().unwrap();
        let overrides = env_overrides_at(tmp.path(), false, None);

        let (code, output) = bash_under(stock_env(), PROBE);
        assert_eq!(code, 0, "baseline: a stock shell survives this: {output}");
        assert_eq!(
            output, "[]",
            "the unset variable silently expands to nothing — the whole problem"
        );

        let mut armed = stock_env();
        armed.extend(overrides);
        let (code, output) = bash_under(armed, PROBE);
        assert_ne!(code, 0, "the same command must fail once armed: {output}");
        assert!(
            output.contains("SP") && output.contains("unbound variable"),
            "the failure must name the variable so it is diagnosable: {output}"
        );
        assert!(
            !output.contains("[]"),
            "the command must not have run at all: {output}"
        );
    }

    /// Chaining is only worth writing if the chained file is really sourced. A
    /// generated file that silently dropped the user's `BASH_ENV` would look
    /// identical to this one from the outside.
    #[test]
    fn an_inherited_bash_env_is_really_sourced_by_the_generated_file() {
        let tmp = TemporaryDirectory::new().unwrap();
        let theirs = tmp.path().join("theirs.sh");
        std::fs::write(&theirs, "export FROM_THEIRS=yes\n").unwrap();

        let mut env = stock_env();
        env.extend(env_overrides_at(
            tmp.path(),
            false,
            Some(theirs.display().to_string()),
        ));

        let (code, output) = bash_under(env, r#"echo "[${FROM_THEIRS}]""#);

        assert_eq!(
            code, 0,
            "sourcing theirs must not itself trip nounset: {output}"
        );
        assert_eq!(
            output, "[yes]",
            "the user's own BASH_ENV must still take effect"
        );
    }

    /// The ordering rule, proven against a real startup file rather than by
    /// reading the generated text. A stock `BASH_ENV` that tests an unset variable
    /// — which is what `[ -z "$PS1" ]` does, and it is everywhere — must still
    /// work. Arming nounset before sourcing theirs would kill every tool call here
    /// with an error pointing at a file the user never connected to clud.
    #[test]
    fn a_stock_startup_file_that_tests_an_unset_variable_still_works() {
        let tmp = TemporaryDirectory::new().unwrap();
        let theirs = tmp.path().join("theirs.sh");
        std::fs::write(
            &theirs,
            "if [ -z \"$PS1\" ]; then export FROM_THEIRS=batch; fi\n",
        )
        .unwrap();

        let mut env = stock_env();
        env.extend(env_overrides_at(
            tmp.path(),
            false,
            Some(theirs.display().to_string()),
        ));

        let (code, output) = bash_under(env, r#"echo "[${FROM_THEIRS}]""#);

        assert_eq!(
            code, 0,
            "their file must run under the shell it was written for: {output}"
        );
        assert_eq!(output, "[batch]", "{output}");
    }

    /// #1457: a script file bash runs for another program (git's
    /// `git-submodule` sourcing `git-sh-i18n`) must not be armed, while the
    /// agent's own `-c` command line, and a nested `bash -c`, still are.
    #[test]
    fn nounset_guards_command_strings_but_not_nested_script_files() {
        let tmp = TemporaryDirectory::new().unwrap();
        let script = tmp.path().join("git-sh-i18n");
        std::fs::write(
            &script,
            "# like git-sh-i18n line 10\nif test -z \"$GIT_TEXTDOMAINDIR\"; then echo \"i18n-ok\"; fi\n",
        )
        .unwrap();
        let mut armed = stock_env();
        armed.extend(env_overrides_at(tmp.path(), false, None));

        let (code, output) = bash_under(armed.clone(), r#"echo "[${UNSET_VAR}]""#);
        assert_ne!(
            code, 0,
            "the agent's command line must stay armed: {output}"
        );
        assert!(output.contains("UNSET_VAR"), "{output}");

        let (code, output) = bash_under(armed.clone(), r#"bash -c 'echo "[${UNSET_VAR}]"'"#);
        assert_ne!(code, 0, "a nested bash -c must stay armed: {output}");

        let cmd = format!("bash '{}'", script.display());
        let (code, output) = bash_under(armed, &cmd);
        assert_eq!(code, 0, "a nested script file must run unarmed: {output}");
        assert!(output.contains("i18n-ok"), "{output}");
    }
}
