use super::*;
use serde::Deserialize;
use tempfile::tempdir;

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    triggers: Vec<String>,
}

fn frontmatter_yaml<'a>(skill_name: &str, skill_md: &'a str) -> &'a str {
    let Some(after_open) = skill_md
        .strip_prefix("---\r\n")
        .or_else(|| skill_md.strip_prefix("---\n"))
    else {
        panic!("skill {skill_name} must start with YAML frontmatter");
    };
    let Some(end) = after_open.find("\n---") else {
        panic!("skill {skill_name} missing closing YAML frontmatter marker");
    };
    &after_open[..end]
}

fn parse_frontmatter(skill: &BundledSkill) -> SkillFrontmatter {
    serde_yaml::from_str(frontmatter_yaml(skill.name, skill.skill_md))
        .unwrap_or_else(|err| panic!("skill {} has invalid YAML frontmatter: {err}", skill.name))
}

fn fake_skills() -> Vec<BundledSkill> {
    vec![
        BundledSkill {
            name: "alpha",
            skill_md: "alpha body\n",
        },
        BundledSkill {
            name: "beta",
            skill_md: "beta body\n",
        },
    ]
}

#[test]
fn do_and_grind_skills_install_with_explicit_only_invocation() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    install_to(base, BUNDLED_SKILLS).unwrap();
    install_explicit_invocation_policies(base).unwrap();

    for name in [
        "do",
        "grind",
        "grind-intake",
        "grind-plan",
        "grind-prework",
        "grind-work",
        "grind-review",
        "grind-integrate",
        "grind-land",
        "grind-cron",
        "video-use",
    ] {
        let body = std::fs::read_to_string(base.join(name).join("SKILL.md")).unwrap();
        assert!(body.contains("disable-model-invocation: true"), "{name}");
        let policy = std::fs::read_to_string(base.join(name).join("agents/openai.yaml")).unwrap();
        assert!(
            policy.contains("allow_implicit_invocation: false"),
            "{name}"
        );
    }

    let user_policy = base.join("grind/agents/openai.yaml");
    std::fs::write(&user_policy, "policy:\n  allow_implicit_invocation: true\n").unwrap();
    install_explicit_invocation_policies(base).unwrap();
    assert_eq!(
        std::fs::read_to_string(user_policy).unwrap(),
        "policy:\n  allow_implicit_invocation: true\n"
    );

    let user_skill = base.join("do/SKILL.md");
    std::fs::write(&user_skill, "user-owned do skill\n").unwrap();
    let do_policy = base.join("do/agents/openai.yaml");
    std::fs::write(&do_policy, "# managed-by: clud\nold policy\n").unwrap();
    install_explicit_invocation_policies(base).unwrap();
    assert_eq!(
        std::fs::read_to_string(do_policy).unwrap(),
        "# managed-by: clud\nold policy\n"
    );
}

#[test]
fn credential_prompt_skill_remains_automatically_discoverable_after_install() {
    let dir = tempdir().unwrap();
    install_to(dir.path(), BUNDLED_SKILLS).unwrap();
    install_explicit_invocation_policies(dir.path()).unwrap();
    let body = std::fs::read_to_string(dir.path().join("clud-secret/SKILL.md")).unwrap();
    assert!(!body.contains("disable-model-invocation: true"));
    assert!(!dir.path().join("clud-secret/agents/openai.yaml").exists());
}

#[test]
fn installs_when_missing() {
    let dir = tempdir().unwrap();
    let report = install_to(dir.path(), &fake_skills()).unwrap();
    assert_eq!(report.installed, vec!["alpha", "beta"]);
    assert!(report.skipped_existing.is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("alpha/SKILL.md")).unwrap(),
        "alpha body\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("beta/SKILL.md")).unwrap(),
        "beta body\n"
    );
}

#[test]
fn skips_existing_and_preserves_user_edits() {
    let dir = tempdir().unwrap();
    let alpha_dir = dir.path().join("alpha");
    std::fs::create_dir_all(&alpha_dir).unwrap();
    std::fs::write(alpha_dir.join("SKILL.md"), "USER EDIT").unwrap();

    let report = install_to(dir.path(), &fake_skills()).unwrap();
    assert_eq!(report.installed, vec!["beta"]);
    assert_eq!(report.skipped_existing, vec!["alpha"]);
    assert_eq!(
        std::fs::read_to_string(alpha_dir.join("SKILL.md")).unwrap(),
        "USER EDIT",
        "existing user content must not be overwritten"
    );
}

#[test]
fn refreshes_a_stale_clud_managed_copy() {
    // The bug this closes: a user who installed clud-issue months ago
    // kept the interview-era body forever, because install skipped any
    // existing file. Editing the bundled skill reached nobody.
    let dir = tempdir().unwrap();
    let managed = [BundledSkill {
        name: "alpha",
        skill_md: "<!-- managed-by: clud -->\nnew body\n",
    }];
    let alpha_dir = dir.path().join("alpha");
    std::fs::create_dir_all(&alpha_dir).unwrap();
    std::fs::write(
        alpha_dir.join("SKILL.md"),
        "<!-- managed-by: clud -->\nold body\n",
    )
    .unwrap();

    let report = install_to(dir.path(), &managed).unwrap();

    assert_eq!(report.refreshed, vec!["alpha"]);
    assert!(report.installed.is_empty());
    assert!(report.skipped_existing.is_empty());
    assert_eq!(
        std::fs::read_to_string(alpha_dir.join("SKILL.md")).unwrap(),
        "<!-- managed-by: clud -->\nnew body\n"
    );

    // Second pass is quiet.
    let again = install_to(dir.path(), &managed).unwrap();
    assert!(again.refreshed.is_empty());
    assert_eq!(again.skipped_existing, vec!["alpha"]);
}

