//! Auto-installer for the bundled `clud`-managed Python tools.
//!
//! Mirrors the [`skills`](crate::skills) module, applied to a parallel
//! [`BUNDLED_TOOLS`](crate::tools::BUNDLED_TOOLS) registry. Each tool is a
//! self-contained `uv run` script (PEP 723 inline metadata) that gets
//! materialized to `~/.clud/tools/<rel_path>` on `clud` startup.
//!
//! Three states per tool:
//! - **Missing** — write the embedded copy, log a one-line install notice.
//! - **Matches modulo whitespace** — silent no-op.
//! - **Diverges semantically** — overwrite with the embedded copy and log
//!   `[clud] updated tool/<rel_path>` in green. The embedded version is the
//!   source of truth; local edits to managed copies are lost.
//!
//! User-edited files (those that lack the `# managed-by: clud` marker) are
//! never touched. This lets a developer hand-customize an installed script
//! and have it survive `clud` upgrades.
//!
//! Retired tools follow the same safety rule through [`PURGED_TOOLS`]: only a
//! stale file that still carries the managed marker is removed. User-authored
//! files at the old path are preserved.
//!
//! All errors are non-fatal: a tool-install hiccup never breaks the launch
//! path — at worst the user sees a `[clud] note: …` line and continues.
//!
//! See `crates/clud-bin/src/tools.rs` for the `BundledTool` struct and the
//! `BUNDLED_TOOLS` registry; this module owns the install-time logic only.

use std::path::{Path, PathBuf};

use crate::tools::{BundledTool, BUNDLED_TOOLS, MANAGED_BY_CLUD_MARKER};

/// Top-level directory under the user's home where clud-managed tools live.
/// Backend-agnostic — tools are not consumed by claude or codex directly,
/// they're invoked by the `clud tool run` subcommand which sets
/// `UV_CACHE_DIR` itself.
const TOOLS_ROOT: &str = ".clud/tools";

/// A previously bundled tool that should be removed from managed installs.
///
/// Keep this list empty until a compatibility window has closed. For example,
/// `hooks/block-bad-cmd.py` remains bundled as a one-release shim while hook
/// configs migrate to the native `clud-cmd-scan` alias of `clud`.
pub struct PurgedTool {
    pub rel_path: &'static str,
    pub reason: &'static str,
}

pub const PURGED_TOOLS: &[PurgedTool] = &[];

/// Resolve `~/.clud/tools/`. Returns `None` if the home directory cannot be
/// determined; callers degrade silently in that case.
pub fn tools_root() -> Option<PathBuf> {
    crate::home::user_home().map(|h| h.join(TOOLS_ROOT))
}

/// Path on disk where a bundled tool with the given relative path is
/// installed. `rel_path` is the same string carried in
/// [`BundledTool::rel_path`].
pub fn target_path_at(home: &Path, rel_path: &str) -> PathBuf {
    home.join(TOOLS_ROOT).join(rel_path)
}

/// Production entry point — install every tool to the current user's home.
/// Cheap on the steady state (one stat + at most one read per tool). All
/// failures degrade silently to stderr `[clud] note: …` lines.
pub fn ensure_installed() {
    let Some(home) = crate::home::user_home() else {
        return;
    };
    ensure_installed_at(&home);
}

/// Testable variant — install every tool under the supplied home root.
pub fn ensure_installed_at(home: &Path) {
    for tool in BUNDLED_TOOLS {
        ensure_tool_installed_at(home, tool);
    }
    purge_retired_tools_at(home, PURGED_TOOLS);
}

fn ensure_tool_installed_at(home: &Path, tool: &BundledTool) {
    let path = target_path_at(home, tool.rel_path);
    match classify(&path, tool.body) {
        Existing::Missing => write_install(&path, tool),
        Existing::Matches => {}
        Existing::Diverges => update_diverges(&path, tool),
        Existing::UserEdited => {
            // Marker absent: the user has either authored this file themselves
            // or hand-customized it. Never clobber.
        }
        Existing::Unreadable(err) => {
            eprintln!("[clud] note: could not read {}: {err}", path.display());
        }
    }
}

#[derive(Debug)]
enum Existing {
    Missing,
    Matches,
    Diverges,
    /// On-disk file lacks the `# managed-by: clud` marker — treat as
    /// user-authored and leave it alone.
    UserEdited,
    Unreadable(std::io::Error),
}

fn classify(path: &Path, embedded: &str) -> Existing {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Existing::Missing,
        Err(e) => Existing::Unreadable(e),
        Ok(content) => {
            if !content.contains(MANAGED_BY_CLUD_MARKER) {
                Existing::UserEdited
            } else if normalize(&content) == normalize(embedded) {
                Existing::Matches
            } else {
                Existing::Diverges
            }
        }
    }
}

