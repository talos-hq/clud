//! Verification-to-edit ratio watchdog (#1988, #2122).
//!
//! An agent can spend most of a session re-running workspace-wide lint,
//! release builds and full test suites after one-line edits, and nothing
//! tells it so. This hook classifies each PreToolUse call as an edit, a
//! verification (lint, build, test, CI replay) or neither, keeps the last
//! [`WINDOW`] classified calls per session, and adds one advisory note to the
//! agent's context when any of three signals trips:
//!
//! * **count:** verifications outnumber edits by more than the ratio;
//! * **wall time (#2122):** verification holds at least [`DEFAULT_SHARE`]% of
//!   the session's Bash wall time. PreToolUse sees no durations, so each Bash
//!   call's start is recorded under its `tool_use_id` and the PostToolUse /
//!   PostToolUseFailure for the same id adds the elapsed time;
//! * **modifiers (#2122):** [`MODIFIER_REPEATS`] `--workspace`/`--all`/
//!   `--release` runs in a row, each after edits to one package only.
//!
//! After a note it stays quiet for another full window. If the pattern
//! continues after a note, the next workspace-wide lint is denied **once**
//! (escalation); repeating the command runs it. `clud session stats` prints
//! the per-kind breakdown the ledger keeps.
//!
//! **Fail open**, like the repeat guard (#1674): a missing session id or any
//! state error means no note and no deny. State holds class letters, kind
//! totals, a few package names and pending call ids; never command text.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[path = "block_bad_cmd_verify_python.rs"]
mod python_write;

use super::block_bad_cmd_repeat::{prune, write_fast};

/// Classified calls kept per session.
pub(super) const WINDOW: usize = 30;
/// Verification calls in the window before the ratio is judged at all.
pub(super) const MIN_VERIFY: usize = 10;
/// Default ratio: more than this many verifications per edit trips the note.
pub(super) const DEFAULT_RATIO: u32 = 3;
/// Session-wide ratio override; `0` disables the watchdog.
pub(super) const RATIO_ENV: &str = "CLUD_VERIFY_EDIT_RATIO";
/// Default verification share of Bash wall time, in percent (#1988 proposes ~70).
pub(super) const DEFAULT_SHARE: u32 = 70;
/// Wall-time share override in percent; `0` disables only that signal.
pub(super) const SHARE_ENV: &str = "CLUD_VERIFY_WALL_SHARE";
/// Bash wall time measured before the share is judged at all.
pub(super) const MIN_SHARE_MS: u64 = 10 * 60 * 1000;
/// Wide runs in a row after single-package edits that trip the modifier note.
pub(super) const MODIFIER_REPEATS: u32 = 3;
/// Calls awaiting their PostToolUse; older ones are dropped first.
const MAX_PENDING: usize = 16;
/// Recent call ids remembered for redelivery dedupe.
const MAX_SEEN: usize = 32;
/// A pending call older than this never reports a duration.
const PENDING_TTL_MS: u64 = 60 * 60 * 1000;
const DIR_NAME: &str = "verify-ratio";
/// Sibling of [`DIR_NAME`] so ledger pruning never touches lock files.
const LOCK_DIR_NAME: &str = "verify-ratio-locks";
const HASH_DOMAIN: &str = "clud-verify-ratio-v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Class {
    Edit,
    Verify,
}

impl Class {
    fn letter(self) -> char {
        match self {
            Self::Edit => 'e',
            Self::Verify => 'v',
        }
    }
}

/// What one call is, for the stats breakdown. `Other` is an unclassified
/// Bash call: it counts toward Bash wall time but not the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Kind {
    Edit,
    Lint,
    Build,
    Test,
    Ci,
    Other,
}

impl Kind {
    pub(super) const ALL: [Self; 6] = [
        Self::Edit,
        Self::Lint,
        Self::Build,
        Self::Test,
        Self::Ci,
        Self::Other,
    ];

