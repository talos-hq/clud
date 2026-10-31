//! Bundle slash-command "skills" inside the `clud` binary and install them
//! into every supported backend's global skills directory on launch.
//!
//! Backends are listed in [`SKILL_BACKENDS`] — adding support for a new CLI
//! (OpenRouter, OpenCode, etc.) is one line: append a new [`SkillBackend`].
//! Each backend declares the home subdir used to detect that it is installed
//! (`.claude`, `.codex`, …) and the path under the user's home where its
//! skills live.
//!
//! This is the *only* skill installer, over the single source tree
//! `crates/clud-bin/assets/skills/`. It used to share the job with a second
//! module (`skill_install.rs`) that embedded a parallel top-level `skills/`
//! tree; the two forked, wrote the same paths with different bytes, and fought
//! on every launch. See DD-039.
//!
//! We install only into a backend whose home subdir already exists — that way
//! users who only run one CLI don't get the other CLIs' skill directories
//! created in their home. A skill file the user has taken ownership of (the
//! `managed-by: clud` marker removed) is never overwritten, so user edits
//! survive; a clud-managed copy that has gone stale is refreshed in place.
//! Codex reads skills from `~/.codex/skills/`, mirroring
//! Claude's `~/.claude/skills/` layout. Clud-managed copies that an older
//! build wrote to `~/.agents/skills/` are purged best-effort during Codex
//! global setup.
//!
//! The asset files live under `crates/clud-bin/assets/skills/` and are
//! embedded at compile time via `include_str!`, so the runtime needs no
//! filesystem access to read the source content.
//!
//! Errors are non-fatal: `main()` calls [`ensure_installed`], logs any
//! failure to stderr, and proceeds with launch.

use std::io;
use std::path::{Path, PathBuf};

use crate::backend::Backend;

const MANAGED_BY_CLUD_MARKER: &str = "managed-by: clud";

/// A hand patch applied ahead of a release, recognised by its own expiry
/// note (#2483). It promised to last only "until the updated clud binary is
/// installed", but it replaced the marker, so no upgrade ever replaced it.
const PENDING_UPGRADE_NOTE: &str = "preserve until the updated clud binary is installed";

/// Whether clud may refresh this installed copy.
fn clud_owned(body: &str) -> bool {
    body.contains(MANAGED_BY_CLUD_MARKER) || body.contains(PENDING_UPGRADE_NOTE)
}

/// One bundled skill: the directory name and the literal `SKILL.md` body.
pub struct BundledSkill {
    pub name: &'static str,
    pub skill_md: &'static str,
}