#[test]
fn refresh_never_touches_a_copy_whose_marker_the_user_removed() {
    let dir = tempdir().unwrap();
    let managed = [BundledSkill {
        name: "alpha",
        skill_md: "<!-- managed-by: clud -->\nnew body\n",
    }];
    let alpha_dir = dir.path().join("alpha");
    std::fs::create_dir_all(&alpha_dir).unwrap();
    std::fs::write(alpha_dir.join("SKILL.md"), "mine now\n").unwrap();

    let report = install_to(dir.path(), &managed).unwrap();

    assert_eq!(report.skipped_existing, vec!["alpha"]);
    assert!(report.refreshed.is_empty());
    assert_eq!(
        std::fs::read_to_string(alpha_dir.join("SKILL.md")).unwrap(),
        "mine now\n",
        "dropping the marker is how a user claims ownership"
    );
}

#[test]
fn install_never_overwrites_a_non_utf8_user_skill() {
    let dir = tempdir().unwrap();
    let managed = [BundledSkill {
        name: "alpha",
        skill_md: "<!-- managed-by: clud -->\nnew body\n",
    }];
    let alpha_dir = dir.path().join("alpha");
    std::fs::create_dir_all(&alpha_dir).unwrap();
    std::fs::write(alpha_dir.join("SKILL.md"), b"caf\xe9\n").unwrap();

    let report = install_to(dir.path(), &managed).unwrap();

    assert!(report.installed.is_empty());
    assert_eq!(report.skipped_existing, vec!["alpha"]);
    assert_eq!(
        std::fs::read(alpha_dir.join("SKILL.md")).unwrap(),
        b"caf\xe9\n"
    );
}

#[test]
fn bundled_skills_all_carry_the_ownership_marker() {
    // Refresh keys on the marker; a bundled skill without one would be
    // installed and then never updated again.
    for skill in BUNDLED_SKILLS {
        assert!(
            skill.skill_md.contains(MANAGED_BY_CLUD_MARKER),
            "skill {} is missing the `{MANAGED_BY_CLUD_MARKER}` marker",
            skill.name
        );
    }
}

#[test]
fn idempotent_second_pass_is_a_noop() {
    let dir = tempdir().unwrap();
    let first = install_to(dir.path(), &fake_skills()).unwrap();
    assert_eq!(first.installed.len(), 2);
    let second = install_to(dir.path(), &fake_skills()).unwrap();
    assert!(second.installed.is_empty());
    assert_eq!(second.skipped_existing, vec!["alpha", "beta"]);
}

/// The regression test for #844. `idempotent_second_pass_is_a_noop` above
/// proves the *algorithm* is idempotent using synthetic skills; this proves
/// the *shipped bundle* is, which is where the bug actually lived. A body
/// that fails to round-trip through write-then-read — missing marker, or
/// on-disk bytes that don't compare equal to `skill_md` — makes every launch
/// rewrite the file and print a green `[clud] updated /<name>` that changed
/// nothing. That ran for months against real users.
#[test]
fn real_bundle_install_is_idempotent() {
    let dir = tempdir().unwrap();

    let first = install_to(dir.path(), BUNDLED_SKILLS).unwrap();
    assert_eq!(first.installed.len(), BUNDLED_SKILLS.len());
    assert!(first.refreshed.is_empty());

    let second = install_to(dir.path(), BUNDLED_SKILLS).unwrap();
    assert!(
        second.installed.is_empty(),
        "reinstalled on second pass: {:?}",
        second.installed
    );
    assert!(
        second.refreshed.is_empty(),
        "second pass reported a refresh with nothing changed: {:?}",
        second.refreshed
    );
    assert_eq!(second.skipped_existing.len(), BUNDLED_SKILLS.len());
}

/// A CRLF-vs-LF difference is not a real change. Without the whitespace
/// tolerance in `install_to`, a home whose checkout or editor rewrote line
/// endings would be "refreshed" on every single launch — the same
/// user-visible symptom as #844, from a different cause.
#[test]
fn line_ending_drift_is_not_a_refresh() {
    let dir = tempdir().unwrap();
    let skills = fake_skills();
    install_to(dir.path(), &skills).unwrap();

    let path = dir.path().join("alpha/SKILL.md");
    let crlf = std::fs::read_to_string(&path)
        .unwrap()
        .replace('\n', "\r\n");
    std::fs::write(&path, &crlf).unwrap();

    let report = install_to(dir.path(), &skills).unwrap();
    assert!(
        report.refreshed.is_empty(),
        "line-ending drift was treated as a real change: {:?}",
        report.refreshed
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        crlf,
        "a no-op pass must not rewrite the file"
    );
}