    fn class(self) -> Option<Class> {
        match self {
            Self::Edit => Some(Class::Edit),
            Self::Lint | Self::Build | Self::Test | Self::Ci => Some(Class::Verify),
            Self::Other => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Lint => "lint",
            Self::Build => "build",
            Self::Test => "test",
            Self::Ci => "ci",
            Self::Other => "other",
        }
    }
}

const EDIT_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit", "apply_patch"];
const SHELL_EDITS: &[&str] = &[
    "sed -i",
    "perl -pi",
    "perl -i",
    "apply_patch",
    "git apply",
    "git mv",
];
const LINT_WORDS: &[&str] = &[
    "clippy",
    "dylint",
    "cargo fmt",
    "ruff",
    "eslint",
    "mypy",
    "pyright",
    "npm run lint",
    "./lint",
    "bash lint",
    "ci/lint.py",
];
const BUILD_WORDS: &[&str] = &["cargo build", "cargo check"];
const TEST_WORDS: &[&str] = &[
    "cargo test",
    "cargo nextest",
    "pytest",
    "npm test",
    "./test",
    "bash test",
];
const CI_WORDS: &[&str] = &["bosn ci run", "clud ci", "local-gate run"];
/// Flags that widen a check past the package that changed.
const WIDE_FLAGS: &[&str] = &["--workspace", "--all", "--release"];
/// Commands that are workspace-wide lint by themselves.
const FULL_GATE_WORDS: &[&str] = &["./lint", "bash lint", "ci/lint.py", "local-gate run"];

/// The kind of one call, from its tool name and shell command.
#[cfg(test)]
pub(super) fn classify_kind(tool_name: &str, command: &str) -> Option<Kind> {
    classify_kind_at(tool_name, command, None)
}

fn classify_kind_at(tool_name: &str, command: &str, cwd: Option<&Path>) -> Option<Kind> {
    if EDIT_TOOLS.contains(&tool_name) {
        return Some(Kind::Edit);
    }
    if command.trim().is_empty() {
        return None;
    }
    let text = command.to_ascii_lowercase();
    // An edit chained with a build counts as the edit: the agent did change
    // something, and #1988 asks for them to be split, not punished twice.
    if SHELL_EDITS.iter().any(|word| text.contains(word))
        || cwd.is_some_and(|cwd| python_write::writes_source(command, cwd))
    {
        return Some(Kind::Edit);
    }
    let text = runner_text(command);
    let has = |words: &[&str]| words.iter().any(|word| text.contains(word));
    // CI replays run everything, so they win over the words they contain.
    let kind = if has(CI_WORDS) {
        Kind::Ci
    } else if has(LINT_WORDS) {
        Kind::Lint
    } else if has(TEST_WORDS) {
        Kind::Test
    } else if has(BUILD_WORDS) {
        Kind::Build
    } else {
        Kind::Other
    };
    Some(kind)
}

/// The class of one call, from its tool name and shell command.
#[cfg(test)]
pub(super) fn classify(tool_name: &str, command: &str) -> Option<Class> {
    classify_kind(tool_name, command).and_then(Kind::class)
}

/// Programs whose arguments are text to print or post, never a check (#2470).
const TEXT_PROGRAMS: &[&str] = &["printf", "echo", "gh", "cat"];

/// The lowercased command without its text-only segments: separators are
/// split outside quotes, and a segment run by a `TEXT_PROGRAMS` entry is
/// dropped, so `printf 'bash lint passed' | gh issue comment` is not a lint.
fn runner_text(command: &str) -> String {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in command.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            (None, '|' | '&' | ';' | '\n') => {
                segments.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    segments.push(current);
    segments
        .iter()
        .filter(|segment| {
            segment
                .split_whitespace()
                .next()
                .is_some_and(|program| !TEXT_PROGRAMS.contains(&program))
        })
        .map(|segment| segment.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ; ")
}

/// The command passes a flag that widens it past one package.
pub(super) fn is_wide(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|word| WIDE_FLAGS.contains(&word))
}

/// A workspace-wide lint: a lint or CI call that is wide or a full gate.
fn is_workspace_lint(kind: Kind, command: &str) -> bool {
    let text = runner_text(command);
    matches!(kind, Kind::Lint | Kind::Ci)
        && (is_wide(command)
            || FULL_GATE_WORDS.iter().any(|word| text.contains(word))
            || (text.contains("bosn ci run") && !text.contains("--job")))
}

/// The package an edited path belongs to: the directory after `crates/`,
/// `packages/` or `libs/`. `None` when the layout says nothing.
pub(super) fn package_of(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split(['/', '\\']).collect();
    parts
        .windows(3)
        .find_map(|w| matches!(w[0], "crates" | "packages" | "libs").then(|| w[1].to_string()))
}

/// Wall time and call count for one kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Total {
    pub calls: u64,
    pub ms: u64,
}

