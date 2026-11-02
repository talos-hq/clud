//! Bundled Claude Code agent definitions and workflow scripts.
//!
//! The `/grind` DAG needs more than skills: capped agent types under
//! `~/.claude/agents/` and the `grind-run` workflow under `~/.claude/workflows/`.
//! Both are Claude-only file kinds, so this installer targets `~/.claude`
//! alone and only when that directory already exists.
//!
//! Ownership follows the skill installer (`skills::install_to`): a missing
//! file is written, a file whose `managed-by: clud` marker was stripped is the
//! user's and is never touched, a stale managed copy is refreshed, and a
//! current one is not rewritten.

use std::io;
use std::path::Path;

const MANAGED_BY_CLUD_MARKER: &str = "managed-by: clud";

/// One bundled file: its path relative to `~/.claude`, and its body.
///
/// `procedure` names a bundled skill whose body is appended to an agent
/// file at install. The `/grind` skills are `disable-model-invocation`
/// so a user's prompt never pulls them in, which also means an agent cannot
/// load them with the `Skill` tool or `skills:` preload. Appending keeps the
/// skill the single source of the procedure.
pub struct BundledClaudeFile {
    pub rel_path: &'static str,
    pub body: &'static str,
    pub procedure: Option<&'static str>,
}

impl BundledClaudeFile {
    /// The exact text written to disk.
    pub fn rendered(&self) -> String {
        let Some(skill) = self.procedure else {
            return self.body.to_string();
        };
        let skill_md = crate::skills::BUNDLED_SKILLS
            .iter()
            .find(|s| s.name == skill)
            .map_or("", |s| s.skill_md);
        format!(
            "{}\n## Procedure (`/{skill}`, appended by clud)\n\n{}",
            self.body.trim_end(),
            skill_body(skill_md).trim_start()
        )
    }
}

/// A `SKILL.md` without its YAML frontmatter and ownership marker.
fn skill_body(skill_md: &str) -> &str {
    let rest = skill_md
        .strip_prefix("---")
        .and_then(|r| r.split_once("\n---"))
        .map_or(skill_md, |(_, body)| body);
    rest.trim_start()
        .strip_prefix("<!-- managed-by: clud -->")
        .unwrap_or(rest)
}

/// Every bundled agent definition and workflow script.
pub const BUNDLED_CLAUDE_FILES: &[BundledClaudeFile] = &[
    BundledClaudeFile {
        rel_path: "agents/grind-orchestrator.md",
        body: include_str!("../assets/agents/grind-orchestrator.md"),
        procedure: None,
    },
    BundledClaudeFile {
        rel_path: "agents/grind-gate.md",
        body: include_str!("../assets/agents/grind-gate.md"),
        procedure: None,
    },
    BundledClaudeFile {
        rel_path: "agents/grind-planner.md",
        body: include_str!("../assets/agents/grind-planner.md"),
        procedure: Some("grind-plan"),
    },
    BundledClaudeFile {
        rel_path: "agents/grind-prework.md",
        body: include_str!("../assets/agents/grind-prework.md"),
        procedure: Some("grind-prework"),
    },
    BundledClaudeFile {
        rel_path: "agents/grind-worker.md",
        body: include_str!("../assets/agents/grind-worker.md"),
        procedure: Some("grind-work"),
    },
    BundledClaudeFile {
        rel_path: "agents/grind-reviewer.md",
        body: include_str!("../assets/agents/grind-reviewer.md"),
        procedure: Some("grind-review"),
    },
    BundledClaudeFile {
        rel_path: "agents/grind-integrator.md",
        body: include_str!("../assets/agents/grind-integrator.md"),
        procedure: Some("grind-integrate"),
    },
    BundledClaudeFile {
        rel_path: "agents/grind-lander.md",
        body: include_str!("../assets/agents/grind-lander.md"),
        procedure: Some("grind-land"),
    },
    BundledClaudeFile {
        rel_path: "workflows/grind-run.js",
        body: include_str!("../assets/workflows/grind-run.js"),
        procedure: None,
    },
];