#[test]
fn creates_missing_parent_dirs() {
    let dir = tempdir().unwrap();
    let nested = dir.path().join("a/b/c");
    let report = install_to(&nested, &fake_skills()).unwrap();
    assert_eq!(report.installed, vec!["alpha", "beta"]);
    assert!(nested.join("alpha/SKILL.md").exists());
}

/// The bundled assets must be non-empty — `include_str!` would fail at
/// build time on a missing file, but a 0-byte file would silently ship.
#[test]
fn bundled_skills_are_non_empty() {
    assert!(!BUNDLED_SKILLS.is_empty());
    for s in BUNDLED_SKILLS {
        assert!(!s.skill_md.trim().is_empty(), "skill {} is empty", s.name);
        assert!(
            s.skill_md.contains("managed-by: clud"),
            "skill {} missing managed-by marker",
            s.name
        );
    }
}

#[test]
fn bundled_skill_frontmatter_is_valid_yaml() {
    assert!(!BUNDLED_SKILLS.is_empty());
    for skill in BUNDLED_SKILLS {
        let frontmatter = parse_frontmatter(skill);
        assert_eq!(
            frontmatter.name, skill.name,
            "skill {} frontmatter name must match BUNDLED_SKILLS entry",
            skill.name
        );
        assert!(
            !frontmatter.description.trim().is_empty(),
            "skill {} missing frontmatter description",
            skill.name
        );
        assert!(
            !frontmatter.triggers.is_empty(),
            "skill {} missing frontmatter triggers",
            skill.name
        );
    }
}

#[test]
fn bundled_includes_all_known_skills() {
    let names: Vec<&str> = BUNDLED_SKILLS.iter().map(|s| s.name).collect();
    assert!(names.contains(&"grind"));
    assert!(names.contains(&"do"));
    assert!(names.contains(&"grind-intake"));
    assert!(names.contains(&"grind-plan"));
    assert!(names.contains(&"grind-work"));
    assert!(names.contains(&"grind-review"));
    assert!(names.contains(&"grind-integrate"));
    assert!(names.contains(&"grind-land"));
    assert!(names.contains(&"grind-cron"));
    assert!(!names.contains(&"clud-meta-work"));
    assert!(names.contains(&"clud-issue"));
    assert!(names.contains(&"clud-issue-triage"));
    assert!(names.contains(&"clud-fix-quick"));
    assert!(names.contains(&"clud-review"));
    assert!(names.contains(&"clud-tag-release"));
    assert!(names.contains(&"clud-docker-rust-app-dev"));
    assert!(names.contains(&"clud-windows-trash"));
    assert!(names.contains(&"clud-extern-repos"));
    assert!(names.contains(&"clud-omarchy"));
    assert!(names.contains(&"clud-improve"));
    assert!(names.contains(&"clud-docker-mac-x86"));
    assert!(names.contains(&"clud-docker-recover"));
    assert!(names.contains(&"clud-bosn"));
    assert!(names.contains(&"clud-preloop"));
}

/// Issue #531: the Docker-recovery skill must trigger on the failure
/// modes from the incident (engine pipe absent, WSL/Docker startup
/// failures, Docker VM disk/memory questions) and must carry the
/// non-destructive, config-driven storage guidance from the follow-up
/// comment. Locks the load-bearing guarantees into the embedded body.
#[test]
fn clud_docker_recover_skill_is_non_destructive_and_config_driven() {
    let skill = BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == "clud-docker-recover")
        .expect("clud-docker-recover must be bundled")
        .skill_md;

    for required in [
        "\"$CLUD_EXE\" tool run docker/docker_recover.py doctor",
        "read-only",
        "CustomWslDistroDir",
        "DataFolder",
        "never compacts, prunes, deletes, resets, or",
        "images and volumes",
        "attempts = 10, interval = 2s",
    ] {
        assert!(
            skill.contains(required),
            "clud-docker-recover skill missing required guidance: {required:?}"
        );
    }
}

#[test]
fn bundled_skills_include_red_green_rule() {
    for skill in BUNDLED_SKILLS {
        assert!(
            skill.skill_md.contains("RED -> GREEN"),
            "skill {} must include the RED -> GREEN code-change rule",
            skill.name
        );
    }
}

#[test]
fn clud_improve_files_concrete_reports_without_generic_prompt() {
    let skill = BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == "clud-improve")
        .expect("clud-improve must be bundled")
        .skill_md;

    for required in [
        "Concrete report means file directly",
        "Bare manual invocation asks once",
        "If the skill was auto-selected and the current user message already contains a concrete clud report, use that message as the report.",
    ] {
        assert!(
            skill.contains(required),
            "clud-improve skill missing argument-aware filing guidance: {required}"
        );
    }
}

