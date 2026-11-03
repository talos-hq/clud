//! The `clud-shim` personality of the one `clud` binary: every clud PATH alias
//! (`python`, `python3`, `gh`, `git`, `rm`, `safe-rm`, `safe-mktemp`) is a hardlink, symlink or copy
//! of `clud` that [`crate::multicall`] routes here by argv[0] before any other
//! startup work (#406, #1461, #1518, #1546, #1551).
//!
//! [`run`] hands argv to [`dispatch::run`], the single entry path. Dispatch
//! looks the invoked name up in `crate::shim_registry::SHIMS`, validates the
//! session, and either runs one of the handlers below with a validated
//! [`dispatch::Session`] or execs the next real binary on PATH. Outside a
//! valid clud session every alias behaves like the binary it shadows; see
//! `docs/architecture/shim-dispatch.md`.
//!
//! Handlers never read a session key and never decide a no-session exit;
//! `session_contract_is_owned_by_dispatch` below enforces that.

mod dispatch;

use std::ffi::OsString;
use std::path::Path;

/// Run the shim named by `argv[0]` and return its exit code.
pub fn run(argv: &[OsString]) -> i32 {
    dispatch::run(argv)
}

/// Replace this process with `path` (Unix `exec`), or run it and return its
/// exit code (Windows). Only an exec failure returns, as 126.
fn exec(label: &str, path: &Path, args: &[OsString]) -> i32 {
    let mut command = std::process::Command::new(path);
    command.args(args);
    crate::platform::shim_main::exec(label, path, command)
}

/// #1486: run `path` with `args` as a child with inherited stdin, stdout and
/// stderr, record its exit, and return it. Unlike [`exec`], the shim outlives
/// the child so the telemetry line can carry the exit code and duration; the
/// streams never pass through the shim.
///
/// Unix: while the child runs the shim ignores SIGINT and SIGQUIT (the
/// terminal sends them to the whole foreground process group, so the child
/// gets its own copy, as under a shell) and forwards SIGTERM and SIGHUP sent
/// to the shim alone. A child killed by signal N is recorded as `128 + N`,
/// then the shim re-raises N on itself so its caller sees the same wait
/// status as when the shim `exec`ed.
fn run_child(
    label: &str,
    path: &Path,
    args: &[OsString],
    recorder: &crate::shim_telemetry::Recorder,
    env: &[(&str, &std::ffi::OsStr)],
) -> i32 {
    let mut command = std::process::Command::new(path);
    command.args(args);
    command.envs(env.iter().copied());
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "clud {label} shim: failed to exec {}: {error}",
                path.display()
            );
            recorder.record(126);
            return 126;
        }
    };
    crate::platform::shim_main::wait_child(label, path, child, recorder)
}

/// #1486: one telemetry line per in-session `git` / `gh` invocation. A
/// handler that ran a child records its exit itself, before it re-raises a
/// fatal signal; every other return is recorded here. Recording never fails
/// and never touches the streams.
fn recorded(
    tool: &str,
    args: &[OsString],
    run: impl FnOnce(&crate::shim_telemetry::Recorder) -> i32,
) -> i32 {
    let recorder = crate::shim_telemetry::Recorder::start(tool, args);
    let code = run(&recorder);
    recorder.record(code);
    code
}

mod git_shim {
    //! In-session `git` (#1486): a telemetry pass-through. Every argv runs
    //! on the real binary unchanged; nothing is refused.

    use std::ffi::OsString;
    use std::path::Path;

    pub fn run(target: &Path, args: &[OsString]) -> i32 {
        super::recorded("git", args, |recorder| {
            super::run_child("git", target, args, recorder, &[])
        })
    }
}

mod python_shim {
    //! In-session `python` / `python3`: run the interpreter clud resolved
    //! at startup, before the alias directory went on PATH.

    use std::ffi::OsString;
    use std::path::Path;

    pub fn run(target: &Path, args: &[OsString]) -> i32 {
        super::exec("python", target, args)
    }
}

mod safe_rm {
    //! `safe-rm` (#1461): a clud command, not a relay. Its roots fall back to
    //! the git checkout or cwd outside a session, so it has no passthrough.

    use std::ffi::OsString;

    pub fn run(args: &[OsString]) -> i32 {
        let args: Option<Vec<String>> = args.iter().map(|a| a.clone().into_string().ok()).collect();
        let Some(args) = args else {
            eprintln!("safe-rm: non-UTF8 arguments are not supported");
            return 2;
        };
        crate::rm_tool::run(&args)
    }
}

