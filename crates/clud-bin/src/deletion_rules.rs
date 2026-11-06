//! Single source of truth for agent-facing deletion policy (#1461).
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Tighten,
    Loosen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub id: &'static str,
    pub direction: Direction,
    pub effect: Effect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Redirect {
        command: &'static str,
        replacement: &'static str,
        prefix_arg: Option<&'static str>,
    },
    Refuse {
        command: &'static str,
    },
    SafeAlias(&'static str),
    Roots,
}

pub const RULES: &[Rule] = &[
    Rule {
        id: "delete/rm.redirect",
        direction: Direction::Tighten,
        effect: Effect::Redirect {
            command: "rm",
            replacement: "safe-rm",
            prefix_arg: None,
        },
    },
    Rule {
        id: "delete/rmdir.redirect",
        direction: Direction::Tighten,
        effect: Effect::Redirect {
            command: "rmdir",
            replacement: "safe-rm",
            prefix_arg: Some("-d"),
        },
    },
    Rule {
        id: "delete/unlink.redirect",
        direction: Direction::Tighten,
        effect: Effect::Redirect {
            command: "unlink",
            replacement: "safe-rm",
            prefix_arg: None,
        },
    },
    Rule {
        id: "delete/find-delete.refuse",
        direction: Direction::Tighten,
        effect: Effect::Refuse {
            command: "find -delete",
        },
    },
    Rule {
        id: "delete/safe-rm",
        direction: Direction::Loosen,
        effect: Effect::SafeAlias("safe-rm"),
    },
    Rule {
        id: "delete/roots.session",
        direction: Direction::Tighten,
        effect: Effect::Roots,
    },
    Rule {
        id: "delete/roots.checkout",
        direction: Direction::Tighten,
        effect: Effect::Roots,
    },
    Rule {
        id: "delete/roots.task-dirs",
        direction: Direction::Tighten,
        effect: Effect::Roots,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    pub removers: Vec<&'static str>,
    pub claude_denies: Vec<String>,
    pub safe_aliases: Vec<&'static str>,
    pub instructions: String,
}

pub fn generate(rules: &[Rule]) -> Generated {
    let mut removers = Vec::new();
    let mut claude_denies = Vec::new();
    let mut safe_aliases = Vec::new();
    let mut unavailable = Vec::new();
    for rule in rules {
        match rule.effect {
            Effect::Redirect { command, .. } => {
                removers.push(command);
                claude_denies.push(format!("Bash({command} *)"));
                unavailable.push(command);
            }
            Effect::Refuse { command } => unavailable.push(command),
            Effect::SafeAlias(alias) => safe_aliases.push(alias),
            Effect::Roots => {}
        }
    }
    let alias = safe_aliases.first().copied().unwrap_or("safe-rm");
    let mut instructions = format!(
        "Delete with {alias} (moves to trash; limited to this session's allowed locations). {} are unavailable.",
        unavailable.join(", ")
    );
    // #1667: the creation ledger needs a file identity, so `safe-mktemp`
    // refuses on Windows and the guidance is not offered there.
    if crate::platform::host::UNIX {
        instructions.push_str(&format!(
            " Make scratch dirs outside those locations with `{} <path>` so `{alias} -r` can remove them.",
            crate::safe_mktemp::COMMAND
        ));
    }
    Generated {
        removers,
        claude_denies,
        safe_aliases,
        instructions,
    }
}

pub fn generated() -> Generated {
    generate(RULES)
}

pub fn redirect_for(command: &str) -> Option<(&'static str, Option<&'static str>)> {
    redirect_for_in(RULES, command)
}

pub fn redirect_for_in(
    rules: &[Rule],
    command: &str,
) -> Option<(&'static str, Option<&'static str>)> {
    rules.iter().find_map(|rule| match rule.effect {
        Effect::Redirect {
            command: source,
            replacement,
            prefix_arg,
        } if source == command => Some((replacement, prefix_arg)),
        _ => None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Main,
    GrindIntegrator,
    GrindEditor,
    GrindReviewer,
    GrindPlanner,
    GrindPrework,
    GrindLander,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteScope {
    None,
    Session,
    Checkout,
    TaskDirs,
}

pub fn scope(profile: Profile) -> Result<DeleteScope, String> {
    let ids = compile(profile)?;
    if !ids.contains("delete/safe-rm") {
        return Ok(DeleteScope::None);
    }
    let scopes = [
        ("delete/roots.session", DeleteScope::Session),
        ("delete/roots.checkout", DeleteScope::Checkout),
        ("delete/roots.task-dirs", DeleteScope::TaskDirs),
    ];
    let active: Vec<_> = scopes
        .into_iter()
        .filter_map(|(id, scope)| ids.contains(id).then_some(scope))
        .collect();
    match active.as_slice() {
        [only] => Ok(*only),
        _ => Err(format!(
            "profile {profile:?} must select exactly one deletion scope"
        )),
    }
}

pub fn compile(profile: Profile) -> Result<BTreeSet<&'static str>, String> {
    let base = [
        "delete/rm.redirect",
        "delete/rmdir.redirect",
        "delete/unlink.redirect",
        "delete/find-delete.refuse",
        "delete/safe-rm",
        "delete/roots.session",
    ];
    let (add, revoke): (&[&str], &[&str]) = match profile {
        Profile::Main => (&[], &[]),
        Profile::GrindIntegrator => (&["delete/roots.checkout"], &["delete/roots.session"]),
        Profile::GrindEditor | Profile::GrindReviewer => {
            (&["delete/roots.task-dirs"], &["delete/roots.session"])
        }
        Profile::GrindPlanner | Profile::GrindPrework | Profile::GrindLander => {
            (&[], &["delete/safe-rm"])
        }
    };
    compose(&base, add, revoke)
}

/// Base ∪ additions − revocations, independent of declaration order.
fn compose(
    base: &[&'static str],
    add: &[&'static str],
    revoke: &[&'static str],
) -> Result<BTreeSet<&'static str>, String> {
    for id in base.iter().chain(add).chain(revoke) {
        require(id)?;
    }
    let mut ids: BTreeSet<_> = base.iter().chain(add).copied().collect();
    for id in revoke {
        ids.remove(id);
    }
    Ok(ids)
}

fn require(id: &str) -> Result<(), String> {
    let _rule = RULES
        .iter()
        .find(|rule| rule.id == id)
        .ok_or_else(|| format!("unknown deletion rule {id}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_are_unique_frozen_and_well_formed() {
        let ids: Vec<_> = RULES.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec![
                "delete/rm.redirect",
                "delete/rmdir.redirect",
                "delete/unlink.redirect",
                "delete/find-delete.refuse",
                "delete/safe-rm",
                "delete/roots.session",
                "delete/roots.checkout",
                "delete/roots.task-dirs"
            ]
        );
        assert_eq!(
            ids.iter().copied().collect::<BTreeSet<_>>().len(),
            ids.len()
        );
        assert!(ids.iter().all(|id| id.split_once('/').is_some()));
    }
    #[test]
    fn built_in_profiles_compile_and_safe_aliases_are_safe() {
        for profile in [
            Profile::Main,
            Profile::GrindIntegrator,
            Profile::GrindEditor,
            Profile::GrindReviewer,
            Profile::GrindPlanner,
            Profile::GrindPrework,
            Profile::GrindLander,
        ] {
            compile(profile).unwrap();
        }
        let generated = generated();
        assert!(generated
            .safe_aliases
            .iter()
            .all(|name| name.starts_with("safe-")));
        assert!(generated
            .claude_denies
            .iter()
            .all(|rule| !rule.contains("safe-rm")));
    }

    #[test]
    fn mutating_rows_changes_every_generated_consumer() {
        let mut rules = RULES.to_vec();
        rules[0].effect = Effect::Redirect {
            command: "wipe",
            replacement: "safe-wipe",
            prefix_arg: None,
        };
        rules[4].effect = Effect::SafeAlias("safe-wipe");
        let generated = generate(&rules);
        assert!(generated.removers.contains(&"wipe"));
        assert!(generated
            .claude_denies
            .contains(&"Bash(wipe *)".to_string()));
        assert!(generated.safe_aliases.contains(&"safe-wipe"));
        assert!(generated.instructions.contains("safe-wipe"));
        assert!(generated.instructions.contains("wipe"));
        assert_eq!(
            generated.instructions.contains("`safe-mktemp <path>`"),
            crate::platform::host::UNIX
        );
        assert_eq!(redirect_for_in(&rules, "wipe"), Some(("safe-wipe", None)));
        assert_eq!(redirect_for_in(&rules, "rm"), None);
    }

    #[test]
    fn built_in_profiles_have_exact_deletion_scopes() {
        assert_eq!(scope(Profile::Main).unwrap(), DeleteScope::Session);
        assert_eq!(
            scope(Profile::GrindIntegrator).unwrap(),
            DeleteScope::Checkout
        );
        assert_eq!(scope(Profile::GrindEditor).unwrap(), DeleteScope::TaskDirs);
        assert_eq!(
            scope(Profile::GrindReviewer).unwrap(),
            DeleteScope::TaskDirs
        );
        for profile in [
            Profile::GrindPlanner,
            Profile::GrindPrework,
            Profile::GrindLander,
        ] {
            assert_eq!(scope(profile).unwrap(), DeleteScope::None);
        }
    }

    #[test]
    fn composition_is_order_independent_and_unknown_ids_are_errors() {
        let base = ["delete/rm.redirect", "delete/roots.session"];
        let adds = ["delete/safe-rm", "delete/roots.checkout"];
        let revokes = ["delete/roots.session"];
        let forward = compose(&base, &adds, &revokes).unwrap();
        let reverse = compose(
            &["delete/roots.session", "delete/rm.redirect"],
            &["delete/roots.checkout", "delete/safe-rm"],
            &revokes,
        )
        .unwrap();
        assert_eq!(forward, reverse);
        assert!(forward.contains("delete/roots.checkout"));
        assert!(!forward.contains("delete/roots.session"));
        let error = compose(&base, &[], &["delete/unknown"]).unwrap_err();
        assert!(error.contains("delete/unknown"), "{error}");
    }

    #[test]
    fn rule_directions_and_profile_revocation_exception_are_explicit() {
        for rule in RULES {
            let expected = if rule.id == "delete/safe-rm" {
                Direction::Loosen
            } else {
                Direction::Tighten
            };
            assert_eq!(rule.direction, expected, "{}", rule.id);
        }
        for profile in [
            Profile::GrindPlanner,
            Profile::GrindPrework,
            Profile::GrindLander,
        ] {
            let ids = compile(profile).unwrap();
            assert!(!ids.contains("delete/safe-rm"));
            assert_eq!(scope(profile).unwrap(), DeleteScope::None);
        }
    }
}