/// The worktree-teardown guarantees this pins used to live in `clud-pr`.
/// That skill was retired (superseded by the harness `/goal` hook), and
/// `clud-git` is now the single source of truth for the worktree / branch /
/// process-audit playbook — so the guardrail follows the playbook rather
/// than retiring with its old host. The failure this prevents is real and
/// Windows-specific: a blind `rm -rf` retry loop papers over a process
/// holding a file open instead of identifying and stopping it.
#[test]
fn clud_git_teardown_requires_process_audit() {
    let skill = BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == "clud-git")
        .expect("clud-git must be bundled")
        .skill_md;

    for required in [
        "Process audit before destructive removal.",
        "Never blind-loop `rm -rf`.",
        "\"$CLUD_EXE\" trash",
    ] {
        assert!(
            skill.contains(required),
            "clud-git skill missing process-audit teardown guidance: {required}"
        );
    }
}

/// #1481: a clean non-fast-forward push race is recovered (fetch, normal
/// rebase, one retry) instead of blocking, and force-push stays forbidden.
#[test]
fn clud_fix_quick_recovers_push_races_without_force() {
    let skill = BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == "clud-fix-quick")
        .expect("clud-fix-quick must be bundled")
        .skill_md;

    for required in [
        "## Push-Race Recovery",
        "`git rebase origin/<branch>`",
        "`git rebase --abort`",
        "exactly once. A second\n   rejection: stop",
        "No force-push, ever.",
        "Never force-push (`--force`, `--force-with-lease`, `+refspec`)",
    ] {
        assert!(
            skill.contains(required),
            "clud-fix-quick missing push-race recovery guidance: {required}"
        );
    }
    assert!(
        !skill.contains("surface the divergence and stop"),
        "clud-fix-quick must not block on a clean push race"
    );
}

#[test]
fn retired_skills_are_not_also_bundled() {
    // A name in both lists would install and then immediately purge on
    // every launch, or purge-then-reinstall depending on call order.
    let bundled: Vec<&str> = BUNDLED_SKILLS.iter().map(|s| s.name).collect();
    for retired in PURGED_BUNDLED_SKILLS {
        assert!(
            !bundled.contains(retired),
            "{retired} is retired but still bundled"
        );
    }
}

#[test]
fn clud_loop_is_retired() {
    assert!(
        PURGED_BUNDLED_SKILLS.contains(&"clud-loop"),
        "clud-loop must stay in the purge list so upgrades clean old homes"
    );
}

#[test]
fn purge_removes_retired_skill_from_every_backend() {
    let home = tempdir().unwrap();
    let mut dirs = Vec::new();
    for backend in SKILL_BACKENDS {
        let dir = backend.skills_dir(home.path()).join("clud-loop");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: clud-loop\n---\n<!-- managed-by: clud -->\n",
        )
        .unwrap();
        dirs.push(dir);
    }

    let report = purge_retired_bundled_skills(home.path(), &["clud-loop"]);

    assert_eq!(report.removed.len(), SKILL_BACKENDS.len());
    assert!(report.failed.is_empty());
    for dir in dirs {
        assert!(!dir.exists(), "{} should be gone", dir.display());
    }
}

#[test]
fn purge_preserves_a_skill_the_user_owns() {
    let home = tempdir().unwrap();
    let dir = SKILL_BACKENDS[0].skills_dir(home.path()).join("clud-loop");
    std::fs::create_dir_all(&dir).unwrap();
    // No `managed-by: clud` marker: the user hand-wrote or edited this.
    std::fs::write(dir.join("SKILL.md"), "---\nname: clud-loop\n---\nmine\n").unwrap();

    let report = purge_retired_bundled_skills(home.path(), &["clud-loop"]);

    assert_eq!(report.preserved, vec!["clud-loop"]);
    assert!(report.removed.is_empty());
    assert!(dir.join("SKILL.md").is_file());
}

#[test]
fn purge_is_idempotent_and_quiet_when_nothing_is_installed() {
    let home = tempdir().unwrap();
    let report = purge_retired_bundled_skills(home.path(), PURGED_BUNDLED_SKILLS);
    assert!(report.removed.is_empty());
    assert!(report.preserved.is_empty());
    assert!(report.failed.is_empty());
}

/// `clud-pr`, `clud-fix`, `clud-do` and `clud-pr-merge` were retired because
/// the harness `/goal` Stop hook does their orchestration natively. They stay
/// in the purge list so upgrades clean the copies already sitting in user
/// homes — dropping a name from here would strand it there forever, which is
/// the leak [`PURGED_BUNDLED_SKILLS`] exists to close.
///
/// `clud-pr-merge` is the one that needs the explicit pin: it was never in
/// [`BUNDLED_SKILLS`], only in the deleted `skill_install.rs` registry, so
/// nothing else in this file would notice it going missing.
#[test]
fn goal_superseded_skills_stay_retired() {
    for retired in ["clud-pr", "clud-fix", "clud-do", "clud-pr-merge"] {
        assert!(
            PURGED_BUNDLED_SKILLS.contains(&retired),
            "{retired} must stay in the purge list so upgrades clean old homes"
        );
    }
}

/// Two entries with the same `name` would race to write the same
/// `<name>/SKILL.md`, and which one won would depend on registry order.
/// Ported from the deleted `skill_install.rs`, which was the only registry
/// that checked this.
#[test]
fn bundled_skill_names_are_unique() {
    let mut names: Vec<&str> = BUNDLED_SKILLS.iter().map(|s| s.name).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(
        before,
        names.len(),
        "duplicate skill name in BUNDLED_SKILLS"
    );
}