/// All skills the binary ships with. Add new entries here when you bundle
/// another `assets/skills/<name>/SKILL.md`.
pub const BUNDLED_SKILLS: &[BundledSkill] = &[
    BundledSkill {
        name: "clud-secret",
        skill_md: include_str!("../assets/skills/clud-secret/SKILL.md"),
    },
    BundledSkill {
        name: "do",
        skill_md: include_str!("../assets/skills/do/SKILL.md"),
    },
    BundledSkill {
        name: "grind",
        skill_md: include_str!("../assets/skills/grind/SKILL.md"),
    },
    BundledSkill {
        name: "grind-intake",
        skill_md: include_str!("../assets/skills/grind-intake/SKILL.md"),
    },
    BundledSkill {
        name: "grind-plan",
        skill_md: include_str!("../assets/skills/grind-plan/SKILL.md"),
    },
    BundledSkill {
        name: "grind-prework",
        skill_md: include_str!("../assets/skills/grind-prework/SKILL.md"),
    },
    BundledSkill {
        name: "grind-work",
        skill_md: include_str!("../assets/skills/grind-work/SKILL.md"),
    },
    BundledSkill {
        name: "grind-review",
        skill_md: include_str!("../assets/skills/grind-review/SKILL.md"),
    },
    BundledSkill {
        name: "grind-integrate",
        skill_md: include_str!("../assets/skills/grind-integrate/SKILL.md"),
    },
    BundledSkill {
        name: "grind-land",
        skill_md: include_str!("../assets/skills/grind-land/SKILL.md"),
    },
    BundledSkill {
        name: "grind-cron",
        skill_md: include_str!("../assets/skills/grind-cron/SKILL.md"),
    },
    BundledSkill {
        name: "clud-issue",
        skill_md: include_str!("../assets/skills/clud-issue/SKILL.md"),
    },
    BundledSkill {
        name: "clud-issue-triage",
        skill_md: include_str!("../assets/skills/clud-issue-triage/SKILL.md"),
    },
    BundledSkill {
        name: "clud-fix-quick",
        skill_md: include_str!("../assets/skills/clud-fix-quick/SKILL.md"),
    },
    BundledSkill {
        name: "clud-review",
        skill_md: include_str!("../assets/skills/clud-review/SKILL.md"),
    },
    BundledSkill {
        name: "clud-git",
        skill_md: include_str!("../assets/skills/clud-git/SKILL.md"),
    },
    BundledSkill {
        name: "clud-git-diff",
        skill_md: include_str!("../assets/skills/clud-git-diff/SKILL.md"),
    },
    BundledSkill {
        name: "clud-python-lint-deadcode",
        skill_md: include_str!("../assets/skills/clud-python-lint-deadcode/SKILL.md"),
    },
    BundledSkill {
        name: "clud-tag-release",
        skill_md: include_str!("../assets/skills/clud-tag-release/SKILL.md"),
    },
    BundledSkill {
        name: "clud-docker-rust-app-dev",
        skill_md: include_str!("../assets/skills/clud-docker-rust-app-dev/SKILL.md"),
    },
    BundledSkill {
        name: "clud-windows-trash",
        skill_md: include_str!("../assets/skills/clud-windows-trash/SKILL.md"),
    },
    BundledSkill {
        name: "clud-extern-repos",
        skill_md: include_str!("../assets/skills/clud-extern-repos/SKILL.md"),
    },
    BundledSkill {
        name: "clud-bosn",
        skill_md: include_str!("../assets/skills/clud-bosn/SKILL.md"),
    },
    BundledSkill {
        name: "clud-preloop",
        skill_md: include_str!("../assets/skills/clud-preloop/SKILL.md"),
    },
    BundledSkill {
        name: "clud-omarchy",
        skill_md: include_str!("../assets/skills/clud-omarchy/SKILL.md"),
    },
    BundledSkill {
        name: "clud-improve",
        skill_md: include_str!("../assets/skills/clud-improve/SKILL.md"),
    },
    BundledSkill {
        name: "clud-docker-mac-x86",
        skill_md: include_str!("../assets/skills/clud-docker-mac-x86/SKILL.md"),
    },
    BundledSkill {
        name: "clud-docker-linux-build",
        skill_md: include_str!("../assets/skills/clud-docker-linux-build/SKILL.md"),
    },
    BundledSkill {
        name: "clud-docker-recover",
        skill_md: include_str!("../assets/skills/clud-docker-recover/SKILL.md"),
    },
    BundledSkill {
        name: "clud-os-debug",
        skill_md: include_str!("../assets/skills/clud-os-debug/SKILL.md"),
    },
    BundledSkill {
        name: "video-use",
        skill_md: include_str!("../assets/skills/video-use/SKILL.md"),
    },
];

