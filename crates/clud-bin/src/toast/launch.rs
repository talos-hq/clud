//! Per-launch toast wiring shared by the subprocess and PTY runners (#1189).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::backend::Backend;
use crate::command::LaunchPlan;

use super::compositor::Fallback;
use super::statusline::{state_path, StatusStateWriter};
use super::{Severity, Toast, ToastEvent, ToastLaunchCfg, ToastSink};

/// Manual-validation hook: `CLUD_TOAST_DEMO=<text>` shows a toast at launch,
/// so a tier can be checked in a real terminal without provoking a CPU
/// episode. Lives for `CLUD_TOAST_DEMO_SECS` seconds (default 20).
pub const DEMO_ENV: &str = "CLUD_TOAST_DEMO";
pub const DEMO_SECS_ENV: &str = "CLUD_TOAST_DEMO_SECS";
const DEMO_DEFAULT_SECS: u64 = 20;

/// The demo toast `env` asks for, if any.
pub fn demo_toast(env: &dyn Fn(&str) -> Option<String>, now: Instant) -> Option<ToastEvent> {
    let text = env(DEMO_ENV)?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let secs = env(DEMO_SECS_ENV)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEMO_DEFAULT_SECS);
    Some(ToastEvent::Show(
        Toast::new("demo", text, Severity::Info, now).expiring_after(Duration::from_secs(secs)),
    ))
}

/// Publish the demo toast from the process environment, if requested.
pub fn publish_demo_toast(sink: &ToastSink) {
    let env = |name: &str| std::env::var(name).ok();
    if let Some(event) = demo_toast(&env, Instant::now()) {
        sink.publish(event);
    }
}

/// The launch-wide state writer for toasts and bridge usage, or `None` when
/// toasts are disabled. Claude additionally exposes this file through its
/// injected `statusLine`; PTY compositors use the same in-memory writer for
/// every harness, so usage never depends on a Claude-only UI feature.
pub fn statusline_writer(plan: &LaunchPlan, cfg: ToastLaunchCfg) -> Option<Arc<StatusStateWriter>> {
    if !cfg.enabled {
        return None;
    }
    let state_dir = crate::daemon::default_state_dir().ok()?;
    Some(Arc::new(StatusStateWriter::new_with_provider(
        state_path(&state_dir, std::process::id()),
        plan.model_provider().as_str(),
    )))
}

/// What `ForegroundRuntime` needs to compose the `statusLine` setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatuslineInjection {
    pub exe: PathBuf,
    pub session_pid: u32,
    pub state_dir: PathBuf,
}

/// Derive the injection from the writer's `<state_dir>/toasts/<pid>.json`.
pub fn injection_for(writer: &StatusStateWriter) -> Option<StatuslineInjection> {
    injection_for_exe(writer, std::env::current_exe().ok()?)
}

fn injection_for_exe(writer: &StatusStateWriter, exe: PathBuf) -> Option<StatuslineInjection> {
    let path = writer.path();
    let state_dir = path.parent()?.parent()?.to_path_buf();
    let session_pid = path.file_stem()?.to_str()?.parse().ok()?;
    Some(StatuslineInjection {
        exe,
        session_pid,
        state_dir,
    })
}

/// The surface a PTY session uses when no in-grid tier applies: Claude's
/// status line when it was injected, the terminal title for every other
/// harness (clud strips the child's own titles in PTY mode, so it owns them).
pub fn fallback_for(plan: &LaunchPlan, writer: Option<&Arc<StatusStateWriter>>) -> Fallback {
    fallback_for_harness(plan.effective_harness(), writer)
}

/// [`fallback_for`] gated on the injection having really happened (#2395):
/// the status file is only read by a `clud statusline` Claude was told to run,
/// so when composing it failed the grid HUD must keep the CPU reading.
pub fn fallback_after_injection(
    plan: &LaunchPlan,
    writer: Option<&Arc<StatusStateWriter>>,
    injected: bool,
) -> Fallback {
    fallback_for_harness(plan.effective_harness(), writer.filter(|_| injected))
}