/// A Bash call whose PostToolUse has not arrived yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Pending {
    pub id: String,
    pub kind: Kind,
    pub start_ms: u64,
}

/// The persisted state: one letter per classified call, newest last, how
/// many classified calls remain before another note may be shown, and the
/// #2122 timing, modifier and escalation facts. Every new field defaults so
/// a #1988 ledger still loads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Ledger {
    pub window: String,
    pub quiet: u32,
    #[serde(default)]
    pub totals: BTreeMap<Kind, Total>,
    #[serde(default)]
    pub pending: Vec<Pending>,
    /// A note was shown; the next continuation may escalate.
    #[serde(default)]
    pub noted: bool,
    /// The one soft deny was spent.
    #[serde(default)]
    pub escalated: bool,
    /// Packages edited since the last verification (`?` for unknown).
    #[serde(default)]
    pub edit_pkgs: Vec<String>,
    #[serde(default)]
    pub last_verify: bool,
    /// Wide runs in a row, each after single-package edits.
    #[serde(default)]
    pub wide_single: u32,
    /// Recent `tool_use_id`s already counted, so a call delivered twice
    /// (two hook configs, a retried hook) is counted and advised once.
    #[serde(default)]
    pub seen: Vec<String>,
}

/// Counts in a window, for the note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Counts {
    pub edits: usize,
    pub verifies: usize,
}

/// One call as the decision table sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Call {
    pub kind: Kind,
    pub wide: bool,
    pub workspace_lint: bool,
    /// The edited file's package, for an edit.
    pub package: Option<String>,
    /// `tool_use_id` of a Bash call, so its PostToolUse can be timed.
    pub id: Option<String>,
    /// `tool_use_id` of any call (edits included), for redelivery dedupe.
    pub dedupe_id: Option<String>,
    pub now_ms: u64,
}

/// The limits in force; `ratio == 0` disables everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Limits {
    pub ratio: u32,
    pub share: u32,
}

/// What the hook does with the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Note(String),
    Deny(String),
}

/// The count decision over injected facts: the ledger to persist and, when
/// the ratio trips, the counts to report. `ratio == 0` disables it.
pub(super) fn decide(previous: &Ledger, class: Class, ratio: u32) -> (Ledger, Option<Counts>) {
    let mut ledger = previous.clone();
    ledger.window.push(class.letter());
    if ledger.window.len() > WINDOW {
        ledger.window = ledger.window[ledger.window.len() - WINDOW..].to_string();
    }
    let counts = window_counts(&ledger.window);
    ledger.quiet = previous.quiet.saturating_sub(1);
    let trips =
        ratio > 0 && class == Class::Verify && ledger.quiet == 0 && over_ratio(counts, ratio);
    if trips {
        ledger.quiet = u32::try_from(WINDOW).unwrap_or(u32::MAX);
        ledger.noted = true;
        return (ledger, Some(counts));
    }
    (ledger, None)
}

fn window_counts(window: &str) -> Counts {
    Counts {
        edits: window.chars().filter(|c| *c == 'e').count(),
        verifies: window.chars().filter(|c| *c == 'v').count(),
    }
}

fn over_ratio(counts: Counts, ratio: u32) -> bool {
    counts.verifies >= MIN_VERIFY && counts.verifies > counts.edits.max(1) * ratio as usize
}