/// Bundled skills that have been retired. Entries stay here after the
/// `assets/skills/<name>/` source is deleted so an upgrade cleans the copies
/// already written into user homes — removing a [`BUNDLED_SKILLS`] entry
/// alone stops *new* installs but leaves every existing one in place, and a
/// stale skill keeps being offered to the agent forever.
///
/// `clud-loop` was the Codex-facing polyfill for Claude's native `/loop`.
/// The `--harness claude` cross-route (#622) gives Codex models the real
/// thing, so the polyfill is redundant. It also mis-fired there: its
/// triggers key on the word "Codex", so a Codex model driving the *Claude*
/// harness picked it over the built-in `/loop`.
///
/// `clud-docker-rust-app` was superseded by `clud-docker-rust-app-dev` and
/// dropped from `assets/skills/`, but every home that installed it still has
/// it — the exact leak this list exists to close.
///
/// `clud-pr`, `clud-fix`, `clud-do` and `clud-pr-merge` were retired together:
/// their orchestration (lock a deliverable in, then drive to it) is what the
/// harness's `/goal` Stop hook now does natively, so the skills were three
/// long playbooks re-implementing a built-in. `clud-pr-merge` goes with them
/// because it only ever existed as `clud-pr`'s merge phase. They may be
/// restored later; until then this list is what removes them from homes that
/// already installed them. Note `clud-pr` and `clud-issue` were also the two
/// skills forked across the old dual source trees, so their removal is what
/// finally makes one name mean one file.
///
/// `clud-meta-work` was folded into the `/grind` DAG: `/grind` routes to
/// capped `grind-*` agents and the bundled `grind-run` workflow, which do what the
/// playbook described with enforcement instead of prose.
///
/// This is deliberately an explicit list rather than "sweep any clud-managed
/// skill dir not in [`BUNDLED_SKILLS`]". An orphan sweep looks tempting and is
/// destructive: skills such as `coding-standards` and `verification-loop` were
/// installed by a since-removed bundler (62a26e4), still carry the marker, and
/// are still in daily use. A sweep would also delete newer skills whenever an
/// older clud binary ran. Retirement is a decision, not an inference.
pub const PURGED_BUNDLED_SKILLS: &[&str] = &[
    "clud-loop",
    "clud-docker-rust-app",
    "clud-pr",
    "clud-fix",
    "clud-do",
    "clud-pr-merge",
    "clud-meta-work",
];

/// One CLI backend that consumes `SKILL.md` files. Adding support for a
/// new tool is a one-line append to [`SKILL_BACKENDS`] — the on-disk layout
/// is the same as Claude Code's, just rooted under the tool's home subdir.
pub struct SkillBackend {
    /// Backend this install target belongs to.
    pub backend: Backend,
    /// Display name for log messages.
    pub name: &'static str,
    /// Path under the user's home dir where this backend stores config
    /// (e.g. `.claude`, `.codex`).
    pub home_subdir: &'static str,
    /// Optional override for the home-relative directory that contains
    /// this backend's skills, when it differs from `home_subdir`. Reserved
    /// for future backends whose skills live outside their config root.
    pub skills_home_subdir: Option<&'static str>,
    /// Path under the skills home dir where skill packages live (almost
    /// always `skills`, parameterized in case a future tool uses a different
    /// name).
    pub skills_subdir: &'static str,
}

impl SkillBackend {
    /// Resolved skills dir for this backend, given a home dir.
    pub fn skills_dir(&self, home: &Path) -> PathBuf {
        home.join(self.skills_home_subdir.unwrap_or(self.home_subdir))
            .join(self.skills_subdir)
    }

    /// True when this backend's home subdir exists as a directory under
    /// `home` — used to gate installs so we don't auto-create a backend
    /// root the user hasn't installed.
    pub fn root_exists(&self, home: &Path) -> bool {
        home.join(self.home_subdir).is_dir()
    }
}

/// Backends we install bundled skills into. To support a new CLI: confirm
/// it loads `SKILL.md`-format playbooks from a per-tool skills dir, then
/// append a `SkillBackend { ... }` entry here.
pub const SKILL_BACKENDS: &[SkillBackend] = &[
    SkillBackend {
        backend: Backend::Claude,
        name: "Claude Code",
        home_subdir: ".claude",
        skills_home_subdir: None,
        skills_subdir: "skills",
    },
    SkillBackend {
        backend: Backend::Codex,
        name: "Codex",
        home_subdir: ".codex",
        skills_home_subdir: None,
        skills_subdir: "skills",
    },
    // To add a new backend (e.g. OpenRouter CLI):
    // SkillBackend {
    //     backend: Backend::OpenRouter,
    //     name: "OpenRouter",
    //     home_subdir: ".openrouter",
    //     skills_home_subdir: None,
    //     skills_subdir: "skills",
    // },
];

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("could not resolve user home directory")]
    NoHomeDir,
    #[error("io error: {0}")]
    Io(io::Error),
}