fn fallback_for_harness(harness: Backend, writer: Option<&Arc<StatusStateWriter>>) -> Fallback {
    match (harness, writer) {
        (Backend::Claude, Some(writer)) => Fallback::StatusFile(Arc::clone(writer)),
        (Backend::Claude, None) => Fallback::None,
        _ => Fallback::Title,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_toast_is_opt_in_and_expires() {
        let now = Instant::now();
        let none = |_: &str| None;
        assert!(demo_toast(&none, now).is_none());
        let blank = |name: &str| (name == DEMO_ENV).then(|| "   ".to_string());
        assert!(demo_toast(&blank, now).is_none());
        let set = |name: &str| match name {
            DEMO_ENV => Some("hello kitty".to_string()),
            DEMO_SECS_ENV => Some("5".to_string()),
            _ => None,
        };
        let Some(ToastEvent::Show(toast)) = demo_toast(&set, now) else {
            panic!("expected a demo toast");
        };
        assert_eq!(toast.text, "hello kitty");
        assert_eq!(toast.expires_at, Some(now + Duration::from_secs(5)));
        let bad_secs = |name: &str| match name {
            DEMO_ENV => Some("x".to_string()),
            DEMO_SECS_ENV => Some("soon".to_string()),
            _ => None,
        };
        let Some(ToastEvent::Show(toast)) = demo_toast(&bad_secs, now) else {
            panic!("expected a demo toast");
        };
        assert_eq!(
            toast.expires_at,
            Some(now + Duration::from_secs(DEMO_DEFAULT_SECS))
        );
    }

    #[test]
    fn the_injection_is_derived_from_the_state_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let writer = StatusStateWriter::new(state_path(dir.path(), 4321));
        let injection = injection_for_exe(&writer, PathBuf::from("/bin/clud")).unwrap();
        assert_eq!(injection.session_pid, 4321);
        assert_eq!(injection.state_dir, dir.path());
        assert_eq!(injection.exe, PathBuf::from("/bin/clud"));
    }

    #[test]
    fn a_failed_injection_drops_the_status_file_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let writer = Arc::new(StatusStateWriter::new(state_path(dir.path(), 1)));
        let plan = crate::foreground_runtime::tests::plan(
            crate::backend::ModelProvider::Claude,
            Backend::Claude,
        );
        assert!(matches!(
            fallback_after_injection(&plan, Some(&writer), true),
            Fallback::StatusFile(_)
        ));
        assert!(matches!(
            fallback_after_injection(&plan, Some(&writer), false),
            Fallback::None
        ));
    }

    #[test]
    fn claude_falls_back_to_its_status_line_and_codex_to_the_title() {
        let dir = tempfile::tempdir().unwrap();
        let writer = Arc::new(StatusStateWriter::new(state_path(dir.path(), 1)));
        assert!(matches!(
            fallback_for_harness(Backend::Claude, Some(&writer)),
            Fallback::StatusFile(_)
        ));
        assert!(matches!(
            fallback_for_harness(Backend::Claude, None),
            Fallback::None
        ));
        assert!(matches!(
            fallback_for_harness(Backend::Codex, None),
            Fallback::Title
        ));
    }

    #[test]
    fn the_hidden_statusline_subcommand_parses_instead_of_passing_through() {
        let raw: Vec<String> = [
            "clud",
            "statusline",
            "--session-pid",
            "77",
            "--state-dir",
            "/tmp/state",
            "--chain-b64",
            "ZWNobyBoaQ",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let args = crate::args::Args::parse_from_raw(raw);
        match args.command {
            Some(crate::args::Command::Statusline(options)) => {
                let crate::args::StatuslineArgs {
                    session_pid,
                    state_dir,
                    chain_b64,
                } = *options;
                assert_eq!(session_pid, 77);
                assert_eq!(state_dir, PathBuf::from("/tmp/state"));
                assert_eq!(chain_b64.as_deref(), Some("ZWNobyBoaQ"));
            }
            other => panic!("expected Statusline, got {other:?}"),
        }
        assert!(
            args.passthrough.is_empty(),
            "no statusline flag may leak to the backend"
        );
    }
}
