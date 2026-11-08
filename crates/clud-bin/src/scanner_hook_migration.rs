//! Consolidate existing Claude scanner registrations across settings layers.
//! An explicit catch-all registration supplies both policy and declared hooks;
//! the launch fragment defers to it instead of registering a second process.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::clud_hooks_compile::{
    dispatcher_command, is_plain_scanner_handler, SCANNER_EXIT_SUFFIX,
};

fn config_paths(repo: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut unique = BTreeSet::new();
    crate::block_bad_cmd_rollout::hook_config_paths(repo, home)
        .into_iter()
        .rev()
        .filter(|(path, _)| {
            path.parent()
                .and_then(Path::file_name)
                .is_some_and(|p| p == ".claude")
        })
        .map(|(path, scope)| {
            if let Some(home) =
                home.filter(|_| scope == crate::block_bad_cmd_rollout::ConfigScope::User)
            {
                crate::session_history::import::claude_config_dir(home).join("settings.json")
            } else {
                path
            }
        })
        .filter(|path| unique.insert(path.canonicalize().unwrap_or_else(|_| path.clone())))
        .collect()
}

/// Upgrade only existing plain managed handlers; no hook is added to an
/// installation that did not already have one. Unrelated handlers and fields
/// remain in their original files. Prefer the user's registration over a repo's.
pub fn migrate(repo: &Path, home: Option<&Path>) -> io::Result<()> {
    migrate_with_suffix(repo, home, BTreeSet::new())
}

fn migrate_with_suffix(
    repo: &Path,
    home: Option<&Path>,
    mut required: BTreeSet<String>,
) -> io::Result<()> {
    let lock_path = home
        .unwrap_or(repo)
        .join(".clud")
        .join(crate::codex_hook_normalize::LOCK_FILE_NAME);
    let _lock = crate::codex_hook_normalize::acquire_lock(&lock_path)?;
    let mut docs = Vec::new();
    for path in config_paths(repo, home) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        required.extend(suffixed_events(&doc));
        docs.push((path, doc));
    }
    let mut seen = BTreeSet::new();
    for (path, mut doc) in docs {
        let before = doc.clone();
        normalize(&mut doc, &mut seen, &required);
        if doc != before {
            let mut text = serde_json::to_string_pretty(&doc).map_err(io::Error::other)?;
            text.push('\n');
            write_config(&path, &text)?;
        }
    }
    Ok(())
}

fn write_config(path: &Path, text: &str) -> io::Result<()> {
    let destination = if path.exists() {
        path.canonicalize()?
    } else {
        path.to_path_buf()
    };
    let parent = destination.parent().expect("config has parent");
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = std::fs::metadata(&destination) {
        file.as_file().set_permissions(metadata.permissions())?;
    }
    file.write_all(text.as_bytes())?;
    file.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

fn suffixed_events(doc: &Value) -> BTreeSet<String> {
    let mut required = BTreeSet::new();
    if let Some(events) = doc.get("hooks").and_then(Value::as_object) {
        for (event, groups) in events {
            if groups.as_array().is_some_and(|groups| {
                groups.iter().any(|group| {
                    group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|handlers| {
                            handlers.iter().any(|handler| {
                                is_plain_scanner_handler(handler, event)
                                    && handler["command"]
                                        .as_str()
                                        .is_some_and(|c| c.ends_with(SCANNER_EXIT_SUFFIX))
                            })
                        })
                })
            }) {
                required.insert(event.clone());
            }
        }
    }
    required
}

fn normalize(doc: &mut Value, seen: &mut BTreeSet<String>, required: &BTreeSet<String>) {
    let Some(events) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for (event, groups) in events {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        let mut found = false;
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|handler| {
                    if is_plain_scanner_handler(handler, event) {
                        found = true;
                        false
                    } else {
                        true
                    }
                });
            }
        }
        if found {
            groups.retain(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_none_or(|h| !h.is_empty())
            });
            if seen.insert(event.clone()) {
                groups.push(json!({"matcher":"*", "hooks":[{
                    "type":"command", "command":format!("{}{}", dispatcher_command(event),
                        if required.contains(event) { SCANNER_EXIT_SUFFIX } else { "" })
                }]}));
            }
        }
    }
}

