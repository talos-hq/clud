//! `clud session-hook`: Claude lifecycle hook that keeps the index current (#922).
//!
//! clud registers this command for `SessionStart`, `PostCompact` and
//! `SessionEnd` on every Claude-harness launch, next to the user's own hooks
//! (`ForegroundRuntime::apply_session_history`). Claude pipes the hook payload
//! (`session_id`, `transcript_path`, `cwd`, ...) on stdin; the launch passes
//! its authoritative route on the command line. The hook upserts one index
//! entry and exits 0. It must never block or fail the session: every error is
//! swallowed.
//!
//! For a portable recovery launch, `SessionStart` also carries
//! `--recovery-file`: the hook prints the recovery context as
//! `hookSpecificOutput.additionalContext` (a file, not argv, so large
//! payloads survive Windows' argv limit), records the lineage, and deletes the
//! file.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::index::{self, canonical_cwd, Lineage, Route, SessionEntry};
use super::transcript::Transcript;

/// Route argument for a provider, as recorded at launch.
pub fn route_arg(provider: crate::backend::ModelProvider, unified: bool) -> &'static str {
    if unified {
        "unified"
    } else {
        provider.as_str()
    }
}

/// Parse a `--route` argument back into a [`Route`].
pub fn route_from_arg(arg: &str) -> Route {
    match arg {
        "claude" => Route::Claude,
        "codex" => Route::ViaClaude("Codex".into()),
        "deepseek" => Route::ViaClaude("DeepSeek".into()),
        "kimi" => Route::ViaClaude("Kimi".into()),
        "openrouter" => Route::ViaClaude("OpenRouter".into()),
        "unified" => Route::ViaClaude("Unified gateway".into()),
        other => Route::ViaClaude(other.to_string()),
    }
}

/// The lifecycle events clud registers the hook for.
pub const EVENTS: [&str; 3] = ["SessionStart", "PostCompact", "SessionEnd"];

/// The Claude settings fragment registering `clud session-hook` for every
/// event in [`EVENTS`]. Paths are double-quoted the way the injected status
/// line is (#1189): a path containing `"`, `$` or a backtick cannot be
/// quoted safely for every shell Claude Code may use, so no fragment is
/// produced and the launch simply runs without index updates.
///
/// On Windows the program path is left unquoted (#2199): Claude Code may run
/// hooks under PowerShell, which parses a leading quoted string as an
/// expression and rejects the arguments after it, while bash rejects the
/// `&` call operator PowerShell would need. A bare path parses in PowerShell,
/// Git Bash and cmd alike, so a Windows program path that needs quoting
/// (whitespace or any shell metacharacter) gets no fragment either.
pub fn settings_fragment(
    exe: &Path,
    state_dir: &Path,
    route: &str,
    recovery_file: Option<&Path>,
    windows: bool,
) -> Option<Value> {
    let render = |path: &Path| {
        let text = path.to_string_lossy().into_owned();
        if windows {
            crate::path_norm::slash_separators(&text)
        } else {
            text
        }
    };
    let exe = render(exe);
    let dir = render(state_dir);
    let recovery = recovery_file.map(render);
    let unsafe_text = |s: &str| s.contains('"') || s.contains('$') || s.contains('`');
    if unsafe_text(&exe)
        || unsafe_text(&dir)
        || recovery.as_deref().is_some_and(unsafe_text)
        || !route.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        || (windows && !bare_program_path(&exe))
    {
        return None;
    }
    let program = if windows { exe } else { format!("\"{exe}\"") };
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        let mut command =
            format!("{program} session-hook --event {event} --route {route} --state-dir \"{dir}\"");
        if let (Some(recovery), "SessionStart") = (&recovery, event) {
            command.push_str(&format!(" --recovery-file \"{recovery}\""));
        }
        hooks.insert(
            event.to_string(),
            serde_json::json!([{"hooks": [{"type": "command", "command": command}]}]),
        );
    }
    Some(serde_json::json!({ "hooks": hooks }))
}

/// Whether `path` reads as one program token, unquoted, in PowerShell, bash
/// and cmd.
fn bare_program_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ':' | '/' | '\\' | '.' | '_' | '-'))
}

/// A portable recovery payload handed to the new session's `SessionStart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryFile {
    pub context: String,
    pub lineage: Lineage,
}

pub struct HookArgs {
    pub event: String,
    pub route: String,
    pub state_dir: PathBuf,
    pub recovery_file: Option<PathBuf>,
}

/// Entry point for `clud session-hook`. Always returns 0.
pub fn run(args: &HookArgs) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    if let Some(output) = handle(args, &payload) {
        println!("{output}");
    }
    0
}

