//! Compiling clud's hook set into a frontend's native configuration
//! (zackees/clud#977, #967 Phase 2b).
//!
//! clud does not write hook lines into anyone's settings file. It composes the
//! registration a frontend needs and hands it over as a **launch argument**,
//! which removes a whole class of hazards a file-writing design carries:
//! idempotence, read-modify-write lost updates, two writers fighting over one
//! file (#847), per-repo gitignore assumptions, and stale state left behind by
//! a killed session. See DD-049.
//!
//! ## What gets registered
//!
//! One dispatcher line per declared event, matching every tool:
//!
//! ```json
//! { "hooks": { "Stop": [ { "matcher": "*",
//!     "hooks": [ { "type": "command", "command": "clud-cmd-scan --event Stop" } ] } ] } }
//! ```
//!
//! Matching every tool is deliberate. The per-hook `matcher` a repo declares is
//! applied by clud when it dispatches, so narrowing here would silently drop
//! declarations: an installed line scoped to `Bash` can never deliver an
//! `Edit`-matched hook.
//!
//! ## Frontend support is not uniform
//!
//! **Claude** takes `--settings <file-or-json>`, an *additional* source that
//! merges with the settings files rather than replacing them, with hook entries
//! concatenating across levels. That is the whole mechanism.
//!
//! **Codex has no argument surface for hooks at all.** Its `-c key=value`
//! overrides values that would otherwise come from `config.toml`, and codex
//! hooks live in a separate `hooks.json`; no flag points at an alternate one.
//! So codex keeps the coverage the already-installed `clud-cmd-scan` PreToolUse
//! line gives it — which since #980 runs declared hooks too — and gets nothing
//! for other events, matching codex's own apparent single-event support. No
//! second codex writer is introduced.
//!
//! ## The `CwdChanged` backstop (#967 Phase 5)
//!
//! Alongside the declared events, clud registers one line of its own:
//! `clud-cmd-scan --event CwdChanged`, the reactive drift backstop that
//! catches a session cwd moved by an alias or a script, which the PreToolUse
//! scanner cannot see. It is registered only where the frontend supports the
//! event — the capability probe answers for the installed client, and a
//! negative answer (or a probe that cannot run) degrades silently to no line
//! at all, because the backstop is hygiene, never correctness (DD-064).

use serde_json::{json, Value};

use crate::clud_hooks::CludHooks;

/// The helper the compiled lines invoke. Same binary as the installed
/// PreToolUse guard; the event argument is what distinguishes the roles.
pub const DISPATCHER_BINARY: &str = "clud-cmd-scan";

/// Marks a session whose frontend has clud's dispatcher lines registered.
///
/// Without it the bare `clud-cmd-scan` line has to run declared hooks itself,
/// because before Phase 2b that line was the only thing that could. With it,
/// the bare line leaves declared hooks to the compiled lines and each hook
/// runs exactly once. Sessions clud did not launch never see the marker and
/// keep the old behavior.
pub const DISPATCH_ENV: &str = "CLUD_HOOK_DISPATCH";

/// The event a bare invocation serves; it needs no compiled line to exist,
/// but gets one anyway so its matcher is not whatever the user happened to
/// install.
pub const PRE_TOOL_USE: &str = "PreToolUse";

/// clud's own reactive drift backstop event (zackees/clud#967 Phase 5). The
/// harness fires it whenever the session cwd changes, including changes the
/// PreToolUse scanner never sees — an alias or a script that chdirs.
pub const CWD_CHANGED_EVENT: &str = "CwdChanged";

/// Claude file-tool denies for the user settings file (#1668, DD-137): it
/// holds the `safe_rm.extra_roots` override. Raw edits remain refused;
/// controlled settings proposals require a separate native user decision.
/// Claude Code's `Edit(path)` rule covers every file-editing tool, Write
/// included; a `Write(path)` rule never matches and draws a startup warning.
pub const SETTINGS_FILE_DENIES: &[&str] = &["Edit(~/.clud/settings.json)"];

/// Settings consent remains required when session safety policy is disabled.
#[must_use]
pub fn settings_authority_fragment() -> Value {
    json!({
        "permissions": { "deny": SETTINGS_FILE_DENIES },
        "hooks": {
            "PreToolUse": [{
                "matcher": guarded_tools_matcher(),
                "hooks": [{ "type": "command", "command": DISPATCHER_BINARY }]
            }]
        }
    })
}

#[must_use]
pub fn deletion_safety_fragment() -> Value {
    let mut deny = crate::deletion_rules::generated().claude_denies;
    deny.extend(SETTINGS_FILE_DENIES.iter().map(|rule| (*rule).to_string()));
    json!({
        "permissions": { "deny": deny },
        "hooks": {
            "PreToolUse": [{
                "matcher": guarded_tools_matcher(),
                "hooks": [{ "type": "command", "command": DISPATCHER_BINARY }]
            }],
            // #1984: the user's prompt may allow worktrees for the session.
            "UserPromptSubmit": [{
                "hooks": [{
                    "type": "command",
                    "command": dispatcher_command(crate::worktree_consent::USER_PROMPT_SUBMIT_EVENT),
                }]
            }],
            "PostToolUse": [{
                "matcher": "Bash|Workflow|CronCreate|CronDelete|ScheduleWakeup",
                "hooks": [{ "type": "command", "command": dispatcher_command("PostToolUse") }]
            }],
            "PostToolUseFailure": [{
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": dispatcher_command("PostToolUseFailure") }]
            }],
            "Stop": [{
                "hooks": [{ "type": "command", "command": dispatcher_command("Stop") }]
            }],
            "SessionEnd": [{
                "hooks": [{ "type": "command", "command": dispatcher_command("SessionEnd") }]
            }]
        }
    })
}