mod rm_shim {
    //! In-session child `rm`: the catastrophe floor, then a handoff to the
    //! next `rm` on PATH. See `docs/architecture/rm-protection.md`.

    use std::ffi::OsString;

    pub fn run(args: &[OsString]) -> i32 {
        let args: Option<Vec<String>> = args.iter().map(|a| a.clone().into_string().ok()).collect();
        let Some(args) = args else {
            return crate::rm_guard::deny("non-UTF8 rm arguments");
        };
        match crate::rm_guard::prepare(&args) {
            Ok(plan) => {
                let code = execute_handoff(&plan);
                crate::rm_guard::audit(&args, Some(&plan), code, None);
                code
            }
            Err(reason) => {
                let code = crate::rm_guard::deny(&reason);
                crate::rm_guard::audit(&args, None, code, Some(&reason));
                code
            }
        }
    }

    fn execute_handoff(plan: &crate::rm_guard::Plan) -> i32 {
        let mut command = vec![plan.program.to_string_lossy().into_owned()];
        command.extend(plan.argv.iter().cloned());
        match crate::subprocess::ManagedSubprocess::start_inheriting_env(command, None, false, None)
        {
            Ok(child) => child.wait(None).unwrap_or(2),
            Err(error) => crate::rm_guard::deny(&format!("system handoff failed: {error}")),
        }
    }
}

pub(crate) mod gh_shim {
    //! In-session `gh` relay. `pr checks --watch` is upgraded to the bundled
    //! watcher, and `gh api` GETs may be answered by the daemon read broker
    //! (#1743, docs/architecture/gh-read-broker.md).

    use std::ffi::OsString;
    use std::path::Path;

    use super::dispatch::{GhRoute, GhSession};
    use crate::shim_telemetry::Recorder;

    pub fn run(session: &GhSession, args: &[OsString]) -> i32 {
        super::recorded("gh", args, |recorder| relay(session, args, recorder))
    }

    fn relay(session: &GhSession, args: &[OsString], recorder: &Recorder) -> i32 {
        if let (true, Some(watcher)) = (session.fail_fast, session.watcher.as_deref()) {
            match watch_words(args) {
                Ok(Some(words)) => return watch(watcher, &words, recorder),
                Ok(None) => {}
                Err(code) => return code,
            }
        }
        // #2352: dispatch already refused a missing ghd (`GhdMissing`).
        if let GhRoute::Ghd(ghd) = &session.route {
            return via_ghd(ghd, &session.target, args, recorder);
        }
        let Some(broker) = session.read_broker.as_ref() else {
            return exec(&session.target, args, recorder);
        };
        if let Some(code) = brokered_read(&session.target, broker, args, recorder) {
            return code;
        }
        let code = exec(&session.target, args, recorder);
        if crate::gh_broker::classify::may_write(args) {
            let tags = crate::gh_broker::scope::write_tags(args, broker.gh_repo.as_deref());
            broker.invalidate(tags.as_deref());
        }
        code
    }

    /// #1743: answer a `gh api` GET from the daemon's read broker. The real
    /// `gh` still formats the output: it reruns the caller's argv with the
    /// endpoint swapped for a one-shot loopback URL serving the brokered
    /// body. `None` (not a read, no daemon, any miss) runs the real `gh`.
    fn brokered_read(
        target: &Path,
        broker: &crate::gh_broker::client::BrokerClient,
        args: &[OsString],
        recorder: &Recorder,
    ) -> Option<i32> {
        let read = crate::gh_broker::classify::api_read(args)?;
        let response = broker.read(&read)?;
        let url = crate::gh_broker::client::serve_replay(response).ok()?;
        let mut replay = args.to_vec();
        replay[read.endpoint_index] = url.into();
        Some(exec(target, &replay, recorder))
    }

    pub(crate) fn watch_words(args: &[OsString]) -> Result<Option<Vec<&str>>, i32> {
        let words: Option<Vec<&str>> = args.iter().map(|arg| arg.to_str()).collect();
        if let Some(words) = words {
            return Ok(is_watch(&words).then_some(words));
        }
        let lossy: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        let words: Vec<_> = lossy.iter().map(|word| word.as_ref()).collect();
        if is_watch(&words) {
            eprintln!("clud gh shim: non-UTF8 PR-watch arguments are ambiguous");
            Err(2)
        } else {
            Ok(None)
        }
    }

