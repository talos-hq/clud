//! Subprocess- and PTY-mode runners for a single [`LaunchPlan`].
//!
//! These were inlined in `main.rs` until the file crossed 1k LOC. They
//! contain the per-iteration loop, the stream-json fallback, the
//! Ctrl-C-aware child teardown, and the launch-mode-specific wiring for
//! the OLE drag-drop registration.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::Backend;
use crate::clud_settings;
use crate::command;
use crate::console_setup::enable_console_vt_input;
use crate::cpu_banner;
use crate::launch_log;
use crate::loop_artifacts;
use crate::loop_check::{
    check_loop_markers, check_loop_markers_with_output, loop_unconverged_exit,
};
use crate::process_tree;
use crate::session;
use crate::stage_trace;
use crate::stream_json;
use crate::subprocess;
use crate::verbose_log;
use crate::voice;
use crate::wedge_watchdog;
use crate::win_creation_flags;

#[path = "runner_exit.rs"]
mod runner_exit;
#[path = "runner_terminal.rs"]
mod runner_terminal;
pub use runner_execution::run_plan_pty;

/// Merge two optional byte channels into one. Used by `run_plan_pty`
/// to combine the drag-drop side channel with the Windows console-input
/// reader (issue #141 follow-up) before handing the result to the
/// pump's `extra_rx` slot.
///
/// Zero or one input returns the inputs themselves (no extra thread).
/// Two inputs spawn a small forwarder thread per channel that drains
/// each input and forwards bytes to a unified output channel. The
/// forwarders exit when their input closes or the output drops.
fn merge_extra_rx(
    a: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    b: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
) -> Option<std::sync::mpsc::Receiver<Vec<u8>>> {
    match (a, b) {
        (None, None) => None,
        (Some(rx), None) | (None, Some(rx)) => Some(rx),
        (Some(a), Some(b)) => {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            for input in [a, b] {
                let tx = tx.clone();
                std::thread::Builder::new()
                    .name("clud-extra-rx-merge".into())
                    .spawn(move || {
                        while let Ok(chunk) = input.recv() {
                            if tx.send(chunk).is_err() {
                                break;
                            }
                        }
                    })
                    .ok();
            }
            Some(rx)
        }
    }
}