/// Line-ending-tolerant equality: CRLF vs LF and trailing whitespace are
/// ignored, but leading indentation is kept because it is meaningful in the
/// bundled Python tools (#1907). Kept local to this module so the
/// tool-install pipeline stays independent of the bundled-skill one in
/// [`skills`](crate::skills).
fn normalize(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    lines.join("\n").trim_end().to_string()
}

fn write_install(path: &Path, tool: &BundledTool) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "[clud] note: could not create tool dir {}: {e}",
                parent.display()
            );
            return;
        }
    }
    if let Err(e) = std::fs::write(path, tool.body) {
        eprintln!(
            "[clud] note: could not install tool/{} at {}: {e}",
            tool.rel_path,
            path.display()
        );
        return;
    }
    eprintln!(
        "[clud] installed tool/{} at {}",
        tool.rel_path,
        path.display()
    );
}

fn update_diverges(path: &Path, tool: &BundledTool) {
    if let Err(e) = std::fs::write(path, tool.body) {
        eprintln!(
            "[clud] note: could not update tool/{} at {}: {e}",
            tool.rel_path,
            path.display()
        );
        return;
    }
    eprintln!("\x1b[32m[clud] updated tool/{}\x1b[0m", tool.rel_path);
}

pub fn purge_retired_tools_at(home: &Path, purged_tools: &[PurgedTool]) -> PurgeReport {
    let mut report = PurgeReport::default();
    for tool in purged_tools {
        let path = target_path_at(home, tool.rel_path);
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report.missing += 1;
            }
            Err(e) => {
                report.errors += 1;
                eprintln!(
                    "[clud] note: could not read retired tool {}: {e}",
                    path.display()
                );
            }
            Ok(content) => {
                if !content.contains(MANAGED_BY_CLUD_MARKER) {
                    report.preserved_user += 1;
                    continue;
                }
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        report.removed += 1;
                        eprintln!(
                            "\x1b[32m[clud] removed retired tool/{} ({})\x1b[0m",
                            tool.rel_path, tool.reason
                        );
                    }
                    Err(e) => {
                        report.errors += 1;
                        eprintln!(
                            "[clud] note: could not remove retired tool/{} at {}: {e}",
                            tool.rel_path,
                            path.display()
                        );
                    }
                }
            }
        }
    }
    report
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PurgeReport {
    pub removed: usize,
    pub preserved_user: usize,
    pub missing: usize,
    pub errors: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;
    use std::fs;

    /// A throwaway tool used to exercise the install state machine without
    /// depending on the production [`BUNDLED_TOOLS`] (which is empty at
    /// foundation time and would defeat the test).
    fn fake_tool() -> BundledTool {
        BundledTool {
            rel_path: "fake/probe.py",
            body: "# managed-by: clud\nprint('probe')\n",
            kill_semantics: crate::tools::KillSemantics::Killable,
            command_timeout: crate::tools::DEFAULT_KILLABLE_TIMEOUT,
            progress_timeout: None,
            quiet_ok: false,
        }
    }

    fn fake_tool_v2() -> BundledTool {
        BundledTool {
            rel_path: "fake/probe.py",
            body: "# managed-by: clud\nprint('probe v2')\n",
            kill_semantics: crate::tools::KillSemantics::Killable,
            command_timeout: crate::tools::DEFAULT_KILLABLE_TIMEOUT,
            progress_timeout: None,
            quiet_ok: false,
        }
    }

    #[test]
    fn normalize_ignores_trailing_whitespace_and_blank_tail() {
        assert_eq!(normalize("a  \nb\n\n"), normalize("a\nb"));
    }

    #[test]
    fn normalize_keeps_python_indentation_changes() {
        let old = "# managed-by: clud\nif a:\n    b()\nc()\n";
        let new = "# managed-by: clud\nif a:\n    b()\n    c()\n";
        assert_ne!(normalize(old), normalize(new));
    }

    #[test]
    fn normalize_handles_crlf_vs_lf() {
        // Windows checkout artifact: file on disk has CRLF, embedded content
        // baked in with LF. Normalize must compare them equal.
        let crlf = "line1\r\nline2\r\n";
        let lf = "line1\nline2\n";
        assert_eq!(normalize(crlf), normalize(lf));
    }

    #[test]
    fn missing_file_triggers_install() {
        let tmp = TemporaryDirectory::new().unwrap();
        let tool = fake_tool();
        let target = target_path_at(tmp.path(), tool.rel_path);
        assert!(matches!(classify(&target, tool.body), Existing::Missing));
        ensure_tool_installed_at(tmp.path(), &tool);
        let written = fs::read_to_string(&target).unwrap();
        assert_eq!(written, tool.body);
    }

    #[test]
    fn second_install_is_a_noop() {
        let tmp = TemporaryDirectory::new().unwrap();
        let tool = fake_tool();
        ensure_tool_installed_at(tmp.path(), &tool);
        let target = target_path_at(tmp.path(), tool.rel_path);
        let mtime_before = fs::metadata(&target).unwrap().modified().unwrap();
        // A no-op classify should not rewrite the file.
        assert!(matches!(classify(&target, tool.body), Existing::Matches));
        ensure_tool_installed_at(tmp.path(), &tool);
        let mtime_after = fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "managed match must not rewrite");
    }

    #[test]
    fn user_edited_file_is_preserved() {
        let tmp = TemporaryDirectory::new().unwrap();
        let tool = fake_tool();
        let target = target_path_at(tmp.path(), tool.rel_path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        // Note: no `managed-by: clud` marker — this is a user-authored file.
        let user_body = "#!/usr/bin/env python\nprint('mine')\n";
        fs::write(&target, user_body).unwrap();

        assert!(matches!(classify(&target, tool.body), Existing::UserEdited));
        ensure_tool_installed_at(tmp.path(), &tool);

        let after = fs::read_to_string(&target).unwrap();
        assert_eq!(after, user_body, "user-edited tool must not be clobbered");
    }

    #[test]
    fn semantic_diff_overwrites_managed_copy() {
        let tmp = TemporaryDirectory::new().unwrap();
        let old = fake_tool();
        let new = fake_tool_v2();

        ensure_tool_installed_at(tmp.path(), &old);
        let target = target_path_at(tmp.path(), old.rel_path);
        assert_eq!(fs::read_to_string(&target).unwrap(), old.body);

        // Now the bundle ships a newer body for the same rel_path.
        assert!(matches!(classify(&target, new.body), Existing::Diverges));
        ensure_tool_installed_at(tmp.path(), &new);
        assert_eq!(fs::read_to_string(&target).unwrap(), new.body);
    }

    #[test]
    fn whitespace_only_diff_is_noop() {
        let tmp = TemporaryDirectory::new().unwrap();
        let tool = fake_tool();
        let target = target_path_at(tmp.path(), tool.rel_path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        // Same content, CRLF line endings — should classify as Matches.
        let on_disk = tool.body.replace('\n', "\r\n");
        fs::write(&target, &on_disk).unwrap();
        assert!(matches!(classify(&target, tool.body), Existing::Matches));
        ensure_tool_installed_at(tmp.path(), &tool);
        // Untouched.
        assert_eq!(fs::read_to_string(&target).unwrap(), on_disk);
    }

    #[test]
    fn creates_missing_parent_dirs() {
        let tmp = TemporaryDirectory::new().unwrap();
        let tool = BundledTool {
            rel_path: "a/b/c/deep.py",
            body: "# managed-by: clud\n",
            kill_semantics: crate::tools::KillSemantics::Killable,
            command_timeout: crate::tools::DEFAULT_KILLABLE_TIMEOUT,
            progress_timeout: None,
            quiet_ok: false,
        };
        ensure_tool_installed_at(tmp.path(), &tool);
        let target = target_path_at(tmp.path(), tool.rel_path);
        assert!(target.exists());
    }

    #[test]
    fn tools_root_resolves_to_dot_clud_tools() {
        let tmp = TemporaryDirectory::new().unwrap();
        // The helper takes an explicit home in tests so we don't depend on
        // $HOME / %USERPROFILE% pointing where we want.
        let resolved = tmp.path().join(TOOLS_ROOT);
        assert_eq!(
            target_path_at(tmp.path(), "x.py").parent(),
            Some(resolved.as_path())
        );
    }

    #[test]
    fn retired_managed_tool_is_removed() {
        let tmp = TemporaryDirectory::new().unwrap();
        let retired = PurgedTool {
            rel_path: "hooks/block-bad-cmd.py",
            reason: "native helper rollout complete",
        };
        let target = target_path_at(tmp.path(), retired.rel_path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "# managed-by: clud\nprint('old')\n").unwrap();

        let report = purge_retired_tools_at(tmp.path(), &[retired]);

        assert_eq!(report.removed, 1);
        assert!(!target.exists());
    }

    #[test]
    fn retired_user_authored_tool_is_preserved() {
        let tmp = TemporaryDirectory::new().unwrap();
        let retired = PurgedTool {
            rel_path: "hooks/block-bad-cmd.py",
            reason: "native helper rollout complete",
        };
        let target = target_path_at(tmp.path(), retired.rel_path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let body = "#!/usr/bin/env python\nprint('mine')\n";
        fs::write(&target, body).unwrap();

        let report = purge_retired_tools_at(tmp.path(), &[retired]);

        assert_eq!(report.preserved_user, 1);
        assert_eq!(fs::read_to_string(target).unwrap(), body);
    }

    #[test]
    fn retired_missing_tool_is_noop() {
        let tmp = TemporaryDirectory::new().unwrap();
        let retired = PurgedTool {
            rel_path: "hooks/block-bad-cmd.py",
            reason: "native helper rollout complete",
        };

        let report = purge_retired_tools_at(tmp.path(), &[retired]);

        assert_eq!(report.missing, 1);
        assert_eq!(report.errors, 0);
    }
}