/// Verification's share of measured Bash wall time, in percent, once enough
/// time has been measured.
pub(super) fn verify_share(totals: &BTreeMap<Kind, Total>) -> Option<u32> {
    let ms = |kinds: &[Kind]| -> u64 {
        kinds
            .iter()
            .filter_map(|kind| totals.get(kind))
            .map(|total| total.ms)
            .sum()
    };
    let verify = ms(&[Kind::Lint, Kind::Build, Kind::Test, Kind::Ci]);
    let bash = verify + ms(&[Kind::Other]);
    (bash >= MIN_SHARE_MS).then(|| u32::try_from(verify * 100 / bash).unwrap_or(100))
}

/// The whole decision table (#1988 + #2122) over injected facts.
pub(super) fn observe(previous: &Ledger, call: &Call, limits: Limits) -> (Ledger, Option<Verdict>) {
    if limits.ratio == 0 {
        return (previous.clone(), None);
    }
    let mut ledger = previous.clone();
    if let Some(id) = &call.dedupe_id {
        if ledger.seen.contains(id) {
            return (ledger, None);
        }
        if ledger.seen.len() >= MAX_SEEN {
            ledger.seen.remove(0);
        }
        ledger.seen.push(id.clone());
    }
    ledger.totals.entry(call.kind).or_default().calls += 1;
    if let Some(id) = &call.id {
        start_pending(&mut ledger, id, call.kind, call.now_ms);
    }
    let Some(class) = call.kind.class() else {
        return (ledger, None);
    };
    track_modifiers(&mut ledger, class, call);
    // Escalation is judged before the window moves: the pattern must have
    // continued since the note, and the deny is spent once per session.
    if class == Class::Verify && call.workspace_lint && ledger.noted && !ledger.escalated {
        let mut probe = ledger.window.clone();
        probe.push('v');
        if over_ratio(window_counts(&probe), limits.ratio) {
            let (mut next, _) = decide(&ledger, class, limits.ratio);
            next.escalated = true;
            return (next, Some(Verdict::Deny(deny_text(limits.ratio))));
        }
    }
    let (mut next, counts) = decide(&ledger, class, limits.ratio);
    if let Some(counts) = counts {
        return (next, Some(Verdict::Note(note_text(counts, limits.ratio))));
    }
    if class != Class::Verify || next.quiet > 0 {
        return (next, None);
    }
    if let Some(share) =
        verify_share(&next.totals).filter(|s| limits.share > 0 && *s >= limits.share)
    {
        quiet_after_note(&mut next);
        return (next, Some(Verdict::Note(share_text(share, limits.share))));
    }
    if next.wide_single >= MODIFIER_REPEATS {
        let package = next.edit_pkgs.first().cloned().unwrap_or_default();
        next.wide_single = 0;
        quiet_after_note(&mut next);
        return (next, Some(Verdict::Note(modifier_text(&package))));
    }
    (next, None)
}

fn quiet_after_note(ledger: &mut Ledger) {
    ledger.quiet = u32::try_from(WINDOW).unwrap_or(u32::MAX);
    ledger.noted = true;
}

fn track_modifiers(ledger: &mut Ledger, class: Class, call: &Call) {
    match class {
        Class::Edit => {
            if ledger.last_verify {
                ledger.edit_pkgs.clear();
                ledger.last_verify = false;
            }
            let package = call.package.clone().unwrap_or_else(|| "?".to_string());
            if !ledger.edit_pkgs.contains(&package) && ledger.edit_pkgs.len() < 4 {
                ledger.edit_pkgs.push(package);
            }
        }
        Class::Verify => {
            ledger.last_verify = true;
            if call.wide {
                let single = ledger.edit_pkgs.len() == 1 && ledger.edit_pkgs[0] != "?";
                ledger.wide_single = if single { ledger.wide_single + 1 } else { 0 };
            }
        }
    }
}

fn start_pending(ledger: &mut Ledger, id: &str, kind: Kind, now_ms: u64) {
    ledger
        .pending
        .retain(|p| p.id != id && now_ms.saturating_sub(p.start_ms) <= PENDING_TTL_MS);
    if ledger.pending.len() >= MAX_PENDING {
        ledger.pending.remove(0);
    }
    ledger.pending.push(Pending {
        id: id.to_string(),
        kind,
        start_ms: now_ms,
    });
}

