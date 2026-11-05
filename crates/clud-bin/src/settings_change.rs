//! One user-reviewed settings operation. Approval never leaves this process.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Subcommand;
use serde_json::{json, Value};

#[path = "settings_change/gui.rs"]
mod gui;

const MAX_DOCUMENT: usize = 1024 * 1024;
const MAX_VALUE: usize = 64 * 1024;
const APPROVAL_LIFETIME: Duration = Duration::from_secs(600);

/// Values are proposals, never proof that the user has approved them.
#[derive(Clone, Subcommand)]
pub enum Mutation {
    /// Set one JSON pointer after native user approval; preserves deletion roots.
    Set {
        /// JSON pointer, for example /web_term/enabled.
        pointer: String,
        /// JSON value, for example false or a quoted string.
        #[arg(allow_hyphen_values = true)]
        value: String,
        /// Explain this specific requested change to the user.
        #[arg(long)]
        reason: String,
    },
    /// Replace deletion roots after separate, explicit native user approval.
    Roots {
        /// JSON array of objects containing an absolute path and non-empty reason.
        value: String,
        /// Explain this specific requested change to the user.
        #[arg(long)]
        reason: String,
    },
}

impl std::fmt::Debug for Mutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingsMutation")
            .field("protected_roots", &matches!(self, Self::Roots { .. }))
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum Failure {
    Cancelled,
    Unavailable,
    Refused(String),
    Storage(String),
    Changed,
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

#[derive(Clone)]
struct Change {
    pointer: String,
    before: Option<Value>,
    after: Option<Value>,
}

#[derive(Clone)]
struct Preview {
    target: PathBuf,
    reason: String,
    changes: Vec<Change>,
    roots: bool,
    created: Instant,
}

// Neither snapshots nor prepared operations have a serialization or Clone surface.
struct Snapshot {
    home: PathBuf,
    canonical_home: PathBuf,
    target: PathBuf,
    bytes: Option<Vec<u8>>,
    legacy: Option<Vec<u8>>,
    document: Value,
}

struct Prepared {
    snapshot: Snapshot,
    updated: Value,
    preview: Preview,
}

/// Utility dispatch before credentials, daemon setup, or any harness starts.
pub fn handle(args: &crate::args::Args) -> Option<i32> {
    let crate::args::Command::Settings {
        change: Some(change),
        ..
    } = args.command.as_ref()?
    else {
        return None;
    };
    if args.dry_run {
        eprintln!("[clud settings] Settings changes do not accept --dry-run; use `clud settings --list` to inspect settings");
        return Some(2);
    }
    Some(run(change))
}

pub(crate) fn run(change: &Mutation) -> i32 {
    let result = crate::home::user_home()
        .ok_or_else(|| Failure::Refused("Cannot resolve the user settings directory".into()))
        .and_then(|home| execute(&home, change, gui::approve));
    match result {
        Ok(path) => {
            println!("saved settings: {}", path.display());
            0
        }
        Err(Failure::Cancelled) => {
            println!("cancelled; settings unchanged");
            3
        }
        Err(error) => {
            let message = match &error {
                Failure::Unavailable => "Settings approval needs a native desktop window. No approval was obtained; settings unchanged. Open a desktop session and retry.".to_string(),
                Failure::Changed => "The settings target, contents, roots, or approval deadline changed. Review a new operation; settings unchanged.".to_string(),
                Failure::Refused(message) | Failure::Storage(message) => message.clone(),
                Failure::Cancelled => unreachable!(),
            };
            eprintln!("[clud settings] {message}");
            if matches!(error, Failure::Refused(_)) {
                2
            } else {
                1
            }
        }
    }
}

fn execute(
    home: &Path,
    change: &Mutation,
    approve: impl FnOnce(&Preview) -> Result<(), Failure>,
) -> Result<PathBuf, Failure> {
    let prepared = prepare(home, change)?;
    approve(&prepared.preview)?;
    commit(prepared)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, Failure> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_DOCUMENT as u64 => {
            return Err(Failure::Refused(
                "Settings document is too large to review safely".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    match fs::File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(MAX_DOCUMENT as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > MAX_DOCUMENT {
                return Err(Failure::Refused(
                    "Settings document grew beyond the review limit".into(),
                ));
            }
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn snapshot(home: &Path) -> Result<Snapshot, Failure> {
    let canonical_home = crate::path_norm::canonicalize_plain(home)?;
    let requested = super::settings_path_at(home);
    if fs::symlink_metadata(&requested).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(Failure::Refused(
            "Settings approval requires a regular settings file, not a symlink".into(),
        ));
    }
    let parent = requested.parent().ok_or(Failure::Changed)?;
    let canonical_parent = match crate::path_norm::canonicalize_plain(parent) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            canonical_home.join(super::CLUD_DIR_NAME)
        }
        Err(error) => return Err(error.into()),
    };
    let target = canonical_parent.join(super::SETTINGS_FILE_NAME);
    let bytes = read_optional(&target)?;
    let legacy = read_optional(&super::legacy_settings_path_at(home))?;
    let document = captured_document(&target, bytes.as_deref(), legacy.as_deref())?;
    Ok(Snapshot {
        home: home.to_path_buf(),
        canonical_home,
        target,
        bytes,
        legacy,
        document,
    })
}

fn captured_document(
    target: &Path,
    bytes: Option<&[u8]>,
    legacy: Option<&[u8]>,
) -> Result<Value, Failure> {
    let (bytes, legacy) = match (bytes, legacy) {
        (Some(bytes), _) => (bytes, false),
        (None, Some(bytes)) => (bytes, true),
        (None, None) => return Ok(json!({})),
    };
    let invalid = || {
        Failure::Refused(
            "Existing settings are invalid; repair them before requesting a change".into(),
        )
    };
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    if legacy {
        super::parse_legacy_toml_settings(target, text).map_err(|_| invalid())
    } else {
        super::parse_json_settings(target, text).map_err(|_| invalid())
    }
}

fn prepare(home: &Path, mutation: &Mutation) -> Result<Prepared, Failure> {
    let original = snapshot(home)?;
    let (updated, reason, roots) = build_update(&original.document, home, mutation)?;
    let mut changes = Vec::new();
    collect_changes("", Some(&original.document), Some(&updated), &mut changes);
    if changes.is_empty() {
        return Err(Failure::Refused(
            "The proposed operation does not change any settings".into(),
        ));
    }
    let preview_size: usize = changes
        .iter()
        .map(|change| {
            change
                .before
                .as_ref()
                .map_or(0, |value| value.to_string().len())
                + change
                    .after
                    .as_ref()
                    .map_or(0, |value| value.to_string().len())
        })
        .sum();
    if changes.len() > 64
        || preview_size > 2 * MAX_VALUE
        || serde_json::to_vec(&updated).map_or(true, |v| v.len() > MAX_DOCUMENT)
    {
        return Err(Failure::Refused(
            "The proposed change is too large to review safely".into(),
        ));
    }
    let preview = Preview {
        target: original.target.clone(),
        reason,
        changes,
        roots,
        created: Instant::now(),
    };
    Ok(Prepared {
        snapshot: original,
        updated,
        preview,
    })
}

fn build_update(
    original: &Value,
    home: &Path,
    mutation: &Mutation,
) -> Result<(Value, String, bool), Failure> {
    let (pointer, raw, reason, roots) = match mutation {
        Mutation::Set {
            pointer,
            value,
            reason,
        } => (pointer.as_str(), value, reason, false),
        Mutation::Roots { value, reason } => ("/safe_rm/extra_roots", value, reason, true),
    };
    if raw.len() > MAX_VALUE
        || reason.trim().is_empty()
        || reason.len() > 1024
        || reason.chars().any(char::is_control)
    {
        return Err(Failure::Refused(
            "Provide a bounded JSON value and a non-empty reason (at most 1024 bytes)".into(),
        ));
    }
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| Failure::Refused("The proposed settings value must be valid JSON".into()))?;
    let value = if roots {
        checked_roots(&value, home)?
    } else {
        value
    };
    let mut updated = original.clone();
    set_pointer(&mut updated, pointer, value)?;
    if !roots && original.pointer("/safe_rm/extra_roots") != updated.pointer("/safe_rm/extra_roots")
    {
        return Err(Failure::Refused("Ordinary settings changes must preserve safe_rm.extra_roots exactly; use `clud settings roots` for separate approval".into()));
    }
    validate_document(&updated)?;
    Ok((updated, reason.trim().to_string(), roots))
}

fn validate_document(document: &Value) -> Result<(), Failure> {
    super::launch_preferences_from_document(document)
        .map(|_| ())
        .map_err(|_| {
            Failure::Refused("The resulting settings fail validation; nothing was changed".into())
        })
}

fn pointer_tokens(pointer: &str) -> Result<Vec<String>, Failure> {
    if !pointer.starts_with('/') || pointer.len() > 512 {
        return Err(Failure::Refused(
            "Use a non-root JSON pointer such as /web_term/enabled".into(),
        ));
    }
    let mut tokens = Vec::new();
    for token in pointer[1..].split('/') {
        let mut decoded = String::new();
        let mut chars = token.chars();
        while let Some(character) = chars.next() {
            decoded.push(match character {
                '~' => match chars.next() {
                    Some('0') => '~',
                    Some('1') => '/',
                    _ => return Err(Failure::Refused("Invalid JSON pointer escape".into())),
                },
                c if c.is_control() => {
                    return Err(Failure::Refused(
                        "Control characters are not settings keys".into(),
                    ))
                }
                c => c,
            });
        }
        if decoded.is_empty() || tokens.len() >= 16 {
            return Err(Failure::Refused(
                "Settings keys must be non-empty and at most 16 levels deep".into(),
            ));
        }
        tokens.push(decoded);
    }
    Ok(tokens)
}

fn set_pointer(document: &mut Value, pointer: &str, value: Value) -> Result<(), Failure> {
    let tokens = pointer_tokens(pointer)?;
    let mut parent = document;
    for token in &tokens[..tokens.len() - 1] {
        parent = parent
            .as_object_mut()
            .ok_or_else(|| Failure::Refused("Settings key parents must be objects".into()))?
            .entry(token.clone())
            .or_insert_with(|| json!({}));
    }
    parent
        .as_object_mut()
        .ok_or_else(|| Failure::Refused("Settings key parents must be objects".into()))?
        .insert(tokens[tokens.len() - 1].clone(), value);
    Ok(())
}

fn checked_roots(value: &Value, home: &Path) -> Result<Value, Failure> {
    let entries = value
        .as_array()
        .filter(|entries| entries.len() <= 32)
        .ok_or_else(|| {
            Failure::Refused("Deletion roots must be a JSON array with at most 32 entries".into())
        })?;
    let mut roots: Vec<Value> = Vec::new();
    for entry in entries {
        let object = entry
            .as_object()
            .filter(|object| object.len() == 2)
            .ok_or_else(|| {
                Failure::Refused("Every deletion root needs only path and reason fields".into())
            })?;
        let path = object.get("path").and_then(Value::as_str);
        let reason = object.get("reason").and_then(Value::as_str);
        let (Some(path), Some(reason)) = (path, reason) else {
            return Err(Failure::Refused(
                "Every deletion root needs a string path and non-empty reason".into(),
            ));
        };
        let entry = super::SafeRmExtraRootEntry {
            path: path.into(),
            reason: reason.into(),
        };
        let facts = crate::rm_tool::extra_roots::probe(
            &entry,
            Some(home),
            crate::platform::rm_tool::effective_uid(),
        );
        let root = crate::rm_tool::extra_roots::verdict(&facts).map_err(Failure::Refused)?;
        let item = json!({"path": root.root, "reason": root.reason});
        if roots
            .iter()
            .any(|existing| existing["path"] == item["path"])
        {
            return Err(Failure::Refused(
                "Duplicate deletion roots are not a distinct authorization".into(),
            ));
        }
        roots.push(item);
    }
    Ok(Value::Array(roots))
}

fn collect_changes(
    pointer: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    out: &mut Vec<Change>,
) {
    if before == after {
        return;
    }
    if let (Some(Value::Object(before)), Some(Value::Object(after))) = (before, after) {
        let keys: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
        for key in keys {
            let escaped = key.replace('~', "~0").replace('/', "~1");
            collect_changes(
                &format!("{pointer}/{escaped}"),
                before.get(key),
                after.get(key),
                out,
            );
        }
    } else {
        out.push(Change {
            pointer: pointer.into(),
            before: before.cloned(),
            after: after.cloned(),
        });
    }
}

fn unchanged(current: &Snapshot, approved: &Snapshot) -> bool {
    current.canonical_home == approved.canonical_home
        && current.target == approved.target
        && current.bytes == approved.bytes
        && current.legacy == approved.legacy
}

fn commit(prepared: Prepared) -> Result<PathBuf, Failure> {
    commit_with_clock(prepared, Instant::now)
}

fn commit_with_clock(
    prepared: Prepared,
    mut now: impl FnMut() -> Instant,
) -> Result<PathBuf, Failure> {
    if now().duration_since(prepared.preview.created) > APPROVAL_LIFETIME {
        return Err(Failure::Changed);
    }
    let current = snapshot(&prepared.snapshot.home)?;
    if !unchanged(&current, &prepared.snapshot) {
        return Err(Failure::Changed);
    }
    let parent = prepared.snapshot.target.parent().ok_or(Failure::Changed)?;
    fs::create_dir_all(parent)?;
    let _lock = super::acquire_lock(&parent.join(super::LOCK_FILE_NAME))?;
    let current = snapshot(&prepared.snapshot.home)?;
    if !unchanged(&current, &prepared.snapshot) {
        return Err(Failure::Changed);
    }
    if prepared.preview.roots {
        let proposed = prepared
            .updated
            .pointer("/safe_rm/extra_roots")
            .ok_or(Failure::Changed)?;
        if checked_roots(proposed, &prepared.snapshot.home)? != *proposed {
            return Err(Failure::Changed);
        }
    }
    validate_document(&prepared.updated)?;
    // Lock contention and validation consume the same approval lifetime.
    if now().duration_since(prepared.preview.created) > APPROVAL_LIFETIME {
        return Err(Failure::Changed);
    }
    append_audit(parent, &prepared.preview, "approved")?;
    if super::write_settings(&prepared.snapshot.target, &prepared.updated).is_err() {
        let _ = append_audit(parent, &prepared.preview, "write_failed");
        return Err(Failure::Storage(
            "Could not replace the approved settings document".into(),
        ));
    }
    if append_audit(parent, &prepared.preview, "saved").is_err() {
        eprintln!("[clud settings] Settings saved, but the completion audit could not be written");
    }
    Ok(prepared.snapshot.target)
}

fn append_audit(parent: &Path, preview: &Preview, outcome: &str) -> Result<(), Failure> {
    let path = parent.join("settings-audit.jsonl");
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(Failure::Refused(
            "Settings audit must be a regular file, not a symlink".into(),
        ));
    }
    let prior = read_optional(&path)?.unwrap_or_default();
    let prior = String::from_utf8(prior).map_err(|_| {
        Failure::Refused("Settings audit is invalid; repair it before requesting a change".into())
    })?;
    let mut lines: Vec<_> = prior.lines().rev().take(63).collect();
    lines.reverse();
    let event = json!({
        "time": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        "pid": std::process::id(), "target": preview.target,
        "keys": preview.changes.iter().map(|change| change.pointer.as_str()).collect::<Vec<_>>(),
        "protected_roots": preview.roots, "outcome": outcome,
    });
    let mut bytes = lines.join("\n");
    if !bytes.is_empty() {
        bytes.push('\n');
    }
    bytes.push_str(&event.to_string());
    bytes.push('\n');
    crate::fs_private::write_private_atomic(&path, bytes.as_bytes())?;
    Ok(())
}

#[cfg(test)]
#[path = "settings_change/tests.rs"]
mod tests;