impl From<io::Error> for InstallError {
    fn from(e: io::Error) -> Self {
        InstallError::Io(e)
    }
}

/// Result of an install pass into a single backend's skills dir.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InstallReport {
    pub installed: Vec<&'static str>,
    pub skipped_existing: Vec<&'static str>,
    /// Clud-managed copies rewritten because the bundled body changed.
    pub refreshed: Vec<&'static str>,
}

/// Result of the legacy Codex skill cleanup pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LegacyPurgeReport {
    pub removed: Vec<&'static str>,
    pub preserved: Vec<&'static str>,
    pub failed: Vec<&'static str>,
}

/// Compatibility helper that installs bundled skills into every backend whose
/// home subdir exists. Production launch setup calls
/// [`ensure_installed_for_backend`] for the selected backend instead. Returns
/// one `(backend, report)` per backend actually written to. Returns
/// [`InstallError::NoHomeDir`] only when the home dir itself cannot be
/// resolved.
pub fn ensure_installed() -> Result<Vec<(&'static SkillBackend, InstallReport)>, InstallError> {
    let home = crate::home::user_home().ok_or(InstallError::NoHomeDir)?;
    ensure_installed_at(&home)
}

pub fn ensure_installed_at(
    home: &Path,
) -> Result<Vec<(&'static SkillBackend, InstallReport)>, InstallError> {
    let _ = purge_stale_agents_skills(home, BUNDLED_SKILLS);
    let _ = purge_retired_bundled_skills(home, PURGED_BUNDLED_SKILLS);
    let mut results = Vec::new();
    for backend in active_backends(home) {
        let report = install_to(&backend.skills_dir(home), BUNDLED_SKILLS)?;
        if backend.backend == Backend::Codex {
            install_explicit_invocation_policies(&backend.skills_dir(home))?;
        }
        results.push((backend, report));
    }
    Ok(results)
}

pub fn ensure_installed_for_backend(
    backend: Backend,
) -> Result<Option<(&'static SkillBackend, InstallReport)>, InstallError> {
    let home = crate::home::user_home().ok_or(InstallError::NoHomeDir)?;
    ensure_installed_for_backend_at(&home, backend)
}

pub fn ensure_installed_for_backend_at(
    home: &Path,
    backend: Backend,
) -> Result<Option<(&'static SkillBackend, InstallReport)>, InstallError> {
    if matches!(backend, Backend::Codex) {
        let _ = purge_stale_agents_skills(home, BUNDLED_SKILLS);
    }
    // Runs for either backend, and sweeps both backends' dirs: a retired
    // skill installed under Codex must not survive because the user now
    // only ever launches Claude.
    let _ = purge_retired_bundled_skills(home, PURGED_BUNDLED_SKILLS);
    let Some(skill_backend) = backend_for(backend) else {
        return Ok(None);
    };
    if !skill_backend.root_exists(home) {
        return Ok(None);
    }
    let report = install_to(&skill_backend.skills_dir(home), BUNDLED_SKILLS)?;
    if matches!(backend, Backend::Codex) {
        install_explicit_invocation_policies(&skill_backend.skills_dir(home))?;
    }
    Ok(Some((skill_backend, report)))
}