/// Events already served by a canonical inherited registration. A narrow or
/// customized scanner is not sufficient to suppress launch safety coverage.
pub fn inherited_events(repo: &Path, home: Option<&Path>) -> BTreeSet<String> {
    let mut events = BTreeSet::new();
    for path in config_paths(repo, home) {
        let is_user = home.is_some_and(|home| {
            path == crate::session_history::import::claude_config_dir(home).join("settings.json")
        });
        if !is_user
            && crate::workspace_trust::claude_json_path().is_none_or(|state| {
                crate::workspace_trust::read_workspace_trust(&state, repo)
                    != crate::workspace_trust::WorkspaceTrust::Trusted
            })
        {
            continue;
        }
        let Some(doc) = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            continue;
        };
        let Some(hooks) = doc.get("hooks").and_then(Value::as_object) else {
            continue;
        };
        for (event, groups) in hooks {
            if groups
                .as_array()
                .is_some_and(|groups| groups.iter().any(|group| canonical_group(group, event)))
            {
                events.insert(event.clone());
            }
        }
    }
    events
}

fn canonical_group(group: &Value, event: &str) -> bool {
    group.get("matcher").and_then(Value::as_str) == Some("*")
        && group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|handlers| {
                handlers.iter().any(|handler| {
                    is_plain_scanner_handler(handler, event)
                        && handler["command"].as_str().is_some_and(|c| {
                            c.strip_suffix(SCANNER_EXIT_SUFFIX).unwrap_or(c)
                                == dispatcher_command(event)
                        })
                })
            })
}