    fn is_watch(words: &[&str]) -> bool {
        if !words
            .iter()
            .any(|w| *w == "--watch" || w.starts_with("--watch="))
        {
            return false;
        }
        let mut positionals = Vec::new();
        let mut skip_value = false;
        for word in words {
            if skip_value {
                skip_value = false;
            } else if matches!(*word, "--repo" | "-R" | "--interval" | "-i" | "--hostname") {
                skip_value = true;
            } else if !word.starts_with('-') {
                positionals.push(*word);
            }
        }
        positionals.starts_with(&["pr", "checks"])
    }

    fn watch(clud: &Path, words: &[&str], recorder: &Recorder) -> i32 {
        let mut selector: Option<&str> = None;
        let mut repo: Option<&str> = None;
        let mut interval: Option<&str> = None;
        let mut watch_seen = false;
        let mut command = Vec::new();
        let mut i = 0;
        while i < words.len() {
            let word = words[i];
            match word {
                "pr" if command.is_empty() => command.push(word),
                "checks" if command == ["pr"] => command.push(word),
                "--watch" => watch_seen = true,
                // #2444: the watcher's default scope is already the
                // branch-required checks, which is what `--required` asks for.
                "--fail-fast" | "--required" => {}
                "--repo" | "-R" | "--interval" | "-i" => {
                    i += 1;
                    let Some(value) = words.get(i).copied() else {
                        return invalid(word);
                    };
                    if value.is_empty() || value.starts_with('-') {
                        return invalid(word);
                    }
                    if word == "--repo" || word == "-R" {
                        repo = Some(value)
                    } else {
                        interval = Some(value)
                    }
                }
                _ if word.starts_with("--repo=") && word.len() > 7 => repo = Some(&word[7..]),
                _ if word.starts_with("-R") && word.len() > 2 => repo = Some(&word[2..]),
                _ if word.starts_with("--interval=") && word.len() > 11 => {
                    interval = Some(&word[11..])
                }
                _ if word.starts_with("-i") && word.len() > 2 => interval = Some(&word[2..]),
                _ if word.starts_with('-') => return invalid(word),
                _ if selector.is_none() => selector = Some(word),
                _ => return invalid(word),
            }
            i += 1;
        }
        if command != ["pr", "checks"] || !watch_seen {
            return invalid("ambiguous gh command");
        }
        let mut args: Vec<OsString> = ["tool", "run", "github/pr_merge_watch.py"]
            .map(OsString::from)
            .to_vec();
        // Refs #1741: a branch, URL or omitted selector goes to the watcher
        // as is. It resolves the PR on `--repo`, else on origin, with the same
        // helper it uses for the watch itself. A bare `gh pr view <branch>`
        // here resolved to the parent repository in a fork.
        if let Some(value) = selector.filter(|value| !value.is_empty()) {
            args.push(value.into());
        }
        // #1742: the upgrade is fail-fast only. The watcher's default also
        // cancels on review activity and on close, which turned an open
        // CodeRabbit thread into a cancelled matrix. On a failure the watcher
        // cancels only the failing run and older runs of its workflow.
        args.extend(["--cancel-on", "fail"].map(OsString::from));
        if let Some(value) = repo {
            args.extend([OsString::from("--repo"), value.into()]);
        }
        if let Some(value) = interval {
            args.extend([OsString::from("--interval"), value.into()]);
        }
        exec(clud, &args, recorder)
    }

    fn invalid(value: &str) -> i32 {
        eprintln!("clud gh shim: unsupported or ambiguous pr checks --watch argument: {value}");
        2
    }

    fn exec(path: &Path, args: &[OsString], recorder: &Recorder) -> i32 {
        super::run_child("gh", path, args, recorder, &[])
    }

    /// #2352: `ghd gh ARGS`. ghd serves what it can and relays the rest to
    /// `real_gh`, which is passed explicitly so the relay never resolves
    /// back to this shim.
    fn via_ghd(ghd: &Path, real_gh: &Path, args: &[OsString], recorder: &Recorder) -> i32 {
        let ghd_args = ghd_argv(args);
        let env = [(GHD_REAL_GH, real_gh.as_os_str())];
        super::run_child("gh", ghd, &ghd_args, recorder, &env)
    }