/// Codex reads invocation policy from agents/openai.yaml, not SKILL.md.
/// Preserve user-owned policy files while refreshing copies installed by clud.
fn install_explicit_invocation_policies(base: &Path) -> Result<(), InstallError> {
    const DO_POLICY: &str = include_str!("../assets/skills/do/agents/openai.yaml");
    const GRIND_POLICY: &str = include_str!("../assets/skills/grind/agents/openai.yaml");
    const GRIND_SKILLS: &[&str] = &[
        "grind",
        "grind-intake",
        "grind-plan",
        "grind-prework",
        "grind-work",
        "grind-review",
        "grind-integrate",
        "grind-land",
        "grind-cron",
    ];
    const VIDEO_USE_POLICY: &str = include_str!("../assets/skills/video-use/agents/openai.yaml");
    for (name, policy) in [("do", DO_POLICY), ("video-use", VIDEO_USE_POLICY)]
        .into_iter()
        .chain(GRIND_SKILLS.iter().map(|name| (*name, GRIND_POLICY)))
    {
        let skill_dir = base.join(name);
        let skill_md = skill_dir.join("SKILL.md");
        let Ok(skill_body) = std::fs::read_to_string(&skill_md) else {
            continue;
        };
        if !skill_body.contains(MANAGED_BY_CLUD_MARKER) {
            continue;
        }
        let agents_dir = skill_dir.join("agents");
        let path = agents_dir.join("openai.yaml");
        match std::fs::read_to_string(&path) {
            Ok(existing) if !existing.contains(MANAGED_BY_CLUD_MARKER) => continue,
            Ok(existing) if normalize(&existing) == normalize(policy) => continue,
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => continue,
            _ => {
                std::fs::create_dir_all(&agents_dir)?;
                std::fs::write(&path, policy)?;
            }
        }
    }
    Ok(())
}

pub fn backend_for(backend: Backend) -> Option<&'static SkillBackend> {
    SKILL_BACKENDS.iter().find(|b| b.backend == backend)
}

/// All `SkillBackend`s whose home subdir currently exists under `home` —
/// i.e. the backends the user has installed.
pub fn active_backends(home: &Path) -> Vec<&'static SkillBackend> {
    SKILL_BACKENDS
        .iter()
        .filter(|b| b.root_exists(home))
        .collect()
}

/// Install the given skills into `base/<name>/SKILL.md`.
///
/// Four states per skill, and only the third one writes to an existing file:
/// - **Missing** — write the embedded copy, report `installed`.
/// - **User-owned** (the `managed-by: clud` marker was stripped, or the file
///   was hand-authored) — never touched, reported `skipped_existing`.
/// - **Ours and semantically stale** — overwrite, reported `refreshed`. The
///   caller is what surfaces this to the user; see `launch_setup.rs`.
/// - **Ours and current** — no write at all, reported `skipped_existing`.
///   Equality is modulo whitespace, so line-ending drift is not a change.
///
/// The no-write-when-current arm is load-bearing: it is what makes a repeat
/// launch a total no-op. `real_bundle_install_is_idempotent` pins it.
pub fn install_to(base: &Path, skills: &[BundledSkill]) -> Result<InstallReport, InstallError> {
    let mut report = InstallReport::default();
    for skill in skills {
        let skill_dir = base.join(skill.name);
        let skill_md = skill_dir.join("SKILL.md");
        match std::fs::read_to_string(&skill_md) {
            // Never installed here: write it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&skill_dir)?;
                std::fs::write(&skill_md, skill.skill_md)?;
                report.installed.push(skill.name);
            }
            // Present but unreadable (non-UTF-8, permissions): it is not ours
            // to overwrite, so leave it alone (#1906).
            Err(_) => {
                report.skipped_existing.push(skill.name);
            }
            // A copy the user has taken ownership of (marker stripped, or
            // hand-authored): never touch it.
            Ok(existing) if !clud_owned(&existing) => {
                report.skipped_existing.push(skill.name);
            }
            // Ours, and current. Compared modulo whitespace so a checkout or
            // editor that rewrote LF to CRLF does not read as a real change —
            // otherwise every launch on such a home rewrites the file and
            // reports an update that changed nothing.
            Ok(existing) if normalize(&existing) == normalize(skill.skill_md) => {
                report.skipped_existing.push(skill.name);
            }
            // Ours, and stale. Without this arm an edit to a bundled skill
            // only ever reaches users who never installed it — every
            // existing home keeps the version it first got, forever.
            Ok(_) => {
                std::fs::write(&skill_md, skill.skill_md)?;
                report.refreshed.push(skill.name);
            }
        }
    }
    Ok(report)
}