/// A PostToolUse for `id`: add its elapsed time to its kind. An unknown id
/// changes nothing.
pub(super) fn finish(previous: &Ledger, id: &str, now_ms: u64) -> Option<Ledger> {
    let at = previous.pending.iter().position(|p| p.id == id)?;
    let mut ledger = previous.clone();
    let pending = ledger.pending.remove(at);
    ledger.totals.entry(pending.kind).or_default().ms += now_ms.saturating_sub(pending.start_ms);
    Some(ledger)
}

/// The ratio: [`RATIO_ENV`] wins, then `hooks.verify_edit_ratio` in clud's
/// global settings, then [`DEFAULT_RATIO`].
pub(super) fn ratio_from(env: Option<&str>, settings: Option<&Value>) -> u32 {
    limit_from(env, settings, "verify_edit_ratio", DEFAULT_RATIO)
}

/// The wall-time share: [`SHARE_ENV`], then `hooks.verify_wall_share`, then
/// [`DEFAULT_SHARE`].
pub(super) fn share_from(env: Option<&str>, settings: Option<&Value>) -> u32 {
    limit_from(env, settings, "verify_wall_share", DEFAULT_SHARE)
}

fn limit_from(env: Option<&str>, settings: Option<&Value>, key: &str, default: u32) -> u32 {
    if let Some(value) = env.and_then(|v| v.trim().parse::<u32>().ok()) {
        return value;
    }
    settings
        .and_then(|doc| doc.get("hooks"))
        .and_then(|hooks| hooks.get(key))
        .and_then(Value::as_u64)
        .map_or(default, |v| u32::try_from(v).unwrap_or(u32::MAX))
}

fn effective_limits() -> Limits {
    let settings = crate::clud_settings::home_dir_path()
        .ok()
        .and_then(|home| std::fs::read(crate::clud_settings::settings_path_at(&home)).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    Limits {
        ratio: ratio_from(std::env::var(RATIO_ENV).ok().as_deref(), settings.as_ref()),
        share: share_from(std::env::var(SHARE_ENV).ok().as_deref(), settings.as_ref()),
    }
}

fn session_file(state_dir: &Path, session_id: &str) -> PathBuf {
    let digest = Sha256::digest(format!("{HASH_DOMAIN}session\0{session_id}").as_bytes());
    let hash: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    state_dir.join(DIR_NAME).join(format!("{hash}.json"))
}

fn load(path: &Path) -> (Option<Vec<u8>>, Ledger) {
    let existing = std::fs::read(path).ok();
    let ledger = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Ledger>(bytes).ok())
        .unwrap_or_default();
    (existing, ledger)
}

fn store(path: &Path, ledger: &Ledger, fresh: bool) -> Option<()> {
    let bytes = serde_json::to_vec(ledger).ok()?;
    write_fast(path, &bytes, fresh).ok()?;
    if fresh {
        if let Some(dir) = path.parent() {
            prune(dir, SystemTime::now());
        }
    }
    Some(())
}

/// Lock file for a ledger. It lives in a sibling directory that [`prune`] never
/// scans: a lock is never rewritten, so its mtime stays old and the prune would
/// unlink a live session's lock while its ledger survives, letting two
/// processes lock different inodes.
fn lock_path(ledger: &Path) -> PathBuf {
    let root = ledger.parent().and_then(Path::parent).unwrap_or(ledger);
    root.join(LOCK_DIR_NAME)
        .join(ledger.file_name().unwrap_or_default())
        .with_extension("lock")
}

/// Cross-process advisory lock around one ledger read-modify-write, so two
/// concurrent hook invocations cannot both read the same cooldown and both
/// advise, or overwrite each other's update. Fail open: when the lock file
/// cannot be opened or locked the call proceeds unlocked.
struct SessionLock(Option<std::fs::File>);