#[test]
fn skill_backends_include_claude_and_codex() {
    let backends: Vec<(Backend, &str, &str, Option<&str>)> = SKILL_BACKENDS
        .iter()
        .map(|b| (b.backend, b.name, b.home_subdir, b.skills_home_subdir))
        .collect();
    assert!(backends.contains(&(Backend::Claude, "Claude Code", ".claude", None)));
    assert!(backends.contains(&(Backend::Codex, "Codex", ".codex", None)));
}

#[test]
fn active_backends_returns_all_when_all_roots_exist() {
    let home = tempdir().unwrap();
    for b in SKILL_BACKENDS {
        std::fs::create_dir_all(home.path().join(b.home_subdir)).unwrap();
    }
    let active = active_backends(home.path());
    assert_eq!(active.len(), SKILL_BACKENDS.len());
}

#[test]
fn active_backends_filters_to_existing_roots() {
    let home = tempdir().unwrap();
    // Only the first backend is installed.
    std::fs::create_dir_all(home.path().join(SKILL_BACKENDS[0].home_subdir)).unwrap();
    let active = active_backends(home.path());
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].home_subdir, SKILL_BACKENDS[0].home_subdir);
}

#[test]
fn active_backends_empty_when_nothing_installed() {
    let home = tempdir().unwrap();
    assert!(active_backends(home.path()).is_empty());
}

/// A file (not a directory) at a backend's home path must not register
/// as installed — `is_dir()` filters it out.
#[test]
fn active_backends_ignores_non_directory_at_root() {
    let home = tempdir().unwrap();
    std::fs::write(
        home.path().join(SKILL_BACKENDS[0].home_subdir),
        b"not a dir",
    )
    .unwrap();
    assert!(active_backends(home.path()).is_empty());
}

#[test]
fn skills_dir_resolves_under_backend_root() {
    let home = tempdir().unwrap();
    let backend = SkillBackend {
        backend: Backend::Claude,
        name: "Test",
        home_subdir: ".testtool",
        skills_home_subdir: None,
        skills_subdir: "skills",
    };
    assert_eq!(
        backend.skills_dir(home.path()),
        home.path().join(".testtool").join("skills")
    );
}

#[test]
fn codex_root_installs_to_codex_skills_dir() {
    let home = tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();

    let codex = ensure_installed_for_backend_at(home.path(), Backend::Codex)
        .unwrap()
        .expect("codex backend should be active");

    assert_eq!(
        codex.0.skills_dir(home.path()),
        home.path().join(".codex/skills")
    );
    assert!(home
        .path()
        .join(".codex/skills/clud-issue/SKILL.md")
        .exists());
    assert!(!home
        .path()
        .join(".agents/skills/clud-issue/SKILL.md")
        .exists());
}

#[test]
fn codex_install_writes_bundled_skill_bodies_byte_for_byte() {
    let home = tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();

    ensure_installed_for_backend_at(home.path(), Backend::Codex)
        .unwrap()
        .expect("codex backend should be active");

    for skill_name in ["clud-issue", "clud-review"] {
        let expected = BUNDLED_SKILLS
            .iter()
            .find(|s| s.name == skill_name)
            .unwrap_or_else(|| panic!("{skill_name} must be bundled"))
            .skill_md;
        let written = std::fs::read_to_string(
            home.path()
                .join(".codex/skills")
                .join(skill_name)
                .join("SKILL.md"),
        )
        .unwrap();
        assert_eq!(written, expected);
    }
}

#[test]
fn purges_managed_stale_agents_skill_copies() {
    let home = tempdir().unwrap();
    let stale_skill = home.path().join(".agents/skills/alpha");
    std::fs::create_dir_all(&stale_skill).unwrap();
    std::fs::write(stale_skill.join("SKILL.md"), "<!-- managed-by: clud -->\n").unwrap();

    let report = purge_stale_agents_skills(home.path(), &fake_skills());

    assert_eq!(report.removed, vec!["alpha"]);
    assert!(report.preserved.is_empty());
    assert!(report.failed.is_empty());
    assert!(!stale_skill.exists());
}

#[test]
fn stale_agents_purge_preserves_unrelated_and_user_authored_content() {
    let home = tempdir().unwrap();
    let stale_root = home.path().join(".agents/skills");
    let custom_skill = stale_root.join("custom");
    let edited_bundled_skill = stale_root.join("alpha");
    let bundled_with_extra = stale_root.join("beta");
    std::fs::create_dir_all(&custom_skill).unwrap();
    std::fs::create_dir_all(&edited_bundled_skill).unwrap();
    std::fs::create_dir_all(&bundled_with_extra).unwrap();
    std::fs::write(
        custom_skill.join("SKILL.md"),
        "<!-- managed-by: clud -->\ncustom\n",
    )
    .unwrap();
    std::fs::write(edited_bundled_skill.join("SKILL.md"), "USER EDIT\n").unwrap();
    std::fs::write(
        bundled_with_extra.join("SKILL.md"),
        "<!-- managed-by: clud -->\n",
    )
    .unwrap();
    std::fs::write(bundled_with_extra.join("notes.txt"), "keep me\n").unwrap();

    let report = purge_stale_agents_skills(home.path(), &fake_skills());

    assert_eq!(report.removed, vec!["beta"]);
    assert_eq!(report.preserved, vec!["alpha"]);
    assert!(custom_skill.join("SKILL.md").exists());
    assert_eq!(
        std::fs::read_to_string(edited_bundled_skill.join("SKILL.md")).unwrap(),
        "USER EDIT\n"
    );
    assert!(!bundled_with_extra.join("SKILL.md").exists());
    assert!(bundled_with_extra.join("notes.txt").exists());
}

