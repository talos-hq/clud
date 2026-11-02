use super::*;

type Pairs = &'static [(&'static str, &'static str)];

fn env(pairs: &[(&str, &str)]) -> EnvSnapshot {
    let mut snapshot = EnvSnapshot::default();
    for (name, value) in pairs {
        let slot = match *name {
            SUBSCRIPTION_PROVIDER_ENV => &mut snapshot.subscription_provider,
            PROVIDER_ENV => &mut snapshot.provider,
            ALLOW_METERED_ENV => &mut snapshot.allow_metered,
            METERED_PROVIDER_ENV => &mut snapshot.metered_provider,
            other => panic!("unknown policy variable {other}"),
        };
        *slot = Some((*value).to_string());
    }
    snapshot
}

/// Allowed rows: (env, resolved provider, metered provider for the warning).
#[test]
fn allowed_configurations_resolve_subscription_first() {
    let table: &[(Pairs, ModelProvider, Option<ModelProvider>)] = &[
        (&[], ModelProvider::Claude, None),
        (
            &[(SUBSCRIPTION_PROVIDER_ENV, "codex")],
            ModelProvider::Codex,
            None,
        ),
        (
            &[(SUBSCRIPTION_PROVIDER_ENV, "native")],
            ModelProvider::Claude,
            None,
        ),
        (&[(PROVIDER_ENV, "native")], ModelProvider::Claude, None),
        (&[(PROVIDER_ENV, "codex")], ModelProvider::Codex, None),
        (
            &[
                (PROVIDER_ENV, "openrouter"),
                (ALLOW_METERED_ENV, "1"),
                (METERED_PROVIDER_ENV, "openrouter"),
            ],
            ModelProvider::OpenRouter,
            Some(ModelProvider::OpenRouter),
        ),
        (
            &[
                (PROVIDER_ENV, "deepseek"),
                (ALLOW_METERED_ENV, "1"),
                (METERED_PROVIDER_ENV, "deepseek"),
            ],
            ModelProvider::DeepSeek,
            Some(ModelProvider::DeepSeek),
        ),
        (
            &[
                (PROVIDER_ENV, "codex"),
                (ALLOW_METERED_ENV, "1"),
                (METERED_PROVIDER_ENV, "codex"),
            ],
            ModelProvider::Codex,
            Some(ModelProvider::Codex),
        ),
        // Consent without CLUD_PROVIDER: the default stays the subscription;
        // only a CLI selection of that provider takes the metered route.
        (
            &[(ALLOW_METERED_ENV, "1"), (METERED_PROVIDER_ENV, "kimi")],
            ModelProvider::Claude,
            Some(ModelProvider::Kimi),
        ),
    ];
    for (pairs, provider, metered) in table {
        let policy = resolve(&env(pairs)).unwrap_or_else(|error| panic!("{pairs:?}: {error}"));
        assert_eq!(policy.provider(), *provider, "{pairs:?}");
        assert_eq!(policy.metered, *metered, "{pairs:?}");
        assert_eq!(policy.warning().is_some(), metered.is_some(), "{pairs:?}");
    }
}

#[test]
fn unset_subscription_provider_applies_the_claude_default() {
    let policy = resolve(&EnvSnapshot::default()).expect("default resolves");
    assert!(policy.subscription_default_applied);
    assert_eq!(policy.subscription_default, ModelProvider::Claude);
    assert!(policy.warning().is_none());
}

#[test]
fn metered_warning_text_is_verbatim() {
    assert_eq!(
        metered_warning(ModelProvider::OpenRouter),
        "[clud] WARNING: METERED PROVIDER IN USE: openrouter\n\
         [clud] WARNING: this launch uses pay-per-token API billing, not your subscription.\n\
         [clud] WARNING: enabled by CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER=openrouter.\n\
         [clud] WARNING: unset both (and CLUD_PROVIDER) to return to your subscription."
    );
    let warning = metered_warning(ModelProvider::Kimi);
    for needle in [
        "kimi",
        "pay-per-token",
        ALLOW_METERED_ENV,
        METERED_PROVIDER_ENV,
    ] {
        assert!(warning.contains(needle), "{needle} missing from {warning}");
    }
}