    /// The argv ghd receives: the `gh` subcommand word, then the caller's
    /// arguments verbatim.
    pub(super) fn ghd_argv(args: &[OsString]) -> Vec<OsString> {
        let mut ghd_args = vec![OsString::from("gh")];
        ghd_args.extend(args.iter().cloned());
        ghd_args
    }

    pub(crate) use crate::shim_registry::GHD_REAL_GH_KEY as GHD_REAL_GH;
}

/// The central contract (#1546): dispatch alone reads session keys, and no
/// handler can exit for a missing or stale session.
#[cfg(test)]
#[test]
fn session_contract_is_owned_by_dispatch() {
    use crate::shim_registry::{self as registry, SHIMS};

    fn production(source: &str) -> &str {
        source.split("#[cfg(test)]").next().unwrap()
    }
    let handlers = production(include_str!("shim_main.rs"));
    let libs = [
        ("rm_guard.rs", production(include_str!("rm_guard.rs"))),
        ("rm_tool.rs", production(include_str!("rm_tool.rs"))),
        ("safe_mktemp.rs", production(include_str!("safe_mktemp.rs"))),
    ];
    let key_idents = [
        "ABI_KEY",
        "SESSION_DIR_KEY",
        "PYTHON_TARGET_KEY",
        "GH_TARGET_KEY",
        "GH_ACTIVE_KEY",
        "GH_FAIL_FAST_KEY",
        "GIT_TARGET_KEY",
        "CLUD_EXE_KEY",
        "GH_READ_BROKER_KEY",
        "DAEMON_STATE_DIR_KEY",
        "SESSION_ID_KEY",
        "GH_FRESH_KEY",
    ];
    let mut needles: Vec<String> = registry::SESSION_KEYS
        .iter()
        .map(|key| key.to_string())
        .collect();
    needles.extend(key_idents.iter().map(|ident| ident.to_string()));
    needles.push("CLUD_EXE".to_string());
    needles.push("CLUD_DAEMON_SOCKET".to_string());
    needles.push("env::var".to_string());
    for needle in &needles {
        assert!(
            !handlers.contains(needle.as_str()),
            "shim_main.rs handlers must not read {needle}; validate it in dispatch"
        );
    }
    for (file, source) in libs {
        for key in registry::SESSION_KEYS {
            assert!(!source.contains(key), "{file} reads session key {key}");
        }
    }
    assert_eq!(
        handlers.matches("exit(").count(),
        0,
        "handlers return codes; only the multicall entry exits"
    );
    assert!(handlers.contains("dispatch::run(argv)"));
    assert!(
        !handlers.contains("127"),
        "command-not-found belongs to dispatch's passthrough"
    );
    // Shim names reach dispatch and the installers only through the registry.
    let dispatch = production(include_str!("shim_main/dispatch.rs"));
    let install = production(include_str!("shim_install.rs"));
    for spec in SHIMS {
        for spelling in [spec.name.to_string(), format!("{}.exe", spec.name)] {
            let literal = format!("\"{spelling}\"");
            assert!(!dispatch.contains(&literal), "dispatch hardcodes {literal}");
            assert!(
                !install.contains(&literal),
                "shim_install hardcodes {literal}"
            );
        }
    }
    assert!(
        !handlers.contains("file_name()"),
        "no argv[0] if-chain outside the registry lookup"
    );
}

/// #2352: the `ghd gh ARGS` argv and the relay variable name are the contract
/// with ghd; pin them so a change cannot silently break the relay.
#[cfg(test)]
#[test]
fn ghd_argv_prefixes_gh_and_preserves_every_arg() {
    fn argv(words: &[&str]) -> Vec<OsString> {
        let args: Vec<OsString> = words.iter().map(OsString::from).collect();
        gh_shim::ghd_argv(&args)
    }
    let expected = |words: &[&str]| -> Vec<OsString> { words.iter().map(OsString::from).collect() };

    assert_eq!(
        argv(&["pr", "view", "12", "--json", "state"]),
        expected(&["gh", "pr", "view", "12", "--json", "state"])
    );
    assert_eq!(argv(&[]), expected(&["gh"]));
    assert_eq!(
        argv(&["issue", "comment", "--body", "two words", "--", "-x"]),
        expected(&["gh", "issue", "comment", "--body", "two words", "--", "-x"])
    );
    assert_eq!(gh_shim::GHD_REAL_GH, "GHD_REAL_GH");
}