#[test]
fn stale_agents_purge_is_idempotent() {
    let home = tempdir().unwrap();
    let stale_skill = home.path().join(".agents/skills/alpha");
    std::fs::create_dir_all(&stale_skill).unwrap();
    std::fs::write(stale_skill.join("SKILL.md"), "<!-- managed-by: clud -->\n").unwrap();

    let first = purge_stale_agents_skills(home.path(), &fake_skills());
    let second = purge_stale_agents_skills(home.path(), &fake_skills());

    assert_eq!(first.removed, vec!["alpha"]);
    assert!(second.removed.is_empty());
    assert!(second.preserved.is_empty());
    assert!(second.failed.is_empty());
}

/// Codex install must not touch a pre-existing `~/.codex/skills/<name>/SKILL.md`
/// that the user has hand-edited.
#[test]
fn codex_install_preserves_user_edited_skill_at_new_path() {
    let home = tempdir().unwrap();
    // Must name a *currently bundled* skill. Pointed at a retired name this
    // test passes vacuously — nothing would have written there anyway.
    let skill_dir = home.path().join(".codex/skills/clud-issue");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "USER EDIT\n").unwrap();

    ensure_installed_for_backend_at(home.path(), Backend::Codex)
        .unwrap()
        .expect("codex backend should be active");

    assert_eq!(
        std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap(),
        "USER EDIT\n",
        "existing user content under ~/.codex/skills/ must not be overwritten"
    );
}

/// Codex install must clean up stale clud-managed copies from `~/.agents/skills/`.
#[test]
fn codex_install_purges_stale_agents_skills() {
    let home = tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();
    let stale = home.path().join(".agents/skills/clud-issue");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("SKILL.md"), "<!-- managed-by: clud -->\nstale\n").unwrap();

    ensure_installed_for_backend_at(home.path(), Backend::Codex)
        .unwrap()
        .expect("codex backend should be active");

    assert!(
        !stale.exists(),
        "stale ~/.agents/skills/ copy must be purged"
    );
}

/// The `skills_home_subdir` field overrides the default skills root.
/// Kept as a unit test of `SkillBackend::skills_dir` so the field's
/// contract stays exercised even when no shipped backend uses it.
#[test]
fn skills_dir_honors_skills_home_subdir_override() {
    let home = tempdir().unwrap();
    let backend = SkillBackend {
        backend: Backend::Codex,
        name: "Test",
        home_subdir: ".sometool",
        skills_home_subdir: Some(".agents"),
        skills_subdir: "skills",
    };
    assert_eq!(
        backend.skills_dir(home.path()),
        home.path().join(".agents").join("skills")
    );
    assert!(!backend.root_exists(home.path()));
    std::fs::create_dir_all(home.path().join(".sometool")).unwrap();
    assert!(backend.root_exists(home.path()));
}

/// Issue #988's four "design questions to settle", pinned in the bodies that
/// answer them. Each was a decision the issue left open, so each is a thing a
/// later edit could quietly undo.
///
/// The install-shape one is the sharpest: preloop's own README documents
/// `curl -fsSL … | sh`, and clud's rule-writing guide uses that exact shape as
/// its canonical example of a pipeline worth blocking. A bundled clud skill
/// telling an agent to run it would have the project contradicting itself in
/// its own assets.
#[test]
fn local_loop_skills_never_pipe_a_download_into_a_shell() {
    for name in ["clud-bosn", "clud-preloop"] {
        let body = BUNDLED_SKILLS
            .iter()
            .find(|skill| skill.name == name)
            .unwrap_or_else(|| panic!("{name} must be bundled"))
            .skill_md;
        // Only fenced blocks: those are the lines an agent copies. Prose that
        // names the shape in order to refuse it is the guidance working, not a
        // violation, and a line-wise scan cannot tell the two apart.
        let mut in_fence = false;
        for line in body.lines() {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if !in_fence {
                continue;
            }
            let piped = line.contains("curl")
                && line.contains('|')
                && (line.contains("sh") || line.contains("bash"));
            assert!(
                !piped,
                "{name} must not instruct a piped-download install: {line}"
            );
        }
    }
}

/// #1273: a roll-up is not complete when the parent merely links to children.
/// The bundled skill must route natural-language roll-up requests to GitHub's
/// native hierarchy and verify the relationship in both directions.
#[test]
fn clud_issue_meta_mode_requires_native_sub_issues() {
    let skill = BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == "clud-issue")
        .expect("clud-issue must be bundled");
    let frontmatter = parse_frontmatter(skill);
    let triggers = frontmatter.triggers.join(" ").to_lowercase();
    for phrase in ["meta issue", "roll up", "parent issue", "combine issues"] {
        assert!(triggers.contains(phrase), "missing meta trigger: {phrase}");
    }

    let body = skill.skill_md;
    for required in [
        "/parent",
        "/sub_issues",
        "sub_issue_id",
        "--paginate",
        "native sub-issue",
    ] {
        assert!(body.contains(required), "meta mode missing {required}");
    }
}