/// Refused rows: (env, expected error, a substring of its message).
#[test]
fn contradictions_are_errors_with_actionable_messages() {
    let table: &[(Pairs, PolicyError, &str)] = &[
        (
            &[(PROVIDER_ENV, "openrouter")],
            PolicyError::MeteredWithoutOverrides(ModelProvider::OpenRouter),
            "Set CLUD_ALLOW_METERED=1 and CLUD_METERED_PROVIDER=openrouter",
        ),
        (
            &[(PROVIDER_ENV, "openrouter"), (ALLOW_METERED_ENV, "1")],
            PolicyError::AllowWithoutMeteredProvider,
            "without CLUD_METERED_PROVIDER",
        ),
        (
            &[
                (PROVIDER_ENV, "openrouter"),
                (METERED_PROVIDER_ENV, "openrouter"),
            ],
            PolicyError::MeteredWithoutAllow(ModelProvider::OpenRouter),
            "without CLUD_ALLOW_METERED=1",
        ),
        (
            &[
                (PROVIDER_ENV, "claude"),
                (ALLOW_METERED_ENV, "1"),
                (METERED_PROVIDER_ENV, "openrouter"),
            ],
            PolicyError::AllowWithSubscriptionProvider(ModelProvider::Claude),
            "contradiction",
        ),
        (
            &[(PROVIDER_ENV, "native"), (ALLOW_METERED_ENV, "1")],
            PolicyError::AllowWithSubscriptionProvider(ModelProvider::Claude),
            "is a subscription provider",
        ),
        (
            &[(PROVIDER_ENV, "codex"), (ALLOW_METERED_ENV, "1")],
            PolicyError::AllowWithoutMeteredProvider,
            "without CLUD_METERED_PROVIDER",
        ),
        (
            &[
                (PROVIDER_ENV, "openrouter"),
                (ALLOW_METERED_ENV, "1"),
                (METERED_PROVIDER_ENV, "deepseek"),
            ],
            PolicyError::MeteredProviderMismatch {
                provider: "openrouter".into(),
                metered: "deepseek".into(),
            },
            "disagrees with CLUD_PROVIDER=openrouter",
        ),
        (
            &[(SUBSCRIPTION_PROVIDER_ENV, "openrouter")],
            PolicyError::MeteredSubscriptionDefault(ModelProvider::OpenRouter),
            "can only be claude (alias native) or codex",
        ),
        (
            &[(ALLOW_METERED_ENV, "1")],
            PolicyError::AllowWithoutMeteredProvider,
            "without CLUD_METERED_PROVIDER",
        ),
        (
            &[(METERED_PROVIDER_ENV, "kimi")],
            PolicyError::MeteredWithoutAllow(ModelProvider::Kimi),
            "without CLUD_ALLOW_METERED=1",
        ),
        (
            &[(ALLOW_METERED_ENV, "1"), (METERED_PROVIDER_ENV, "claude")],
            PolicyError::AllowWithSubscriptionProvider(ModelProvider::Claude),
            "contradiction",
        ),
        (
            &[
                (PROVIDER_ENV, "kimi"),
                (ALLOW_METERED_ENV, "yes"),
                (METERED_PROVIDER_ENV, "kimi"),
            ],
            PolicyError::InvalidAllowValue("yes".into()),
            "only accepted value is 1",
        ),
        (
            &[(PROVIDER_ENV, "gpt")],
            PolicyError::UnknownProvider {
                var: PROVIDER_ENV,
                value: "gpt".into(),
            },
            "not a known provider",
        ),
    ];
    for (pairs, expected, needle) in table {
        let error = resolve(&env(pairs)).expect_err(&format!("{pairs:?} must be refused"));
        assert_eq!(&error, expected, "{pairs:?}");
        let message = error.to_string();
        assert!(message.contains(needle), "{pairs:?}: {message}");
    }
}