impl SessionLock {
    fn acquire(path: &Path) -> Self {
        use fs4::fs_std::FileExt;
        let lock = lock_path(path);
        if let Some(dir) = lock.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock)
            .ok()
            .filter(|file| FileExt::lock_exclusive(file).is_ok());
        Self(file)
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        use fs4::fs_std::FileExt;
        if let Some(file) = &self.0 {
            let _ = FileExt::unlock(file);
        }
    }
}

/// Record `class` for the session in `state_dir`; the counts when a note is
/// due. Every failure returns `None`.
#[cfg(test)]
pub(super) fn observe_at(
    state_dir: &Path,
    session_id: &str,
    class: Class,
    ratio: u32,
) -> Option<Counts> {
    if ratio == 0 || session_id.is_empty() {
        return None;
    }
    let path = session_file(state_dir, session_id);
    let (existing, previous) = load(&path);
    let (ledger, note) = decide(&previous, class, ratio);
    store(&path, &ledger, existing.is_none())?;
    note
}

/// Run [`observe`] against the session's file in `state_dir`.
pub(super) fn observe_call_at(
    state_dir: &Path,
    session_id: &str,
    call: &Call,
    limits: Limits,
) -> Option<Verdict> {
    if limits.ratio == 0 || session_id.is_empty() {
        return None;
    }
    let path = session_file(state_dir, session_id);
    let _guard = SessionLock::acquire(&path);
    let (existing, previous) = load(&path);
    let (ledger, verdict) = observe(&previous, call, limits);
    store(&path, &ledger, existing.is_none())?;
    verdict
}