/// Both skills hinge on separating *local iteration* from *the subject matter
/// being Docker or Actions*. The issue calls that boundary the main risk, and a
/// trigger-happy skill becomes noise the model learns to ignore -- so the
/// negative cases live in the body, not just in the frontmatter triggers.
#[test]
fn local_loop_skills_carry_their_negative_cases() {
    for name in ["clud-bosn", "clud-preloop"] {
        let body = BUNDLED_SKILLS
            .iter()
            .find(|skill| skill.name == name)
            .unwrap_or_else(|| panic!("{name} must be bundled"))
            .skill_md;
        assert!(
            body.contains("## Do not use this when"),
            "{name} must spell out when it should not fire"
        );
    }
}

/// Bootstrap is detect-and-instruct: a skill that fires and then strands the
/// agent is worse than one that never fired, and auto-installing a tool the
/// user never asked for is worse still.
#[test]
fn local_loop_skills_detect_before_they_commit() {
    for (name, probe) in [
        ("clud-bosn", "command -v bosn"),
        ("clud-preloop", "command -v preloop"),
    ] {
        let body = BUNDLED_SKILLS
            .iter()
            .find(|skill| skill.name == name)
            .unwrap_or_else(|| panic!("{name} must be bundled"))
            .skill_md;
        assert!(
            body.contains(probe),
            "{name} must check for the tool before committing the agent to it"
        );
    }
}

/// The prerequisites and the capability limits each README states. These are
/// what turn "the skill fired and then it did not work" into a sentence the
/// agent can say before starting.
#[test]
fn local_loop_skills_state_the_limits_their_readmes_state() {
    let bosn = BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == "clud-bosn")
        .expect("clud-bosn must be bundled")
        .skill_md;
    for required in [
        "docker info", // an engine has to exist; bosn manages one, it is not one
        "WSL",         // refused outright, exits non-zero
        "bosn.toml",   // manifest-driven; without one there is nothing to converge
        "partial",     // the Docker front door, so no drop-in replacement claim
        "podman",      // named as not built
        "BuildKit",    // builder cache is outside the label contract
    ] {
        assert!(bosn.contains(required), "clud-bosn must mention {required}");
    }

    let preloop = BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == "clud-preloop")
        .expect("clud-preloop must be bundled")
        .skill_md;
    for required in [
        "sha256",    // the verified-download path that replaces curl | sh
        "5 minutes", // first-run base image build, before anything runs
        "Rosetta",   // amd64 Docker actions unsupported on Apple Silicon goldens
        "curated",   // the base image is not GitHub's, so green here != green there
    ] {
        assert!(
            preloop.contains(required),
            "clud-preloop must mention {required}"
        );
    }
}

/// The one question the issue left genuinely open: whether the two skills
/// compose or conflict. preloop runs each job in its own microVM with its own
/// Docker inside it, which host-side bosn never sees -- so they do not overlap,
/// and the skill says which tool owns which loop rather than leaving the agent
/// to guess.
#[test]
fn local_loop_skills_settle_their_scope_boundary() {
    let preloop = BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == "clud-preloop")
        .expect("clud-preloop must be bundled")
        .skill_md;
    assert!(preloop.contains("clud-bosn"), "the boundary must be named");
    assert!(
        preloop.contains("microVM"),
        "and it must say why they do not overlap"
    );
}