#[test]
fn a_cli_provider_may_not_contradict_clud_provider() {
    let codex = resolve(&env(&[(PROVIDER_ENV, "codex")])).expect("codex resolves");
    assert_eq!(
        reconcile_cli(&codex, Some(ModelProvider::Claude)),
        Err(PolicyError::CliContradictsEnv {
            cli: ModelProvider::Claude,
            env: ModelProvider::Codex,
        })
    );
    assert_eq!(reconcile_cli(&codex, Some(ModelProvider::Codex)), Ok(()));
    assert_eq!(reconcile_cli(&codex, None), Ok(()));
    let unset = resolve(&EnvSnapshot::default()).expect("default resolves");
    assert_eq!(reconcile_cli(&unset, Some(ModelProvider::Codex)), Ok(()));
}

#[test]
fn only_the_explicit_metered_codex_route_permits_the_api_key() {
    let subscription = resolve(&env(&[(PROVIDER_ENV, "codex")])).expect("codex");
    assert!(!subscription.allows_codex_api_key());
    assert!(!resolve(&EnvSnapshot::default())
        .expect("default")
        .allows_codex_api_key());
    let metered = resolve(&env(&[
        (PROVIDER_ENV, "codex"),
        (ALLOW_METERED_ENV, "1"),
        (METERED_PROVIDER_ENV, "codex"),
    ]))
    .expect("metered codex");
    assert!(metered.allows_codex_api_key());
}

#[test]
fn every_resolved_metered_provider_needs_consent_for_exactly_it() {
    let none = resolve(&EnvSnapshot::default()).expect("default");
    for provider in ModelProvider::ALL.iter().copied() {
        let result = enforce_resolved(&none, provider, false);
        if CostOwner::for_provider(provider) == CostOwner::Metered {
            assert_eq!(
                result,
                Err(PolicyError::ResolvedMeteredWithoutConsent(provider)),
                "{provider}"
            );
        } else {
            assert_eq!(result, Ok(()), "{provider}");
        }
    }
    let kimi = resolve(&env(&[
        (ALLOW_METERED_ENV, "1"),
        (METERED_PROVIDER_ENV, "kimi"),
    ]))
    .expect("kimi consent");
    assert_eq!(enforce_resolved(&kimi, ModelProvider::Kimi, false), Ok(()));
    assert_eq!(
        enforce_resolved(&kimi, ModelProvider::OpenRouter, false),
        Err(PolicyError::ResolvedMeteredWithoutConsent(
            ModelProvider::OpenRouter
        ))
    );
}

#[test]
fn failover_metered_consent_needs_the_environment_too() {
    let none = resolve(&EnvSnapshot::default()).expect("default");
    assert_eq!(
        enforce_resolved(&none, ModelProvider::Claude, true),
        Err(PolicyError::FailoverMeteredWithoutConsent)
    );
    assert_eq!(
        enforce_resolved(&none, ModelProvider::Claude, false),
        Ok(())
    );
}

#[test]
fn a_consent_list_serves_a_unified_session_but_not_a_single_route() {
    let list = resolve(&env(&[
        (ALLOW_METERED_ENV, "1"),
        (METERED_PROVIDER_ENV, "openrouter, deepseek"),
    ]))
    .expect("consent list");
    assert!(list.allows(ModelProvider::OpenRouter));
    assert!(list.allows(ModelProvider::DeepSeek));
    assert!(!list.allows(ModelProvider::Kimi));
    assert_eq!(list.metered, None);
    assert_eq!(list.provider(), ModelProvider::Claude);
    let pinned = resolve(&env(&[
        (PROVIDER_ENV, "openrouter"),
        (ALLOW_METERED_ENV, "1"),
        (METERED_PROVIDER_ENV, "openrouter,deepseek"),
    ]))
    .expect_err("CLUD_PROVIDER needs exactly its own consent");
    assert!(pinned.to_string().contains("disagrees"), "{pinned}");
}