/// Process one hook payload; returns what to print on stdout, if anything.
pub fn handle(args: &HookArgs, payload: &Value) -> Option<String> {
    let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
    let recovery = args
        .recovery_file
        .as_deref()
        .filter(|_| args.event == "SessionStart")
        .and_then(take_recovery_file);
    // #1675: bind this launch's pending launch-context record to the session
    // id Claude just assigned. The token arrives through the env the launch
    // gave the child; failure-silent like everything else here.
    if args.event == "SessionStart" {
        if let (Ok(token), Some(session_id)) = (
            std::env::var(crate::launch_context::TOKEN_ENV),
            text("session_id"),
        ) {
            crate::launch_context::bind(&args.state_dir, &token, &session_id);
        }
    }
    if let (Some(session_id), Some(cwd)) = (text("session_id"), text("cwd")) {
        let transcript_path = text("transcript_path").map(PathBuf::from);
        let _ = record(
            &args.state_dir,
            &cwd,
            session_id,
            transcript_path,
            route_from_arg(&args.route),
            recovery.as_ref().map(|r| r.lineage.clone()),
            text("model"),
        );
    }
    // DD-230: a checkout parked on a branch that already landed is told so
    // before the first prompt, so the next task does not start on it.
    let nudge = (args.event == "SessionStart" && branch_nudge_enabled())
        .then(|| text("cwd"))
        .flatten()
        .and_then(|cwd| crate::gc::branches::session_nudge(Path::new(&cwd)));
    session_start_context(recovery.map(|r| r.context), nudge).map(|context| {
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": context,
            }
        })
        .to_string()
    })
}

/// Env: set to `0` to turn the SessionStart branch nudge off (DD-230).
pub const BRANCH_NUDGE_ENV: &str = "CLUD_BRANCH_NUDGE";

fn branch_nudge_enabled() -> bool {
    std::env::var(BRANCH_NUDGE_ENV).map_or(true, |v| v.trim() != "0")
}

/// The recovery context and the branch nudge, joined; `None` when neither.
fn session_start_context(recovery: Option<String>, nudge: Option<String>) -> Option<String> {
    match (recovery, nudge) {
        (Some(recovery), Some(nudge)) => Some(format!("{recovery}\n\n{nudge}")),
        (recovery, nudge) => recovery.or(nudge),
    }
}

/// Read and delete a recovery file. Deleting on read keeps the payload (which
/// may carry secrets) on disk only until the new session ingests it.
fn take_recovery_file(path: &Path) -> Option<RecoveryFile> {
    let bytes = std::fs::read(path).ok()?;
    let _ = std::fs::remove_file(path);
    serde_json::from_slice(&bytes).ok()
}

fn record(
    state_dir: &Path,
    cwd: &str,
    session_id: String,
    transcript_path: Option<PathBuf>,
    route: Route,
    lineage: Option<Lineage>,
    model: Option<String>,
) -> std::io::Result<()> {
    let transcript = transcript_path
        .as_deref()
        .and_then(|path| Transcript::load(path).ok());
    let entry = SessionEntry {
        session_id,
        transcript_path: transcript_path.unwrap_or_default(),
        route,
        route_inferred: false,
        model: model.or_else(|| transcript.as_ref().and_then(Transcript::model)),
        title: transcript
            .as_ref()
            .and_then(|t| t.title().or_else(|| t.preview(80))),
        last_activity: transcript
            .as_ref()
            .and_then(|t| t.last_timestamp().map(str::to_string))
            .or_else(|| Some(now_rfc3339())),
        compact_checkpoints: transcript.as_ref().map_or(0, |t| t.checkpoints().len()),
        lineage,
    };
    index::update(state_dir, &canonical_cwd(Path::new(cwd)), |index| {
        index.upsert(entry)
    })
}

fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    super::rfc3339_utc(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_history::transcript::tests::{assistant, user};

    fn args(state: &Path, event: &str, route: &str, recovery: Option<PathBuf>) -> HookArgs {
        HookArgs {
            event: event.into(),
            route: route.into(),
            state_dir: state.to_path_buf(),
            recovery_file: recovery,
        }
    }

    #[test]
    fn session_start_records_the_launch_route_authoritatively() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cwd = root.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("s-9.jsonl");
        std::fs::write(
            &transcript,
            [
                user("01", None, "do the thing"),
                assistant("02", "01", "done"),
            ]
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        let state = root.path().join("state");
        let payload = serde_json::json!({
            "session_id": "s-9",
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "startup",
        });
        assert!(handle(&args(&state, "SessionStart", "codex", None), &payload).is_none());
        let index = index::read(&state, &canonical_cwd(&cwd));
        let entry = index.find("s-9").unwrap();
        assert_eq!(entry.route, Route::ViaClaude("Codex".into()));
        assert!(!entry.route_inferred);
        assert_eq!(entry.title.as_deref(), Some("do the thing"));
    }

    #[test]
    fn a_recovery_file_becomes_additional_context_and_is_deleted() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cwd = root.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let state = root.path().join("state");
        let recovery_path = root.path().join("recovery.json");
        let recovery = RecoveryFile {
            context: "[CLUD RECOVERY CHECKPOINT]".into(),
            lineage: Lineage {
                recovered_from: "old".into(),
                checkpoint: Some("c1".into()),
                truncated_tokens: 42,
            },
        };
        std::fs::write(&recovery_path, serde_json::to_vec(&recovery).unwrap()).unwrap();
        let payload = serde_json::json!({"session_id": "new", "cwd": cwd});
        let output = handle(
            &args(
                &state,
                "SessionStart",
                "claude",
                Some(recovery_path.clone()),
            ),
            &payload,
        )
        .expect("recovery output");
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["additionalContext"],
            "[CLUD RECOVERY CHECKPOINT]"
        );
        assert!(
            !recovery_path.exists(),
            "recovery file must be removed after ingestion"
        );
        let index = index::read(&state, &canonical_cwd(&cwd));
        assert_eq!(index.find("new").unwrap().lineage, Some(recovery.lineage));
    }

    #[test]
    fn the_settings_fragment_registers_every_event_and_recovery_only_at_start() {
        let fragment = settings_fragment(
            Path::new("/opt/clud"),
            Path::new("/home/me/.clud/state"),
            "codex",
            Some(Path::new(
                "/home/me/.clud/state/session-history/recovery/x.json",
            )),
            false,
        )
        .unwrap();
        for event in EVENTS {
            let command = fragment["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(
                command.starts_with("\"/opt/clud\" session-hook --event "),
                "{command}"
            );
            assert!(command.contains("--route codex"), "{command}");
            assert_eq!(
                command.contains("--recovery-file"),
                event == "SessionStart",
                "{command}"
            );
        }
    }

    #[test]
    fn the_windows_fragment_leaves_the_program_unquoted_for_powershell() {
        // #2199: PowerShell rejected `"C:/.../clud.exe" session-hook ...`.
        let fragment = settings_fragment(
            Path::new("C:\\Users\\me\\.local\\bin\\clud.exe"),
            Path::new("C:\\Users\\me\\.clud\\state"),
            "claude",
            None,
            true,
        )
        .unwrap();
        for event in EVENTS {
            let command = fragment["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(
                command.starts_with("C:/Users/me/.local/bin/clud.exe session-hook --event "),
                "{command}"
            );
        }
    }

    #[test]
    fn a_windows_program_path_that_needs_quoting_produces_no_fragment() {
        let state = Path::new("C:/state");
        for exe in [
            "C:/Program Files/clud/clud.exe",
            "C:/a&b/clud.exe",
            "C:/a(b)/clud.exe",
        ] {
            assert!(
                settings_fragment(Path::new(exe), state, "claude", None, true).is_none(),
                "{exe}"
            );
        }
    }

    #[test]
    fn unquotable_paths_or_routes_produce_no_fragment() {
        let ok = Path::new("/opt/clud");
        let state = Path::new("/state");
        assert!(
            settings_fragment(Path::new("/o\"pt/clud"), state, "claude", None, false).is_none()
        );
        assert!(settings_fragment(ok, Path::new("/$HOME"), "claude", None, false).is_none());
        assert!(settings_fragment(ok, state, "claude; rm", None, false).is_none());
        assert!(settings_fragment(ok, state, "claude", Some(Path::new("/a`b")), false).is_none());
    }

    #[test]
    fn a_malformed_payload_is_ignored_without_output() {
        let state = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        assert!(handle(
            &args(state.path(), "SessionEnd", "claude", None),
            &Value::Null
        )
        .is_none());
    }

    #[test]
    fn route_args_round_trip_for_every_provider() {
        use crate::backend::ModelProvider;
        for provider in ModelProvider::ALL {
            let route = route_from_arg(route_arg(*provider, false));
            if *provider == ModelProvider::Claude {
                assert_eq!(route, Route::Claude);
            } else {
                assert!(matches!(route, Route::ViaClaude(_)), "{provider:?}");
            }
        }
        assert_eq!(
            route_from_arg(route_arg(ModelProvider::Claude, true)).label(),
            "Unified gateway via Claude"
        );
    }
}