/// Run [`finish`] against the session's file in `state_dir`.
pub(super) fn finish_at(state_dir: &Path, session_id: &str, id: &str, now_ms: u64) -> Option<()> {
    if session_id.is_empty() || id.is_empty() {
        return None;
    }
    let path = session_file(state_dir, session_id);
    let _guard = SessionLock::acquire(&path);
    let (existing, previous) = load(&path);
    existing.as_ref()?;
    let ledger = finish(&previous, id, now_ms)?;
    store(&path, &ledger, false)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn tool_use_id(raw: &Value) -> Option<String> {
    raw.get("tool_use_id")
        .or_else(|| raw.get("toolUseId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn edited_path(tool_input: Option<&Value>) -> Option<String> {
    let input = tool_input?;
    ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn classify_payload(payload: &super::HookPayloadView) -> Option<Kind> {
    // Python recognition uses the POSIX shell grammar. Other shells retain
    // the existing ledger behavior rather than being interpreted as Bash.
    let python_cwd = (super::shell_dialect_for_payload(payload) == super::ShellDialect::Posix)
        .then_some(payload.cwd.as_path());
    classify_kind_at(&payload.tool_name, &payload.command, python_cwd)
}

/// PreToolUse entry: a note to add or a call to deny, or `None`.
pub(super) fn verdict(payload: &super::HookPayloadView, raw: &Value) -> Option<Verdict> {
    let session_id = payload.session_id.as_deref()?;
    let kind = classify_payload(payload)?;
    let limits = effective_limits();
    if limits.ratio == 0 {
        return None;
    }
    // Only shell calls are timed: the PostToolUse matcher covers Bash, and a
    // shell edit (`sed -i`) costs nothing worth attributing.
    let timed = kind != Kind::Edit && !payload.command.trim().is_empty();
    let call = Call {
        kind,
        wide: is_wide(&payload.command),
        workspace_lint: is_workspace_lint(kind, &payload.command),
        package: edited_path(payload.tool_input.as_ref()).and_then(|p| package_of(&p)),
        id: timed.then(|| tool_use_id(raw)).flatten(),
        dedupe_id: tool_use_id(raw),
        now_ms: now_ms(),
    };
    let state_dir = crate::daemon::default_state_dir().ok()?;
    observe_call_at(&state_dir, session_id, &call, limits)
}

/// PostToolUse / PostToolUseFailure entry: time the call that finished.
pub(super) fn record_finish(raw: &Value, session_id: Option<&str>) {
    let (Some(session_id), Some(id)) = (session_id, tool_use_id(raw)) else {
        return;
    };
    if let Ok(state_dir) = crate::daemon::default_state_dir() {
        let _ = finish_at(&state_dir, session_id, &id, now_ms());
    }
}

/// The `clud session stats` text for a session, or `None` when nothing was
/// recorded for it.
pub(super) fn stats_at(state_dir: &Path, session_id: &str) -> Option<String> {
    let (existing, ledger) = load(&session_file(state_dir, session_id));
    existing?;
    Some(stats_text(&ledger))
}

pub(super) fn stats_text(ledger: &Ledger) -> String {
    let bash_ms: u64 = Kind::ALL
        .iter()
        .filter(|kind| **kind != Kind::Edit)
        .filter_map(|kind| ledger.totals.get(kind))
        .map(|total| total.ms)
        .sum();
    let mut out = format!(
        "{:<6} {:>6} {:>10} {:>7}\n",
        "kind", "calls", "wall", "bash%"
    );
    for kind in Kind::ALL {
        let total = ledger.totals.get(&kind).copied().unwrap_or_default();
        let share = if kind == Kind::Edit || bash_ms == 0 {
            "-".to_string()
        } else {
            format!("{}%", total.ms * 100 / bash_ms)
        };
        let wall = if kind == Kind::Edit {
            "-".to_string()
        } else {
            format_ms(total.ms)
        };
        out.push_str(&format!(
            "{:<6} {:>6} {:>10} {:>7}\n",
            kind.name(),
            total.calls,
            wall,
            share
        ));
    }
    out.push_str(&format!("bash wall time: {}\n", format_ms(bash_ms)));
    if let Some(share) = verify_share(&ledger.totals) {
        out.push_str(&format!("verification share: {share}%\n"));
    }
    out
}

fn format_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{}s", secs, (ms % 1000) / 100)
    }
}

pub(super) fn note_text(counts: Counts, ratio: u32) -> String {
    format!(
        "[clud verify-ratio] Verification is crowding out editing: {} lint/build/test/CI calls \
         against {} edit attempts in the last {WINDOW} classified calls (limit {ratio} per edit, #1988). \
         Scope checks to what changed (`-p <crate>`, one test file), drop `--release` unless the \
         test needs optimized code, batch edits, and run the full gate once per commit or batch. \
         Keep edits and builds in separate tool calls. This is advice, not a refusal; set \
         {RATIO_ENV}=0 or `hooks.verify_edit_ratio` in clud's settings to change it.",
        counts.verifies, counts.edits
    )
}

fn share_text(share: u32, limit: u32) -> String {
    format!(
        "[clud verify-ratio] Lint/build/test/CI hold {share}% of this session's Bash wall time \
         (limit {limit}%, #2122). Scope checks to what changed and run the full gate once per \
         batch of edits; `clud session stats` shows the breakdown. This is advice, not a refusal; \
         set {SHARE_ENV}=0 or `hooks.verify_wall_share` in clud's settings to change it."
    )
}

fn modifier_text(package: &str) -> String {
    format!(
        "[clud verify-ratio] {MODIFIER_REPEATS} `--workspace`/`--all`/`--release` runs in a row, \
         each after edits to `{package}` only (#2122). Check that package alone (`-p {package}`) \
         and keep the wide run for the final gate. This is advice, not a refusal."
    )
}

fn deny_text(ratio: u32) -> String {
    format!(
        "[clud verify-ratio] Refused once: another workspace-wide lint while verification still \
         outnumbers edits more than {ratio} to 1 after the earlier note (#2122). Lint the package \
         you changed instead, or batch more edits first. This happens once per session: repeat \
         the same command to run it anyway."
    )
}

/// PreToolUse output that adds `note` to the agent's context without making
/// a permission decision, so the harness's normal prompt still applies.
pub(super) fn context_json(note: &str) -> Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": note,
        }
    })
}

#[cfg(test)]
#[path = "block_bad_cmd_verify_ratio_tests.rs"]
mod tests;