/// Files clud used to install and has since retired or renamed. Each is
/// deleted only while it still carries the `managed-by: clud` marker, so a
/// user's own file at the same path is never touched.
///
/// `workflows/grind.js` was renamed to `grind-run.js`: a workflow named
/// `grind` listed a second `/grind` beside the router skill, and picking it
/// skipped the router's questions.
pub const PURGED_CLAUDE_FILES: &[&str] = &["workflows/grind.js"];

/// Result of one install pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InstallReport {
    pub installed: Vec<&'static str>,
    pub skipped_existing: Vec<&'static str>,
    pub refreshed: Vec<&'static str>,
    pub purged: Vec<&'static str>,
}

/// Install [`BUNDLED_CLAUDE_FILES`] under `home/.claude`, or do nothing when
/// Claude Code is not installed there.
pub fn ensure_installed_at(home: &Path) -> io::Result<Option<InstallReport>> {
    let root = home.join(".claude");
    if !root.is_dir() {
        return Ok(None);
    }
    let mut report = install_to(&root, BUNDLED_CLAUDE_FILES)?;
    report.purged = purge_retired(&root, PURGED_CLAUDE_FILES);
    Ok(Some(report))
}

/// Delete each retired file that is still clud-managed; report what went.
pub fn purge_retired(root: &Path, retired: &[&'static str]) -> Vec<&'static str> {
    retired
        .iter()
        .copied()
        .filter(|rel| {
            let path = root.join(rel);
            std::fs::read_to_string(&path).is_ok_and(|body| body.contains(MANAGED_BY_CLUD_MARKER))
                && std::fs::remove_file(&path).is_ok()
        })
        .collect()
}

pub fn install_to(root: &Path, files: &[BundledClaudeFile]) -> io::Result<InstallReport> {
    let mut report = InstallReport::default();
    for file in files {
        let path = root.join(file.rel_path);
        let body = file.rendered();
        match std::fs::read_to_string(&path) {
            // Only a missing file is ours to write. Any other read failure
            // (permissions, non-UTF-8 content) may be a user's file.
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            Err(_) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, &body)?;
                report.installed.push(file.rel_path);
            }
            Ok(existing) if !existing.contains(MANAGED_BY_CLUD_MARKER) => {
                report.skipped_existing.push(file.rel_path);
            }
            Ok(existing) if normalize(&existing) == normalize(&body) => {
                report.skipped_existing.push(file.rel_path);
            }
            Ok(_) => {
                std::fs::write(&path, &body)?;
                report.refreshed.push(file.rel_path);
            }
        }
    }
    Ok(report)
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_file_carries_the_marker() {
        for file in BUNDLED_CLAUDE_FILES {
            assert!(
                file.body.contains(MANAGED_BY_CLUD_MARKER),
                "{} must carry the managed-by marker",
                file.rel_path
            );
        }
    }

    /// Issue #1676: the bounded reader is only worth shipping with routing.
    #[test]
    fn worker_routes_reads_through_the_bounded_slice_tool() {
        let worker = BUNDLED_CLAUDE_FILES
            .iter()
            .find(|f| f.rel_path == "agents/grind-worker.md")
            .expect("grind-worker.md must stay bundled");
        assert!(
            worker.body.contains("code/slice.py"),
            "grind-worker must name the bounded reader it is told to use"
        );
        assert!(worker.body.contains("code/outline.py"));
    }

    #[test]
    fn bundled_includes_the_grind_roles_and_workflow() {
        let paths: Vec<_> = BUNDLED_CLAUDE_FILES.iter().map(|f| f.rel_path).collect();
        for role in [
            "orchestrator",
            "gate",
            "planner",
            "prework",
            "worker",
            "reviewer",
            "integrator",
            "lander",
        ] {
            let path = format!("agents/grind-{role}.md");
            assert!(paths.contains(&path.as_str()), "missing {path}");
        }
        assert!(paths.contains(&"workflows/grind-run.js"));
        assert!(!paths.contains(&"workflows/grind.js"));
    }

    /// The agent `name:` must match its file, or the workflow's
    /// `agentType: 'grind-<role>'` resolves to nothing.
    #[test]
    fn agent_names_match_their_files() {
        for file in BUNDLED_CLAUDE_FILES {
            let Some(stem) = file
                .rel_path
                .strip_prefix("agents/")
                .and_then(|p| p.strip_suffix(".md"))
            else {
                continue;
            };
            assert!(
                file.body.contains(&format!("\nname: {stem}\n")),
                "{} frontmatter name must be {stem}",
                file.rel_path
            );
        }
    }

    /// Every capped role declares its tools, and reviewers get exactly the
    /// worker's.
    #[test]
    fn capped_roles_declare_tool_lists() {
        let body = |role: &str| {
            BUNDLED_CLAUDE_FILES
                .iter()
                .find(|f| f.rel_path == format!("agents/grind-{role}.md"))
                .unwrap()
                .body
        };
        for role in [
            "orchestrator",
            "gate",
            "planner",
            "prework",
            "worker",
            "reviewer",
            "integrator",
            "lander",
        ] {
            assert!(
                body(role).contains("\ntools: "),
                "grind-{role} needs tools:"
            );
        }
        let tools = |role: &str| {
            body(role)
                .lines()
                .find(|l| l.starts_with("tools: "))
                .unwrap()
                .to_string()
        };
        assert_eq!(tools("reviewer"), tools("worker"));
        assert!(!body("lander").contains("Edit"));
        assert!(!body("planner").contains("Edit"));
        assert_eq!(tools("prework"), "tools: Bash, Read, Grep, Glob");
    }

    #[test]
    fn gate_agent_is_small_shell_only_and_requires_execution_evidence() {
        let file = BUNDLED_CLAUDE_FILES
            .iter()
            .find(|file| file.rel_path == "agents/grind-gate.md")
            .unwrap();
        assert!(file.procedure.is_none());
        assert!(file.body.contains("\ntools: Bash\n"));
        assert!(file.body.contains("\nmodel: haiku\n"));
        for instruction in [
            "Copy it verbatim",
            "actually completed successfully",
            "Do not retry, sleep or poll",
            "execution evidence remains authoritative",
        ] {
            assert!(file.body.contains(instruction), "missing {instruction}");
        }
    }

    #[test]
    fn orchestrator_instructions_cover_dependencies_and_approval() {
        let file = BUNDLED_CLAUDE_FILES
            .iter()
            .find(|f| f.rel_path == "agents/grind-orchestrator.md")
            .unwrap();
        assert!(file.procedure.is_none());
        assert!(file.body.contains("\ntools: Read, Grep, Glob, Bash\n"));
        for instruction in [
            "ask what blocks it",
            "`merged`, `released`, `tag` or `published`",
            "exact read-only `check` command",
            "PR URLs, versions and tags",
            "dirty or active checkouts",
            "`proposed_goals`",
            "never silently add it to `goals`",
            "obtains approval before execution",
        ] {
            assert!(file.body.contains(instruction), "missing {instruction}");
        }
    }

    #[test]
    fn install_is_idempotent_and_respects_user_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let first = install_to(root, BUNDLED_CLAUDE_FILES).unwrap();
        assert_eq!(first.installed.len(), BUNDLED_CLAUDE_FILES.len());
        let second = install_to(root, BUNDLED_CLAUDE_FILES).unwrap();
        assert!(second.installed.is_empty() && second.refreshed.is_empty());

        let owned = root.join("workflows/grind-run.js");
        std::fs::write(&owned, "// my own grind").unwrap();
        let third = install_to(root, BUNDLED_CLAUDE_FILES).unwrap();
        assert!(third.skipped_existing.contains(&"workflows/grind-run.js"));
        assert_eq!(std::fs::read_to_string(&owned).unwrap(), "// my own grind");
    }

    /// No workflow may share a skill's name, or Claude Code lists two
    /// commands under one `/name`.
    #[test]
    fn workflow_names_never_collide_with_skills() {
        for file in BUNDLED_CLAUDE_FILES {
            let Some(stem) = file
                .rel_path
                .strip_prefix("workflows/")
                .and_then(|p| p.strip_suffix(".js"))
            else {
                continue;
            };
            assert!(
                file.body.contains(&format!("name: '{stem}'")),
                "{} meta name must be {stem}",
                file.rel_path
            );
            assert!(
                !crate::skills::BUNDLED_SKILLS.iter().any(|s| s.name == stem),
                "workflow {stem} collides with the bundled skill of the same name"
            );
        }
    }

    #[test]
    fn retired_grind_workflow_is_purged_but_user_copies_survive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".claude");
        std::fs::create_dir_all(root.join("workflows")).unwrap();
        let old = root.join("workflows/grind.js");

        std::fs::write(&old, "// managed-by: clud\nold").unwrap();
        let report = ensure_installed_at(dir.path()).unwrap().unwrap();
        assert_eq!(report.purged, vec!["workflows/grind.js"]);
        assert!(!old.exists());
        assert!(root.join("workflows/grind-run.js").is_file());

        std::fs::write(&old, "// my own grind").unwrap();
        let report = ensure_installed_at(dir.path()).unwrap().unwrap();
        assert!(report.purged.is_empty());
        assert!(old.exists());
    }

    /// Each grind agent carries its leaf skill's procedure, since a hidden
    /// skill cannot be loaded by the agent itself.
    #[test]
    fn agents_carry_their_leaf_procedure() {
        for file in BUNDLED_CLAUDE_FILES {
            let Some(skill) = file.procedure else {
                continue;
            };
            let bundled = crate::skills::BUNDLED_SKILLS
                .iter()
                .find(|s| s.name == skill)
                .unwrap_or_else(|| panic!("{} names unknown skill {skill}", file.rel_path));
            let rendered = file.rendered();
            assert!(rendered.starts_with(file.body.trim_end()));
            assert!(rendered.contains(skill_body(bundled.skill_md).trim()));
            assert!(
                !rendered.contains("disable-model-invocation"),
                "frontmatter must not leak"
            );
            assert_eq!(rendered.matches(MANAGED_BY_CLUD_MARKER).count(), 1);
        }
    }

    /// The router and leaves are available only through explicit invocation.
    #[test]
    fn grind_skills_are_hidden_from_the_model() {
        for skill in crate::skills::BUNDLED_SKILLS {
            let hidden = skill
                .skill_md
                .contains("\ndisable-model-invocation: true\n");
            if skill.name == "grind" || skill.name.starts_with("grind-") {
                assert!(hidden, "{} must be disable-model-invocation", skill.name);
            }
        }
    }

    /// Workers and reviewers cannot run commands, so a planned task that
    /// writes no file (e.g. "verify build and tests") can only end "Not
    /// done" (#1397). The planner must put checks in `verify`, and the
    /// workflow must route any file-less task there instead of to a worker.
    #[test]
    fn grind_routes_file_less_tasks_to_the_integrator() {
        let workflow = include_str!("../assets/workflows/grind-run.js");
        assert!(
            workflow.contains("const routeCheckTasks = "),
            "grind-run.js must move file-less tasks into verify"
        );
        assert!(
            workflow.contains("routeCheckTasks(earlierDeps(planned, g))"),
            "runGoal must apply routeCheckTasks before work()"
        );
        let plan_skill = include_str!("../assets/skills/grind-plan/SKILL.md");
        assert!(
            plan_skill.contains("Never plan a task that only runs commands"),
            "grind-plan must forbid verify-only tasks"
        );
    }

    #[test]
    fn missing_claude_home_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_installed_at(dir.path()).unwrap().is_none());
    }
}