/// Every tool a scanner rule inspects. The harness filters by matcher before
/// the hook runs, so a rule for a tool missing here never fires: until
/// #1954 the matcher was `Bash` alone, and the `/loop` stop guard, the grind
/// write/agent/question caps, the recursive-agent guard and the Monitor wait
/// refusal were dead in every repo without its own catch-all matcher.
/// `block_bad_cmd`'s `every_rule_tool_reaches_the_dispatcher` keeps this in
/// step with the rules.
pub const GUARDED_TOOLS: &[&str] = &[
    "EnterPlanMode",
    "Bash",
    // #2233: the unchanged-re-read guard reads `tool_input` only, so the
    // harness must route `Read` here for it to fire at all.
    "Read",
    "PowerShell",
    "ScheduleWakeup",
    "Monitor",
    "Agent",
    "EnterWorktree",
    "Workflow",
    "StructuredOutput",
    "AskUserQuestion",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "exec_command",
    "shell_command",
    "apply_patch",
    "spawn_agent",
];

/// clud's `WorktreeCreate` / `WorktreeRemove` lines (#1962, #1971): Claude
/// Code's own worktrees are built under `~/.clud/worktrees` instead of
/// `.claude/worktrees/` (`claude_worktree_hook`). Passed only to a Claude
/// Code that knows these events (2.1.50+); older ones ignored a settings
/// document that named an unknown event, which would also drop the guards.
#[must_use]
pub fn worktree_hooks_fragment() -> Value {
    let mut events = serde_json::Map::new();
    for event in [
        crate::claude_worktree_hook::WORKTREE_CREATE_EVENT,
        crate::claude_worktree_hook::WORKTREE_REMOVE_EVENT,
    ] {
        events.insert(
            event.to_string(),
            json!([{ "hooks": [{ "type": "command", "command": dispatcher_command(event) }] }]),
        );
    }
    json!({ "hooks": Value::Object(events) })
}

/// [`GUARDED_TOOLS`] as one matcher; the harness reads it as an anchored
/// regex over the tool name.
#[must_use]
pub fn guarded_tools_matcher() -> String {
    GUARDED_TOOLS.join("|")
}

/// The command string a compiled line runs for `event`.
#[must_use]
pub fn dispatcher_command(event: &str) -> String {
    format!("{DISPATCHER_BINARY} --event {event}")
}

/// Build the Claude settings fragment registering clud's dispatcher for every
/// event `hooks` declares, plus clud's own `CwdChanged` backstop line when
/// `cwd_changed_supported` says the frontend can fire the event — the answer
/// of the version probe in `backend_bootstrap.rs`, threaded through
/// `foreground_runtime.rs`.
///
/// `None` when the repo declares nothing, which is the signal not to pass
/// `--settings` at all — a repo that has not opted in should see a launch
/// identical to the one it saw before this feature existed.
#[must_use]
pub fn claude_settings_fragment(hooks: &CludHooks, cwd_changed_supported: bool) -> Option<Value> {
    let mut events = serde_json::Map::new();
    for event in hooks.events() {
        events.insert(
            event.to_string(),
            json!([{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": dispatcher_command(event),
                }],
            }]),
        );
    }
    if cwd_changed_supported {
        events.insert(CWD_CHANGED_EVENT.to_string(), cwd_changed_registration());
    }
    if events.is_empty() {
        return None;
    }
    Some(json!({ "hooks": Value::Object(events) }))
}

/// The registration for clud's own `CwdChanged` backstop line.
///
/// The harness ignores `matcher` on `CwdChanged` (it fires on every directory
/// change), but the fragment's other lines carry one and this keeps the shape
/// uniform. A repo that declares `CwdChanged` itself gets the same single
/// line either way — the map insert is idempotent, and one dispatcher line
/// per event is all the frontend needs.
#[must_use]
fn cwd_changed_registration() -> Value {
    json!([{
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": dispatcher_command(CWD_CHANGED_EVENT),
        }],
    }])
}

/// Merge `overlay`'s hook entries into `base`, concatenating per event.
///
/// Mirrors how the harness itself layers hooks — entries add rather than
/// replace — so clud's registration cannot silently displace a document it is
/// merging into, whether that is the bridge's lifecycle hooks or a settings
/// file the user passed on the command line.
pub fn merge_hook_settings(base: &mut Value, overlay: &Value) -> Result<(), String> {
    let Some(overlay_events) = overlay.get("hooks").and_then(Value::as_object) else {
        return Ok(());
    };
    let base_root = base
        .as_object_mut()
        .ok_or_else(|| "settings must be a JSON object".to_string())?;
    let base_events = base_root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| "settings `hooks` must be a JSON object".to_string())?;

    for (event, entries) in overlay_events {
        let entries = entries
            .as_array()
            .ok_or_else(|| format!("settings hook event {event} must be a JSON array"))?;
        let existing = base_events
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("settings hook event {event} must be a JSON array"))?;
        existing.extend(entries.iter().cloned());
    }
    Ok(())
}

#[cfg(test)]
#[path = "clud_hooks_compile_tests.rs"]
mod clud_hooks_compile_tests;