/// Poll interval for [`lease_shared_rx`]'s forwarder; also bounds how long
/// dropping a [`SharedRxLease`] can block.
const SHARED_RX_LEASE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Guard for one iteration's lease on a shared receiver. Dropping it
/// stops the forwarder thread and joins it (bounded by
/// [`SHARED_RX_LEASE_POLL`]), so the next lease is the only reader.
struct SharedRxLease {
    stop: std::sync::Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SharedRxLease {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Lease a process-lifetime receiver for one PTY iteration (#1360).
///
/// The OLE drag-drop receiver outlives every loop iteration, but
/// [`merge_extra_rx`] moves its inputs into forwarder threads that die
/// with the iteration's merged output. Moving the drag-drop receiver
/// there meant only iteration 0 ever saw drops. Instead, each iteration
/// gets a fresh channel fed by a forwarder that polls the shared receiver
/// under its mutex and never moves it; the returned [`SharedRxLease`]
/// stops that forwarder when the iteration ends.
fn lease_shared_rx(
    shared: &std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<Vec<u8>>>>,
) -> (std::sync::mpsc::Receiver<Vec<u8>>, SharedRxLease) {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let thread_stop = std::sync::Arc::clone(&stop);
    let shared = std::sync::Arc::clone(shared);
    let thread = std::thread::Builder::new()
        .name("clud-dnd-lease".into())
        .spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                let received = match shared.lock() {
                    Ok(guard) => guard.recv_timeout(SHARED_RX_LEASE_POLL),
                    Err(_) => break,
                };
                match received {
                    Ok(chunk) => {
                        if tx.send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .ok();
    (rx, SharedRxLease { stop, thread })
}

/// The two keys that force UTF-8 on any Python helper the agent shells
/// out to. Public so the daemon-side drift guard can assert against the
/// same list rather than restating it (#1209).
pub const WINDOWS_STDIO_KEYS: &[&str] = &["PYTHONIOENCODING", "PYTHONUTF8"];

/// Set only by a parsed `clud --unsafe` launch; helpers inherit it from the
/// session rather than interpreting a user's ambient environment themselves.
pub const UNSAFE_MODE_ENV: &str = "CLUD_UNSAFE_MODE";

pub fn unsafe_mode_enabled() -> bool {
    std::env::var(UNSAFE_MODE_ENV).ok().as_deref() == Some("1")
}

/// Every environment key the child-env policy owns, in one list.
///
/// The Windows stdio pair is included on **every** platform on purpose:
/// the daemon/runner drift guard compares the two builders key by key, so
/// a platform-neutral list lets it assert "both builders agree on this
/// key's value" (absent == absent off Windows, `1`/`utf-8` == `1`/`utf-8`
/// on Windows). `PATH` is deliberately absent — `activate_rm` prepends to
/// whatever the base carried rather than owning the value.
pub fn child_env_policy_keys() -> Vec<&'static str> {
    let mut keys = vec!["IN_CLUD", "CLUD_EXE", running_process::ORIGINATOR_ENV_VAR];
    keys.push(UNSAFE_MODE_ENV);
    keys.extend(crate::gc::session_tmp::OVERRIDDEN_KEYS.iter().copied());
    keys.push(crate::shell::completion_guard::SUPPRESS_KEY);
    keys.push(crate::shell::nounset::BASH_ENV_KEY);
    keys.push(crate::shell::nounset::PREV_KEY);
    keys.push(crate::shell::cmd_gate::GATE_KEY);
    keys.push(crate::rm_tool::ROOTS_ENV);
    keys.push("CLUD_SESSION_ID");
    keys.push(crate::shim_session::RM_SHIM_DIR_KEY);
    keys.push(crate::shim_session::GH_SHIM_TARGET_KEY);
    keys.push(crate::shim_registry::GHD_REAL_GH_KEY);
    keys.push(crate::shim_session::GH_SHIM_ACTIVE_KEY);
    keys.push(crate::shim_session::GH_SHIM_FAIL_FAST_KEY);
    keys.push(crate::shim_session::GH_READ_BROKER_KEY);
    keys.push(crate::shim_session::GIT_SHIM_TARGET_KEY);
    keys.push(crate::shim_registry::ABI_KEY);
    keys.push(crate::shim_session::GIT_TERMINAL_PROMPT_KEY);
    keys.push(crate::shim_session::GIT_ASKPASS_KEY);
    keys.extend(WINDOWS_STDIO_KEYS.iter().copied());
    keys
}

/// Apply every child-env policy layer to `base`: inject tracking vars,
/// and — when `windows_stdio` is set — force UTF-8 for any Python helper
/// the agent shells out to (Codex / Claude tool scripts, MCP servers,
/// install probes …) so output doesn't mojibake against the user's OEM
/// codepage. Paired with the `chcp 65001` prefix in
/// `platform::subprocess::batch_shell_command` (issue #168). Node itself
/// respects the console codepage and needs no dedicated env var.
///
/// Then layers in, in order:
/// - Issue #509: points the backend agent's temp dir at ~/.clud/tmp so its
///   scatter of temp files lands where the daemon can reclaim them. Empty
///   when disabled (CLUD_SESSION_TMP=0) or the dir can't be created, in
///   which case the child keeps the OS temp dir.
/// - Issue #753: keeps Git-Bash completion functions out of the backend's
///   shell snapshot. Without this, every Bash tool call re-sources ~85
///   base64-decoded function definitions (~170 process spawns) before it
///   runs anything. See shell::completion_guard.
/// - Issue #1066: arms `set -u` in every non-interactive bash the backend
///   spawns, so an unset expansion aborts instead of silently becoming
///   empty. `push_or_replace` is what chains rather than duplicates: the
///   module has already stashed any inherited BASH_ENV under
///   CLUD_PREV_BASH_ENV, and the generated file sources it.
/// - Issue #1461: `CLUD_RM_ROOTS`, where `safe-rm` may delete: the launch
///   directory's git checkout and clud's
///   session temp dir. See rm_tool::session_roots_value.
/// - Finally, activates the `rm` shim session (`rm`, `safe-rm`).
///
/// This is now the ONE builder: [`child_env`] calls it with
/// `std::env::vars()`, and `daemon::io_helpers::child_env_from` calls it
/// with the daemon+client merged base — the merge #1209 introduced after
/// the Windows stdio pair had drifted into only one of the two builders.
pub fn apply_child_env_policy(base: Vec<(String, String)>) -> Vec<(String, String)> {
    apply_child_env_policy_with(base, crate::platform::host::WINDOWS)
}

/// Test seam — `windows_stdio` is `host::WINDOWS` in production. Passed in
/// rather than read so the Windows UTF-8 layer (the layer that actually
/// drifted) is assertable on a Linux CI lane.
pub fn apply_child_env_policy_with(
    base: Vec<(String, String)>,
    windows_stdio: bool,
) -> Vec<(String, String)> {
    apply_child_env_policy_with_nounset_opt_out(
        base,
        windows_stdio,
        crate::shell::nounset::is_opted_out(),
    )
}

/// Test seam for the nounset opt-out, which production obtains from the
/// process environment before the backend child is constructed.
fn apply_child_env_policy_with_nounset_opt_out(
    base: Vec<(String, String)>,
    windows_stdio: bool,
    nounset_opted_out: bool,
) -> Vec<(String, String)> {
    let originator_key = running_process::ORIGINATOR_ENV_VAR;

    let unsafe_mode = base
        .iter()
        .any(|(key, value)| key == UNSAFE_MODE_ENV && value == "1");

    let mut strip_keys: Vec<&str> = vec!["IN_CLUD", "CLUD_EXE", originator_key, UNSAFE_MODE_ENV];
    if windows_stdio {
        strip_keys.extend(WINDOWS_STDIO_KEYS.iter().copied());
    }

    let mut env: Vec<(String, String)> = base
        .into_iter()
        .filter(|(k, _)| !strip_keys.contains(&k.as_str()))
        .collect();

    env.push(("IN_CLUD".to_string(), "1".to_string()));
    if unsafe_mode {
        env.push((UNSAFE_MODE_ENV.to_string(), "1".to_string()));
    }
    // Codex does not provide Claude's session id. Mint one for each clud
    // launch so safe-mktemp and safe-rm share the creation ledger.
    let session = child_session_id(&env);
    push_or_replace(&mut env, "CLUD_SESSION_ID", &session);

    // Internal hooks and bundled instructions must use this executable, not
    // a second `clud` resolved from PATH (which may be a different uvx copy).
    // Strip any inherited value first so a nested launch cannot keep its
    // parent's executable when it is running a different version.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe) = exe.to_str() {
            env.push(("CLUD_EXE".to_string(), exe.to_string()));
        }
    }

    let originator_value = format!("CLUD:{}", std::process::id());
    env.push((originator_key.to_string(), originator_value));

    if windows_stdio {
        env.push(("PYTHONIOENCODING".to_string(), "utf-8".to_string()));
        env.push(("PYTHONUTF8".to_string(), "1".to_string()));
    }

    // Issue #509: point the backend agent's temp dir at ~/.clud/tmp so its
    // scatter of temp files lands where the daemon can reclaim them. Empty
    // when disabled (CLUD_SESSION_TMP=0) or the dir can't be created, in
    // which case the child keeps the OS temp dir.
    for (key, value) in crate::gc::session_tmp::env_overrides() {
        push_or_replace(&mut env, &key, &value);
    }
    // Issue #1485: the agent worktree root, a sibling of ~/.clud/tmp that
    // the mtime sweep never sees. Created idempotently; failure is harmless.
    let _ = crate::gc::worktree_root::ensure_worktree_root();

    // Issue #753: keep Git-Bash completion functions out of the backend's
    // shell snapshot. Without this, every Bash tool call re-sources ~85
    // base64-decoded function definitions (~170 process spawns) before it
    // runs anything. See shell::completion_guard.
    for (key, value) in crate::shell::completion_guard::env_overrides() {
        push_or_replace(&mut env, &key, &value);
    }

    // Issue #1066: arm `set -u` in every non-interactive bash the backend
    // spawns, so an unset expansion aborts instead of silently becoming empty.
    // `push_or_replace` is what chains rather than duplicates: the module has
    // already stashed any inherited BASH_ENV under CLUD_PREV_BASH_ENV, and the
    // generated file sources it. There is no daemon-side copy to keep in step
    // with any more; `daemon::io_helpers::child_env_from` calls this builder.
    // A nested clud process inherits its parent's generated BASH_ENV. The
    // explicit escape hatch must clear that inherited policy; otherwise Bash
    // still sources it even though this launch declined to install nounset.
    // Do not clear a user-owned BASH_ENV: it remains stock shell behavior.
    let nounset_opted_out = nounset_opted_out || unsafe_mode;
    let inherited_clud_nounset = nounset_opted_out
        && env
            .iter()
            .find(|(key, _)| key == crate::shell::nounset::BASH_ENV_KEY)
            .is_some_and(|(_, value)| crate::shell::nounset::is_clud_nounset_script(value));
    if inherited_clud_nounset {
        env.retain(|(key, _)| {
            key != crate::shell::nounset::BASH_ENV_KEY && key != crate::shell::nounset::PREV_KEY
        });
    }
    let nounset_overrides = (!nounset_opted_out)
        .then(crate::shell::nounset::env_overrides)
        .unwrap_or_default();
    for (key, value) in nounset_overrides {
        push_or_replace(&mut env, &key, &value);
    }

    // Issue #1067 step 3: opt-in (`CLUD_CMD_GATE_AUTO=1`) command gate, set
    // only when the wrapper resolves on this env's PATH. See shell::cmd_gate.
    if unsafe_mode {
        env.retain(|(key, _)| key != crate::shell::cmd_gate::GATE_KEY);
    } else {
        for (key, value) in crate::shell::cmd_gate::env_overrides(&env) {
            push_or_replace(&mut env, &key, &value);
        }
    }

    // Issue #1340: the session's deletion roots. The launch directory is the
    // base's `PWD` (the client's shell cwd on the daemon path, where this
    // process's own cwd is the daemon's), else this process's cwd. Always
    // replaced, so a nested launch gets its own checkout's roots.
    let launch_dir = env
        .iter()
        .find(|(key, _)| key == "PWD")
        .map(|(_, value)| std::path::PathBuf::from(value))
        .filter(|path| path.is_absolute() && path.is_dir())
        .or_else(|| std::env::current_dir().ok());
    match launch_dir.and_then(|dir| crate::rm_tool::session_roots_value(&dir)) {
        Some(roots) => push_or_replace(&mut env, crate::rm_tool::ROOTS_ENV, &roots),
        None => env.retain(|(key, _)| key != crate::rm_tool::ROOTS_ENV),
    }

    crate::shim_session::activate_rm(&mut env);
    if nounset_opted_out
        && env
            .iter()
            .any(|(key, _)| key == crate::shim_session::RM_SHIM_DIR_KEY)
    {
        let inherited = env
            .iter()
            .find(|(key, _)| key == crate::shell::nounset::BASH_ENV_KEY)
            .map(|(_, value)| value.clone());
        for (key, value) in crate::shell::nounset::rm_path_overrides(inherited) {
            push_or_replace(&mut env, &key, &value);
        }
    }
    env
}

fn child_session_id(env: &[(String, String)]) -> String {
    env.iter()
        .find(|(key, value)| key == "CLUD_SESSION_ID" && !value.is_empty())
        .map(|(_, value)| value.clone())
        .or_else(|| {
            env.iter()
                .find(|(key, value)| key == crate::grind_facts::SESSION_ENV && !value.is_empty())
                .map(|(_, value)| value.clone())
        })
        .unwrap_or_else(|| {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            format!(
                "clud-{}-{nanos}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
        })
}

/// Build the child environment for a foreground launch: the parent env
/// plus every policy layer in [`apply_child_env_policy`].
pub fn child_env() -> Vec<(String, String)> {
    apply_child_env_policy(std::env::vars().collect())
}

/// Wrap [`child_env`] with the per-backend shell policy from
/// `~/.clud/settings.json`. When `shell.disable_powershell` resolves true for
/// `backend` (issue #447):
///
/// - Both backends get `CLUD_DISABLE_POWERSHELL=1` so skills / CLAUDE.md
///   content can branch on it.
/// - Claude additionally gets `CLAUDE_CODE_USE_POWERSHELL_TOOL=0` (the
///   undocumented env-var kill-switch extracted from the bundled binary's
///   error strings) plus `CLAUDE_CODE_GIT_BASH_PATH` pointing at the lazily
///   resolved vendored Git Bash (see [`crate::shell::git_bash_resolver`]).
///   The PowerShell-tool toggle is set even if the resolver fails so Claude
///   surfaces a hard error instead of silently falling back to PowerShell.
/// - Codex has no equivalent env-var override (openai/codex#16717 is
///   closed). The Codex side ships as a PreToolUse hook in a follow-up PR;
///   here we just hand it `CLUD_DISABLE_POWERSHELL=1` for advisory use.
///
/// `Backend::Claude` is the case that actually changes behavior today.
pub fn child_env_for_backend(backend: Backend) -> Vec<(String, String)> {
    let home = crate::home::user_home();
    child_env_for_backend_at(backend, home.as_deref())
}

/// Test seam — accepts the home dir explicitly so the policy can be exercised
/// against a temp directory without mutating the real `~/.clud/settings.json`.
pub fn child_env_for_backend_at(backend: Backend, home: Option<&Path>) -> Vec<(String, String)> {
    let mut env = child_env();
    let Some(home) = home else {
        return env;
    };

    let disable = match clud_settings::load_shell_disable_powershell_for_backend_at(home, backend) {
        Ok(value) => value,
        Err(_) => return env,
    };
    if !disable {
        return env;
    }

    push_or_replace(&mut env, "CLUD_DISABLE_POWERSHELL", "1");

    if !matches!(backend, Backend::Claude) {
        return env;
    }

    // The PowerShell-tool toggle is set unconditionally — if the resolver
    // below fails, Claude will hard-fail visibly with "Git Bash was not
    // found and the PowerShell tool is disabled" rather than silently
    // resurrecting PowerShell.
    push_or_replace(&mut env, "CLAUDE_CODE_USE_POWERSHELL_TOOL", "0");

    match crate::shell::git_bash_resolver::resolve_or_fetch_git_bash(home) {
        Ok(path) => {
            push_or_replace(
                &mut env,
                "CLAUDE_CODE_GIT_BASH_PATH",
                &path.to_string_lossy(),
            );
        }
        Err(error) => {
            eprintln!(
                "[clud] shell.disable_powershell=true but vendored bash fetch failed: {error}. \
                 Set CLAUDE_CODE_GIT_BASH_PATH to a bash.exe already on disk to recover."
            );
        }
    }

    env
}

fn push_or_replace(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    env.retain(|(k, _)| k != key);
    env.push((key.to_string(), value.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn child_env_provides_stable_deletion_session_identity() {
        let with_claude = apply_child_env_policy_with_nounset_opt_out(
            vec![(
                crate::grind_facts::SESSION_ENV.into(),
                "claude-session".into(),
            )],
            false,
            true,
        );
        assert_eq!(
            value(&with_claude, "CLUD_SESSION_ID"),
            Some("claude-session")
        );

        let with_codex = apply_child_env_policy_with_nounset_opt_out(Vec::new(), false, true);
        let minted = value(&with_codex, "CLUD_SESSION_ID").unwrap().to_owned();
        assert!(minted.starts_with("clud-"));
        let nested = apply_child_env_policy_with_nounset_opt_out(with_codex, false, true);
        assert_eq!(value(&nested, "CLUD_SESSION_ID"), Some(minted.as_str()));
    }

    fn value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    type SharedRx = std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<Vec<u8>>>>;

    fn shared_channel() -> (std::sync::mpsc::Sender<Vec<u8>>, SharedRx) {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        (tx, std::sync::Arc::new(std::sync::Mutex::new(rx)))
    }

    /// #1360: a drop in iteration N > 0 must reach that iteration's child.
    #[test]
    fn dnd_chunks_reach_every_iteration() {
        let timeout = std::time::Duration::from_secs(1);
        let (tx, shared) = shared_channel();

        // Iteration 0.
        let (_other_tx0, other_rx0) = std::sync::mpsc::channel::<Vec<u8>>();
        let (leased, lease) = lease_shared_rx(&shared);
        let merged = merge_extra_rx(Some(leased), Some(other_rx0)).expect("merged rx");
        tx.send(b"a".to_vec()).expect("send in iteration 0");
        assert_eq!(
            merged.recv_timeout(timeout).expect("iteration 0 chunk"),
            b"a"
        );
        drop(merged);
        drop(lease);

        // The shared receiver must survive iteration 0.
        tx.send(b"b".to_vec())
            .expect("receiver must stay alive after iteration 0 ends");

        // Iteration 1.
        let (_other_tx1, other_rx1) = std::sync::mpsc::channel::<Vec<u8>>();
        let (leased, lease) = lease_shared_rx(&shared);
        let merged = merge_extra_rx(Some(leased), Some(other_rx1)).expect("merged rx");
        assert_eq!(
            merged.recv_timeout(timeout).expect("iteration 1 chunk"),
            b"b"
        );
        drop(merged);
        drop(lease);
    }

    #[test]
    fn lease_drop_stops_forwarder() {
        let timeout = std::time::Duration::from_secs(1);
        let (tx, shared) = shared_channel();

        let (first_rx, first_lease) = lease_shared_rx(&shared);
        drop(first_lease);
        // Sent after the first lease is gone: a dead forwarder must not
        // swallow it.
        tx.send(b"late".to_vec()).expect("shared receiver alive");
        assert!(first_rx.recv_timeout(timeout / 10).is_err());

        let (second_rx, second_lease) = lease_shared_rx(&shared);
        assert_eq!(second_rx.recv_timeout(timeout).expect("chunk"), b"late");
        drop(second_lease);
    }

    #[test]
    fn child_env_sets_the_rm_roots_from_the_launch_checkout() {
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        std::fs::create_dir_all(repo_path.join(".git")).unwrap();
        std::fs::create_dir_all(repo_path.join("src")).unwrap();
        let pwd = repo_path.join("src").to_string_lossy().into_owned();
        let env = apply_child_env_policy_with_nounset_opt_out(
            vec![
                ("PWD".to_string(), pwd),
                (
                    crate::rm_tool::ROOTS_ENV.to_string(),
                    "/inherited".to_string(),
                ),
            ],
            false,
            false,
        );
        let values: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k == crate::rm_tool::ROOTS_ENV)
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(values.len(), 1, "replaced, never duplicated");
        let roots: Vec<std::path::PathBuf> = std::env::split_paths(values[0]).collect();
        assert_eq!(
            roots[0], repo_path,
            "the checkout, not the subdir or the parent's value"
        );
    }

    #[test]
    fn child_env_pins_clud_exe_instead_of_inheriting_a_path_poison() {
        let env = apply_child_env_policy_with_nounset_opt_out(
            vec![("CLUD_EXE".to_string(), "poisoned-clud".to_string())],
            false,
            false,
        );
        let expected = std::env::current_exe().unwrap();
        assert_eq!(value(&env, "CLUD_EXE"), expected.to_str());
    }

    /// #1067 step 3 through the real builder and a real PATH lookup: opting
    /// in gates the session only once a `tap` executable is on its PATH.
    #[test]
    fn opted_in_session_is_gated_only_when_tap_is_on_path() {
        let bin = tempdir().unwrap();
        let path = bin.path().to_str().unwrap().to_string();
        let base = |path: &str| {
            vec![
                ("CLUD_CMD_GATE_AUTO".to_string(), "1".to_string()),
                ("PATH".to_string(), path.to_string()),
            ]
        };
        let env = apply_child_env_policy_with_nounset_opt_out(base(&path), false, true);
        assert_eq!(value(&env, "CLUD_CMD_GATE"), None, "no tap, no gate");

        let tap = bin.path().join(if crate::platform::host::WINDOWS {
            "tap.exe"
        } else {
            "tap"
        });
        std::fs::write(&tap, b"#!/bin/sh\n").unwrap();
        crate::platform::runner::mark_executable_for_test(&tap);
        let env = apply_child_env_policy_with_nounset_opt_out(base(&path), false, true);
        assert_eq!(value(&env, "CLUD_CMD_GATE"), Some("enforce"));

        // Without the opt-in, a tap on PATH changes nothing.
        let env = apply_child_env_policy_with_nounset_opt_out(
            vec![("PATH".to_string(), path.clone())],
            false,
            true,
        );
        assert_eq!(value(&env, "CLUD_CMD_GATE"), None);
    }

    #[test]
    fn nounset_opt_out_strips_an_inherited_clud_shim_at_the_policy_boundary() {
        let tmp = tempdir().unwrap();
        let inherited = crate::shell::nounset::env_overrides_at(tmp.path(), false, None)
            .into_iter()
            .find(|(key, _)| key == crate::shell::nounset::BASH_ENV_KEY)
            .map(|(_, value)| value)
            .expect("generated BASH_ENV");
        let env = apply_child_env_policy_with_nounset_opt_out(
            vec![
                (crate::shell::nounset::BASH_ENV_KEY.to_string(), inherited),
                (
                    crate::shell::nounset::PREV_KEY.to_string(),
                    "prior.sh".to_string(),
                ),
            ],
            false,
            true,
        );
        assert!(value(&env, crate::shell::nounset::BASH_ENV_KEY).is_none());
        assert!(value(&env, crate::shell::nounset::PREV_KEY).is_none());
    }

    #[test]
    fn nounset_opt_out_preserves_a_user_owned_bash_env() {
        let tmp = tempdir().unwrap();
        let user_owned = tmp.path().join("user-bash-env.sh");
        std::fs::write(&user_owned, "# user-owned\necho custom\n").unwrap();
        let user_owned = user_owned.display().to_string();
        let env = apply_child_env_policy_with_nounset_opt_out(
            vec![(
                crate::shell::nounset::BASH_ENV_KEY.to_string(),
                user_owned.clone(),
            )],
            false,
            true,
        );
        assert_eq!(
            value(&env, crate::shell::nounset::BASH_ENV_KEY),
            Some(user_owned.as_str())
        );
    }
}

pub fn get_terminal_size() -> (u16, u16) {
    let probe = terminal_size::terminal_size().map(|(w, h)| (w.0, h.0));
    resolve_terminal_size(probe)
}

fn display_verbose_command(command: &[String]) -> String {
    let Some((program, args)) = command.split_first() else {
        return String::new();
    };
    let mut rendered = Vec::with_capacity(command.len());
    rendered.push(display_program_name(program));
    rendered.extend(args.iter().cloned());
    rendered.join(" ")
}

fn display_program_name(program: &str) -> String {
    let tail = program.rsplit(['\\', '/']).next().unwrap_or(program);
    if tail.is_empty() {
        program.to_string()
    } else {
        tail.to_string()
    }
}

/// Translate a `(cols, rows)` probe result into a `(rows, cols)` size to hand
/// to the PTY. `None` means no real terminal — return a safe fallback.
/// 200 cols is wide enough that typical codex/claude output doesn't wrap
/// awkwardly, but stays within the range real terminal emulators actually
/// exercise — passing 32767 to ConPTY pushes layout math into corners that
/// trigger cursor drift in ratatui/Ink-based TUIs (issue #31, T3).
pub fn resolve_terminal_size(probe: Option<(u16, u16)>) -> (u16, u16) {
    match probe {
        Some((cols, rows)) => (rows, cols),
        None => (24, 200),
    }
}

/// Translate the final loop exit code into a `(summary, error)` pair
/// for `LoopSession::on_loop_end`. The mapping mirrors
/// `check_loop_markers`/`loop_unconverged_exit`:
///   - 0 → DONE
///   - 2 → iteration cap exhausted
///   - 3 → BLOCKED marker
///   - 130 → interrupt (Ctrl-C)
///   - anything else → "exit code N" + same as the error string
pub fn summarize_loop_outcome(exit_code: i32) -> (&'static str, Option<String>) {
    match exit_code {
        0 => ("DONE", None),
        2 => (
            "iteration cap exhausted",
            Some("iteration cap exhausted".to_string()),
        ),
        3 => ("BLOCKED", Some("blocked by agent".to_string())),
        130 => ("interrupted", Some("Interrupted by user".to_string())),
        _ => ("exit", Some(format!("exit code {exit_code}"))),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub fn run_plan_subprocess(
    plan: &command::LaunchPlan,
    job_tracker: Option<&crate::job_orphan_reaper::ForegroundJobTracker>,
    verbose: bool,
    interrupted: &AtomicBool,
    mut loop_session: Option<&mut loop_artifacts::LoopSession>,
    cpu_banner_cfg: cpu_banner::CpuBannerCfg,
    toast_cfg: crate::toast::ToastLaunchCfg,
) -> i32 {
    use std::path::PathBuf;

    // Issue #466: CPU-burn banner. Inert when cfg.enabled = false (no thread
    // spawned). Stopped explicitly below under a `cpu_banner_stop` stage on
    // the normal exit path; the early returns rely on `Drop`, which performs
    // the same bounded stop (#1172).
    //
    // #1189: subprocess mode cannot draw into the terminal the child owns.
    // Claude's injected status line shows the toast instead, fed by this
    // session's state file; other harnesses get no toast surface.
    let status_writer = crate::toast::launch::statusline_writer(plan, toast_cfg);
    let banner_sink = status_writer
        .clone()
        .map(crate::toast::ToastSink::StatusFile)
        .unwrap_or_default();
    crate::toast::launch::publish_demo_toast(&banner_sink);
    let mut cpu_banner = cpu_banner::BannerWatcher::spawn(cpu_banner_cfg, banner_sink);

    let statusline = toast_cfg
        .claude_statusline
        .then(|| {
            status_writer
                .as_deref()
                .and_then(crate::toast::launch::injection_for)
        })
        .flatten();
    let (runtime, _) = match crate::foreground_runtime::ForegroundRuntime::start_with_statusline(
        plan,
        child_env_for_backend(plan.backend),
        statusline.as_ref(),
        status_writer.as_ref(),
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            // #998: this is one of the failures clud names itself, and until
            // now the launch record kept only the bare exit code. Same text
            // the user just saw, so record and terminal agree.
            crate::launch_log::record_failure_reason(format_args!(
                "failed to start provider bridge: {error}"
            ));
            eprintln!("[clud] failed to start provider bridge: {error}");
            if verbose {
                verbose_log::log("[clud] provider bridge startup failed");
            }
            return 1;
        }
    };
    let mut last_exit = 0i32;
    let trace_enabled = stage_trace::stderr_enabled(verbose);

    for iteration in 0..plan.iterations {
        // Re-check the interrupted flag at the top of every iteration. A
        // Ctrl+C that fires between the previous child's reap and our next
        // spawn would otherwise be silently swallowed and we'd cheerfully
        // launch another codex run. 130 is the conventional SIGINT exit
        // code and mirrors what `ProcessOutcome::Interrupted` produces.
        if interrupted.load(Ordering::SeqCst) {
            if verbose {
                verbose_log::log("[clud] interrupted via Ctrl+C");
            }
            return 130;
        }

        let iter_num = iteration + 1;
        if plan.iterations > 1 {
            eprintln!("[clud] iteration {}/{}", iter_num, plan.iterations);
        }
        if let Some(s) = loop_session.as_deref_mut() {
            s.on_iteration_start(iter_num);
        }

        if verbose {
            verbose_log::log(format_args!(
                "[clud] exec (subprocess): {}",
                display_verbose_command(&plan.command)
            ));
        }

        let batch_wrapped = subprocess::argv_is_batch_wrapped(&plan.command);
        // Windows pipe-owning launches use `ManagedSubprocess`'s suspended
        // spawn: the child is assigned to its Job Object before it can run.
        // Console-attached launches and every non-Windows launch retain the
        // existing NativeProcess path and its Ctrl-C process-group behavior.
        let process = match runtime.spawn_subprocess(
            plan.command.clone(),
            plan.cwd.as_ref().map(PathBuf::from),
            plan.stream_json_progress,
            win_creation_flags::user_facing_backend_creationflags(),
        ) {
            Ok(process) => process,
            Err(e) => {
                eprintln!("[clud] failed to execute {}: {}", plan.command[0], e);
                if verbose {
                    verbose_log::log(format_args!("[clud] subprocess: start failed: {e}"));
                }
                if let Some(s) = loop_session.as_deref_mut() {
                    s.on_iteration_end(iter_num, 1, Some(format!("failed to start: {e}")));
                }
                return 1;
            }
        };
        if verbose {
            verbose_log::log("[clud] subprocess: started");
        }
        if let (Some(pid), Some(tracker)) = (process.pid(), job_tracker) {
            tracker.register_backend(pid, plan.backend.executable_name());
        }
        // Issue #541: wedge watchdog. Fresh per iteration (new pid each
        // time); dropped at the end of this loop body, which joins its
        // background thread promptly (see `WedgeWatchdog::stop`).
        let _wedge_watchdog = wedge_watchdog::WedgeWatchdog::spawn_for_pid(
            process.pid(),
            plan.backend.executable_name(),
        );

        // Issue #95: in stream-json mode we also accumulate the rendered
        // output so we can fall back to scanning for the
        // `<<<CLUD_LOOP_DONE: ...>>>` token if the agent skipped the
        // marker file. In inherited-stdio mode the child writes directly
        // to the user's terminal and we never see the bytes — the token
        // fallback is unavailable there.
        let mut captured_output = String::new();
        // #1168: a wedged `clud -p` on the Windows lanes left an empty stage
        // trace with the backend already finished, so the wait and the
        // per-iteration teardown get breadcrumbs. `child_wait` covers polling
        // the backend to exit (and, on Windows, its Job Object closing);
        // `child_teardown` covers joining the wedge watchdog and dropping the
        // process handle, which is where a descendant-held pipe would hold
        // us. An unmatched `begin` in the harness's rendering of the trace
        // names the one that did not come back.
        let wait_started =
            stage_trace::begin(trace_enabled, stage_trace::Phase::Launch, "child_wait");
        let exit_code = if plan.stream_json_progress {
            run_with_stream_json_renderer(
                &process,
                interrupted,
                &mut captured_output,
                batch_wrapped,
                iter_num,
            )
        } else {
            run_with_inherited_stdio(&process, interrupted, batch_wrapped)
        };
        stage_trace::done(
            trace_enabled,
            stage_trace::Phase::Launch,
            "child_wait",
            wait_started,
        );
        stage_trace::scoped(
            trace_enabled,
            stage_trace::Phase::Launch,
            "child_teardown",
            || {
                drop(_wedge_watchdog);
                drop(process);
            },
        );
        match exit_code {
            ProcessOutcome::Exited(code) => {
                last_exit = code;
                if verbose {
                    verbose_log::log(format_args!("[clud] subprocess: exited code {code}"));
                }
                if let Some(s) = loop_session.as_deref_mut() {
                    s.on_iteration_end(iter_num, code, None);
                }
                if last_exit != 0 && plan.iterations > 1 {
                    eprintln!(
                        "[clud] iteration {} failed with exit code {}",
                        iter_num, last_exit
                    );
                    note_silent_bridge(&runtime, last_exit);
                    return last_exit;
                }
            }
            ProcessOutcome::Interrupted => {
                if verbose {
                    verbose_log::log("[clud] interrupted via Ctrl+C");
                }
                if let Some(s) = loop_session.as_deref_mut() {
                    s.on_iteration_end(iter_num, 130, Some("Interrupted by user".to_string()));
                }
                return 130;
            }
            ProcessOutcome::Error => {
                if verbose {
                    verbose_log::log("[clud] subprocess: runner error");
                }
                if let Some(s) = loop_session.as_deref_mut() {
                    s.on_iteration_end(iter_num, 1, Some("runner error".to_string()));
                }
                return 1;
            }
        }

        if let Some(code) = check_loop_markers_with_output(plan, iter_num, &captured_output) {
            return code;
        }
    }

    if let Some(code) = loop_unconverged_exit(plan) {
        return code;
    }

    note_silent_bridge(&runtime, last_exit);
    stage_trace::scoped(
        trace_enabled,
        stage_trace::Phase::Launch,
        "runtime_drop",
        || drop(runtime),
    );
    // #1172: the #1168 trace isolated a Windows `clud -p` wedge to the window
    // after `runtime_drop`, where this was the only untraced step -- an
    // unbounded join on the banner thread. The stop is bounded now, and it
    // has a breadcrumb so the next stall names it or clears it.
    let outcome = stage_trace::scoped(
        trace_enabled,
        stage_trace::Phase::Launch,
        "cpu_banner_stop",
        || cpu_banner.stop(),
    );
    if outcome == cpu_banner::StopOutcome::Detached {
        stage_trace::trace(
            trace_enabled,
            "launch-stage note cpu_banner_stop detached the sampler thread mid-refresh",
        );
    }
    last_exit
}

/// Classify a launch that is exiting non-zero having never asked the bridge for
/// a turn (#998). No-op otherwise, so a reason a nearer failure already raised
/// is never overwritten with silence -- the failures that call
/// `record_failure_reason` today all `return` before reaching these sites.
fn note_silent_bridge(runtime: &crate::foreground_runtime::ForegroundRuntime, exit_code: i32) {
    if let Some(reason) =
        launch_log::silent_bridge_reason(runtime.bridge_turn_requests(), exit_code)
    {
        launch_log::record_failure_reason(reason);
    }
}

/// Outcome of one subprocess-mode iteration. Threaded through both the
/// inherited-stdio path and the stream-json renderer path so the outer loop
/// in `run_plan_subprocess` can stay uniform.
enum ProcessOutcome {
    Exited(i32),
    Interrupted,
    Error,
}

/// Tear down a backend child that has not exited yet because the user
/// just hit Ctrl+C.
///
/// **Goal: sub-100ms return to shell.** The legacy path did a synchronous
/// `kill_tree` + bounded `process.wait(2s)`, which produced the user-
/// reported up-to-4s Ctrl+C lag (kill_tree's sysinfo refresh plus the
/// blocking wait; in the stream-JSON path the post-loop `process.wait(2s)`
/// could add a second 2s window). We now hand the root PID to the always-
/// on daemon over a fire-and-forget IPC, then return immediately and let
/// the kill-on-close Job Object (running-process-core 3.4+) TerminateProcess
/// the direct child as our process exits. `TerminateProcess` is synchronous
/// and silent — no signal, so cmd.exe never gets a chance to print
/// `Terminate batch job (Y/N)?`. If the daemon isn't available we fall
/// back to the old synchronous path (with the cooperative Ctrl+Break +
/// `kill_tree` + bounded wait) so `--no-daemon` users still get cleanup.
fn teardown_interrupted_child(process: &subprocess::ManagedSubprocess, batch_wrapped: bool) {
    if let Some(pid) = process.pid() {
        crate::ctrl_c_track::record_forensics(Some(pid));
        match crate::daemon::default_state_dir() {
            Ok(state_dir) => {
                if crate::daemon::try_handoff_kill_to_daemon(
                    &state_dir,
                    &[pid],
                    Some("ctrl_c_subprocess"),
                ) {
                    // The daemon will kill_tree on a background thread; our
                    // job is just to get out of the way so the user gets
                    // their shell back. The Job Object reaps the direct
                    // child via TerminateProcess as we exit.
                    crate::ctrl_c_track::record_handoff(true, Some("ctrl_c_subprocess"));
                    return;
                }
                crate::ctrl_c_track::record_handoff(false, Some("daemon_unreachable"));
            }
            Err(_) => {
                crate::ctrl_c_track::record_handoff(false, Some("no_state_dir"));
            }
        }
        // Daemon-less fallback: keep the legacy synchronous behavior so
        // `--no-daemon` invocations still leave the process tree clean.
        if process_tree::should_cooperative_break(batch_wrapped) {
            let _ = process_tree::try_break_group(pid);
        }
        process_tree::kill_tree(pid);
    } else {
        crate::ctrl_c_track::record_handoff(false, Some("no_child_pid"));
        crate::ctrl_c_track::record_forensics(None);
    }
    let _ = process.kill();
    // Daemon-less fallback only: bounded wait so the legacy path doesn't
    // race itself. Skipped when we successfully handed off above — that's
    // the whole point of the fast return.
    let _ = process.wait(Some(std::time::Duration::from_secs(2)));
}

/// Inherited-stdio path: poll the child until it exits, kill on Ctrl+C.
/// This is the original `run_plan_subprocess` body, extracted unchanged so
/// the stream-json path can sit alongside it without duplicating the
/// non-streaming control flow.
#[path = "runner_execution.rs"]
mod runner_execution;
use runner_execution::{run_with_inherited_stdio, run_with_stream_json_renderer};