/// Whitespace-tolerant equality. Collapses runs of whitespace (including a
/// CRLF-vs-LF difference) into single spaces and trims the ends, so `"a  b\r\n"`
/// and `"a b"` compare equal.
///
/// Deliberately not shared with `tool_install.rs`'s identical helper: the two
/// answer different questions (skill bodies vs. tool scripts) and coupling them
/// would mean a tuning change for one silently retargets the other.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove stale clud-managed skill copies from `~/.agents/skills/`.
///
/// Clud used to install Codex skills under `~/.agents/skills/` based on an
/// expected cross-vendor skill location. Codex actually loads from
/// `~/.codex/skills/`, so the agents-dir copies were inert duplicates. The
/// cleanup is deliberately best effort and conservative: it only touches
/// directories named after currently bundled clud skills, and only removes
/// a `SKILL.md` that still carries the clud ownership marker. Any unrelated
/// files in the skill directory are left in place.
pub fn purge_stale_agents_skills(home: &Path, skills: &[BundledSkill]) -> LegacyPurgeReport {
    let stale_dir = home.join(".agents").join("skills");
    let mut report = LegacyPurgeReport::default();
    if !stale_dir.is_dir() {
        return report;
    }

    for skill in skills {
        let skill_dir = stale_dir.join(skill.name);
        let skill_md = skill_dir.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let body = match std::fs::read_to_string(&skill_md) {
            Ok(body) => body,
            Err(_) => {
                report.failed.push(skill.name);
                continue;
            }
        };
        if !body.contains(MANAGED_BY_CLUD_MARKER) {
            report.preserved.push(skill.name);
            continue;
        }
        match std::fs::remove_file(&skill_md) {
            Ok(()) => {
                report.removed.push(skill.name);
                let _ = std::fs::remove_dir(&skill_dir);
            }
            Err(_) => report.failed.push(skill.name),
        }
    }

    let _ = std::fs::remove_dir(&stale_dir);
    report
}

/// Remove retired bundled skills from every backend's skills dir.
///
/// Same conservative discipline as [`purge_stale_agents_skills`]: only
/// directories named in [`PURGED_BUNDLED_SKILLS`] are considered, and only a
/// `SKILL.md` still carrying the clud ownership marker is deleted. A user who
/// edited or hand-wrote the file keeps it. Unrelated files in the skill
/// directory are left alone, so the directory itself only disappears when it
/// held nothing but our `SKILL.md`.
pub fn purge_retired_bundled_skills(home: &Path, retired: &[&'static str]) -> LegacyPurgeReport {
    let mut report = LegacyPurgeReport::default();
    for backend in SKILL_BACKENDS {
        let skills_dir = backend.skills_dir(home);
        if !skills_dir.is_dir() {
            continue;
        }
        for name in retired {
            let skill_dir = skills_dir.join(name);
            let skill_md = skill_dir.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let body = match std::fs::read_to_string(&skill_md) {
                Ok(body) => body,
                Err(_) => {
                    report.failed.push(name);
                    continue;
                }
            };
            if !body.contains(MANAGED_BY_CLUD_MARKER) {
                report.preserved.push(name);
                continue;
            }
            match std::fs::remove_file(&skill_md) {
                Ok(()) => {
                    report.removed.push(name);
                    let _ = std::fs::remove_dir(&skill_dir);
                }
                Err(_) => report.failed.push(name),
            }
        }
    }
    report
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