/// #563's safety criteria, pinned.
///
/// This skill tells an agent how to attach a debugger to a running process, so the
/// lines that say *not* to defeat the OS when it refuses are the ones that must
/// survive editing. Each string below corresponds to an acceptance criterion:
/// elevation is explicit and consent-based, nothing is installed, and a refusal
/// ends the attempt rather than starting a workaround.
/// Prose wraps. Collapsing whitespace means these assertions pin the *words*
/// rather than the line breaks, so reflowing a paragraph cannot fail the build
/// and — more importantly — cannot silently stop pinning the guarantee.
fn flattened(skill_name: &str) -> String {
    let skill = BUNDLED_SKILLS
        .iter()
        .find(|s| s.name == skill_name)
        .unwrap_or_else(|| panic!("{skill_name} must be bundled"))
        .skill_md;
    skill.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn os_debug_skill_keeps_its_privilege_boundaries() {
    let skill = flattened("clud-os-debug");
    let skill = skill.as_str();

    for required in [
        // Never weaken a policy to make a diagnosis convenient.
        "ptrace_scope",
        "SIP",
        "stop if the user declines",
        "It installs nothing.",
        // The Windows toolkit is a privileged machine-wide change; propose, do
        // not perform.
        "do not launch an installer unattended",
        // The three instruments must stay distinguishable (#563's last criterion).
        "Live stack dump",
        "Debugger attach",
        "Sampled profile",
        // Reporting contract: an artifact nobody can locate is not evidence.
        "artifact path",
    ] {
        assert!(
            skill.contains(required),
            "clud-os-debug lost a required safety or reporting guarantee: {required:?}"
        );
    }
}

/// Attaching suspends the target. A skill that omits that will get an agent to
/// freeze a user-facing service to answer a question a stack dump would have
/// answered without stopping anything.
#[test]
fn os_debug_skill_warns_that_attaching_suspends_the_target() {
    assert!(flattened("clud-os-debug").contains("suspends the target"));
}

/// `/do`'s body is rendered at invocation by `clud do-prompt` (#1322); the
/// contract lives in `command::do_prompt`, which has its own tests.
#[test]
fn video_use_skill_is_explicit_only_and_bridges_to_the_pinned_checkout() {
    let skill = BUNDLED_SKILLS
        .iter()
        .find(|s| s.name == "video-use")
        .expect("video-use must be bundled (#1866)")
        .skill_md;
    let parsed: serde_yaml::Value =
        serde_yaml::from_str(frontmatter_yaml("video-use", skill)).unwrap();
    assert_eq!(parsed["name"].as_str(), Some("video-use"));
    assert_eq!(parsed["disable-model-invocation"].as_bool(), Some(true));
    assert!(skill.contains(MANAGED_BY_CLUD_MARKER));
    // The pin lives only in video.rs; the skill asks clud for the path.
    assert!(skill.contains("clud video --path"));
    assert!(!skill.contains(crate::video::VIDEO_USE_SHA));
    assert!(skill.contains("ELEVENLABS_API_KEY"));
}

#[test]
fn do_skill_renders_its_prompt_through_clud_do_prompt() {
    let skill = BUNDLED_SKILLS
        .iter()
        .find(|s| s.name == "do")
        .expect("do must be bundled")
        .skill_md;
    // A literal command: Claude Code will not auto-approve one containing a
    // shell expansion such as `${CLUD_EXE:-clud}` (tests/harness H9), and a
    // clud-launched session already puts clud's own directory on PATH.
    assert!(skill.contains("!`clud do-prompt \"$ARGUMENTS\"`"));
    assert!(skill.contains("allowed-tools: Bash(clud do-prompt:*)"));
    assert!(skill.contains("disable-model-invocation: true"));
}

/// `/grind` renders the repo's `./lint` / `./test` report at invocation via
/// `clud grind-scripts` (#1336), and the integrator runs them before every push.
#[test]
fn grind_skill_renders_scripts_through_clud_grind_scripts() {
    let skill = |name: &str| {
        BUNDLED_SKILLS
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} must be bundled"))
            .skill_md
    };
    let grind = skill("grind");
    assert!(grind.contains("!`clud grind-scripts`"));
    assert!(grind.contains("allowed-tools: Bash(clud grind-scripts:*)"));
    let integrate = skill("grind-integrate");
    assert!(
        integrate.contains("run lint before test before every push"),
        "grind-integrate must run lint before test before every push"
    );
}

fn bundled_skill_md(name: &str) -> &'static str {
    BUNDLED_SKILLS
        .iter()
        .find(|skill| skill.name == name)
        .unwrap_or_else(|| panic!("{name} must be bundled"))
        .skill_md
}

/// #1971: triage only reads, so it reads at a revision instead of creating a
/// worktree per issue (each one a cold Cargo target on a Rust repo).
#[test]
fn clud_issue_triage_creates_no_worktree() {
    let skill = bundled_skill_md("clud-issue-triage");
    assert!(
        !skill.contains("git worktree add"),
        "triage must not create worktrees"
    );
    assert!(
        skill.contains("git show"),
        "triage must read code at a revision"
    );
}

/// #1971: `clud-git` leads with the allowed conditions and defaults to
/// branching in place; `/grind` recommends sequential mode for Rust.
#[test]
fn worktrees_need_a_named_condition() {
    let git = bundled_skill_md("clud-git");
    let policy = git
        .find("## Do you need a worktree at all?")
        .expect("clud-git must state the worktree policy");
    let playbook = git.find("## Worktree creation playbook").unwrap();
    assert!(
        policy < playbook,
        "the policy must precede the creation playbook"
    );
    for required in [
        "Concurrent writers must build at the same time",
        "The user explicitly asks",
        "someone else's in-progress work",
        "git switch -c <branch> origin/main",
    ] {
        assert!(git.contains(required), "clud-git missing: {required}");
    }
    let grind = bundled_skill_md("grind");
    assert!(
        grind.contains("For a Rust repo, offer *Sequential* first"),
        "grind must recommend sequential mode for Rust"
    );
}

#[test]
fn pending_upgrade_patch_is_refreshed() {
    let dir = tempdir().unwrap();
    let alpha_dir = dir.path().join("alpha");
    std::fs::create_dir_all(&alpha_dir).unwrap();
    let patch = "---\nname: alpha\n---\n<!-- Local explicit-invocation policy; \
                 preserve until the updated clud binary is installed. -->\nold\n";
    std::fs::write(alpha_dir.join("SKILL.md"), patch).unwrap();

    let report = install_to(dir.path(), &fake_skills()).unwrap();
    assert_eq!(report.refreshed, vec!["alpha"]);
    assert_eq!(
        std::fs::read_to_string(alpha_dir.join("SKILL.md")).unwrap(),
        "alpha body\n"
    );
}