/// The inherited scanner owns these events; retain every unrelated launch hook.
pub fn remove_inherited(settings: &mut Value, repo: &Path, home: Option<&Path>) {
    let required = suffixed_events(settings);
    if !required.is_empty() && migrate_with_suffix(repo, home, required).is_err() {
        return;
    }
    let Some(events) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for event in inherited_events(repo, home) {
        if let Some(groups) = events.get_mut(&event).and_then(Value::as_array_mut) {
            for group in groups.iter_mut() {
                if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                    handlers.retain(|handler| !is_plain_scanner_handler(handler, &event));
                }
            }
            groups.retain(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_none_or(|h| !h.is_empty())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_suffix_consolidation_and_migration() {
        let suffix = "; exit $LASTEXITCODE";
        for spelling in [
            "clud-cmd-scan",
            "clud-block-bad-cmd",
            "clud tool run hooks/block-bad-cmd.py",
            "& $env:CLUD_EXE tool run hooks/block-bad-cmd.py",
            "clud-cmd-scan --event PreToolUse",
        ] {
            let mut launch = json!({"hooks":{"PreToolUse":[{"matcher":"*", "hooks":[
                {"type":"command","command":format!("{spelling}{suffix}")},
                {"type":"command","command":"clud-cmd-scan --event PreToolUse"},
                {"type":"command","command":"clud-cmd-scan; echo custom"}
            ]}]}});
            crate::clud_hooks_compile::consolidate_scanner_hooks(&mut launch);
            assert_eq!(
                launch["hooks"]["PreToolUse"].as_array().unwrap().len(),
                2,
                "{spelling}"
            );
            assert_eq!(
                launch["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
                format!("clud-cmd-scan --event PreToolUse{suffix}")
            );
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            let home = tmp.path().join("home");
            let user = home.join(".claude/settings.json");
            write(
                &user,
                &json!({"hooks":{"PreToolUse":[{"matcher":"*", "hooks":[
                    {"type":"command","command":format!("{spelling}{suffix}")}
                ]}]}}),
            );
            migrate(&repo, Some(&home)).unwrap();
            let before = std::fs::read(&user).unwrap();
            migrate(&repo, Some(&home)).unwrap();
            assert_eq!(before, std::fs::read(&user).unwrap());
            let doc: Value = serde_json::from_slice(&before).unwrap();
            assert_eq!(
                doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
                format!("clud-cmd-scan --event PreToolUse{suffix}")
            );
            remove_inherited(&mut launch, &repo, Some(&home));
            assert_eq!(launch["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        }
    }

    #[test]
    fn exit_suffix_launch_upgrades_plain_inherited_scanner() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let user = home.join(".claude/settings.json");
        write(
            &user,
            &json!({"hooks":{"PreToolUse":[{"matcher":"*", "hooks":[
                {"type":"command","command":"clud-cmd-scan --event PreToolUse"}
            ]}]}}),
        );
        let mut launch = json!({"hooks":{"PreToolUse":[{"matcher":"*", "hooks":[
            {"type":"command","command":"clud-cmd-scan --event PreToolUse; exit $LASTEXITCODE"}
        ]}]}});
        remove_inherited(&mut launch, &repo, Some(&home));
        assert_eq!(launch["hooks"]["PreToolUse"], json!([]));
        let doc: Value = serde_json::from_slice(&std::fs::read(&user).unwrap()).unwrap();
        assert_eq!(
            doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "clud-cmd-scan --event PreToolUse; exit $LASTEXITCODE"
        );
    }

    #[test]
    fn exit_suffix_from_project_is_preserved_in_user_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let user = home.join(".claude/settings.json");
        let project = repo.join(".claude/settings.json");
        for (path, command) in [
            (&user, "clud-cmd-scan --event PostToolUse"),
            (
                &project,
                "clud-cmd-scan --event PostToolUse; exit $LASTEXITCODE",
            ),
        ] {
            write(
                path,
                &json!({"hooks":{"PostToolUse":[{"matcher":"*", "hooks":[
                    {"type":"command","command":command}
                ]}]}}),
            );
        }
        migrate(&repo, Some(&home)).unwrap();
        let doc: Value = serde_json::from_slice(&std::fs::read(&user).unwrap()).unwrap();
        assert_eq!(
            doc["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "clud-cmd-scan --event PostToolUse; exit $LASTEXITCODE"
        );
        let doc: Value = serde_json::from_slice(&std::fs::read(&project).unwrap()).unwrap();
        assert_eq!(doc["hooks"]["PostToolUse"], json!([]));
    }

    #[test]
    fn exit_suffix_customizations_are_untouched() {
        for command in [
            "clud-cmd-scan; exit 0",
            "clud-cmd-scan; exit $LASTEXITCODE ",
            "clud-cmd-scan; echo custom; exit $LASTEXITCODE",
            "clud-cmd-scan; exit $LASTEXITCODE; echo custom",
            "clud-cmd-scan --event Stop; exit $LASTEXITCODE",
        ] {
            let mut doc = json!({"hooks":{"PreToolUse":[{"matcher":"*", "hooks":[
                {"type":"command","command":command}
            ]}]}});
            let before = doc.clone();
            crate::clud_hooks_compile::consolidate_scanner_hooks(&mut doc);
            normalize(&mut doc, &mut BTreeSet::new(), &BTreeSet::new());
            assert_eq!(doc, before, "{command}");
        }
        assert!(!is_plain_scanner_handler(
            &json!({
                "type":"command", "command":"clud-cmd-scan; exit $LASTEXITCODE", "timeout":10
            }),
            "PreToolUse"
        ));
    }

    fn write(path: &Path, doc: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, doc.to_string()).unwrap();
    }

    #[test]
    fn normal_settings_and_launch_share_one_scanner_and_preserve_user_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let user = home.join(".claude/settings.json");
        let project = repo.join(".claude/settings.json");
        write(
            &user,
            &json!({"model":"keep", "hooks":{"PreToolUse":[{
                "matcher":"Bash", "hooks":[{"type":"command", "command":"clud-block-bad-cmd"}]
            }]}}),
        );
        write(
            &project,
            &json!({"hooks":{"PreToolUse":[{
                "matcher":"Bash", "hooks":[
                    {"type":"command", "command":"clud-cmd-scan"},
                    {"type":"command", "command":"user-check"}
                ]
            }]}}),
        );
        migrate(&repo, Some(&home)).unwrap();
        let user_doc: Value = serde_json::from_slice(&std::fs::read(&user).unwrap()).unwrap();
        let project_doc: Value = serde_json::from_slice(&std::fs::read(&project).unwrap()).unwrap();
        assert_eq!(user_doc["model"], "keep");
        assert!(canonical_group(
            &user_doc["hooks"]["PreToolUse"][0],
            "PreToolUse"
        ));
        assert_eq!(
            project_doc["hooks"]["PreToolUse"][0]["hooks"],
            json!([
                {"type":"command", "command":"user-check"}
            ])
        );
        let before = std::fs::read(&user).unwrap();
        migrate(&repo, Some(&home)).unwrap();
        assert_eq!(std::fs::read(&user).unwrap(), before);

        let mut launch = crate::clud_hooks_compile::deletion_safety_fragment();
        remove_inherited(&mut launch, &repo, Some(&home));
        assert_eq!(launch["hooks"]["PreToolUse"], json!([]));
        assert!(!launch["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn narrow_or_custom_handlers_do_not_suppress_launch_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let path = repo.join(".claude/settings.json");
        write(
            &path,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash", "hooks":[
                {"type":"command", "command":"clud-cmd-scan --event PreToolUse"}
            ]}]}}),
        );
        assert!(inherited_events(&repo, None).is_empty());
        write(
            &path,
            &json!({"hooks":{"PreToolUse":[{"matcher":"*", "hooks":[
                {"type":"command", "command":"clud-cmd-scan --event PreToolUse", "timeout":1}
            ]}]}}),
        );
        assert!(inherited_events(&repo, None).is_empty());
        let before = std::fs::read(&path).unwrap();
        migrate(&repo, None).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn untrusted_project_scanner_does_not_replace_launch_safety() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let project = repo.join(".claude/settings.json");
        write(
            &project,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash", "hooks":[
                {"type":"command", "command":"clud-block-bad-cmd"},
                {"type":"command", "command":"project-check"}
            ]}]}}),
        );
        migrate(&repo, Some(&home)).unwrap();
        assert!(!home.join(".claude/settings.json").exists());
        let local: Value = serde_json::from_slice(&std::fs::read(project).unwrap()).unwrap();
        assert!(canonical_group(
            &local["hooks"]["PreToolUse"][1],
            "PreToolUse"
        ));
        assert!(inherited_events(&repo, Some(&home)).is_empty());
        let mut launch = crate::clud_hooks_compile::deletion_safety_fragment();
        let before = launch.clone();
        remove_inherited(&mut launch, &repo, Some(&home));
        assert_eq!(launch, before);
    }

    #[test]
    fn migration_preserves_a_symlinked_user_settings_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let target = home.join("dotfiles/claude.json");
        write(
            &target,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash", "hooks":[
                {"type":"command", "command":"clud-block-bad-cmd"}
            ]}]}}),
        );
        let link = home.join(".claude/settings.json");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        if crate::platform::alias_link::symlink(&target, &link).is_err() {
            return;
        }
        migrate(&tmp.path().join("repo"), Some(&home)).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let doc: Value = serde_json::from_slice(&std::fs::read(target).unwrap()).unwrap();
        assert!(canonical_group(
            &doc["hooks"]["PreToolUse"][0],
            "PreToolUse"
        ));
    }
}
