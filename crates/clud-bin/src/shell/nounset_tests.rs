use super::*;
use kernal_api::platform::fs::TemporaryDirectory;

#[test]
fn arms_nounset_by_default() {
    let tmp = TemporaryDirectory::new().unwrap();
    let overrides = env_overrides_at(tmp.path(), false, None);

    let bash_env = overrides
        .iter()
        .find(|(key, _)| key == BASH_ENV_KEY)
        .expect("BASH_ENV must be set");
    assert!(Path::new(&bash_env.1).is_file(), "the file must exist");
    assert!(
        std::fs::read_to_string(&bash_env.1)
            .unwrap()
            .contains("set -u"),
        "the whole point is that the sourced file enables nounset"
    );
    // Nothing to chain, so no stash key.
    assert!(!overrides.iter().any(|(key, _)| key == PREV_KEY));
}

#[test]
fn identifies_only_a_clud_generated_nounset_script() {
    let tmp = TemporaryDirectory::new().unwrap();
    let generated = env_overrides_at(tmp.path(), false, None)
        .into_iter()
        .find(|(key, _)| key == BASH_ENV_KEY)
        .map(|(_, value)| value)
        .expect("generated BASH_ENV");
    assert!(is_clud_nounset_script(&generated));

    let user_owned = tmp.path().join("user-bash-env.sh");
    std::fs::write(
        &user_owned,
        format!("{GENERATED_MARKER} user-owned extension\necho user-startup\n"),
    )
    .unwrap();
    assert!(!is_clud_nounset_script(
        user_owned.to_str().expect("UTF-8 temporary path")
    ));
}

#[test]
fn opting_out_leaves_the_shell_alone() {
    let tmp = TemporaryDirectory::new().unwrap();
    assert!(
        env_overrides_at(tmp.path(), true, None).is_empty(),
        "{OPT_OUT_KEY} must restore stock shell behaviour"
    );
    // And it must not have written a file the shell could still find.
    assert!(!tmp.path().join(FILE_NAME).exists());
}

#[test]
fn an_inherited_bash_env_is_chained_not_clobbered() {
    // Skipping when the user has their own would be a silent coverage gap;
    // overwriting it would break their setup. The file sources theirs after
    // arming nounset.
    let tmp = TemporaryDirectory::new().unwrap();
    let overrides = env_overrides_at(tmp.path(), false, Some("/home/u/mine.sh".to_string()));

    assert_eq!(
        overrides
            .iter()
            .find(|(key, _)| key == PREV_KEY)
            .map(|(_, value)| value.as_str()),
        Some("/home/u/mine.sh"),
        "the previous BASH_ENV must be stashed for the generated file to source"
    );
    let body = script_body();
    assert!(body.contains(&format!(". \"${{{PREV_KEY}}}\"")), "{body}");
}

#[test]
fn our_own_path_is_never_chained_to_itself() {
    // A clud session launched from inside a clud session would otherwise stash
    // our path as "previous" and the file would source itself forever.
    let tmp = TemporaryDirectory::new().unwrap();
    let first = env_overrides_at(tmp.path(), false, None);
    let ours = first
        .iter()
        .find(|(key, _)| key == BASH_ENV_KEY)
        .map(|(_, value)| value.clone())
        .unwrap();

    let second = env_overrides_at(tmp.path(), false, Some(ours));

    assert!(
        !second.iter().any(|(key, _)| key == PREV_KEY),
        "our own path must not be chained as the previous one"
    );
}

#[test]
fn a_differently_spelled_path_to_our_own_shim_is_not_chained() {
    // The self-loop guard compares strings, so a value that canonicalizes to
    // our shim but is spelled differently (a `..` segment, a symlink, a
    // trailing slash) is treated as "not ours" and chained back to itself.
    // bash then sources the shim, which sources itself, recursively (#1201).
    let tmp = TemporaryDirectory::new().unwrap();
    // Write the shim, then address it through a `..` segment so the guard's
    // string comparison would fail but canonicalization must succeed.
    env_overrides_at(tmp.path(), false, None);

    let sub = tmp.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let dotted = sub.join("..").join(FILE_NAME);

    let second = env_overrides_at(tmp.path(), false, Some(dotted.display().to_string()));

    assert!(
        !second.iter().any(|(key, _)| key == PREV_KEY),
        "a path to our shim spelled with a `..` segment must not be chained"
    );
}

#[test]
fn an_empty_inherited_value_is_not_chained() {
    let tmp = TemporaryDirectory::new().unwrap();
    let overrides = env_overrides_at(tmp.path(), false, Some(String::new()));
    assert!(!overrides.iter().any(|(key, _)| key == PREV_KEY));
}

/// The user's own startup file must be sourced *before* nounset is armed.
///
/// Their file was written for a stock shell, and stock startup files routinely
/// test unset variables (`[ -z "$PS1" ]`, `$SSH_AUTH_SOCK`). Arming first would
/// abort every tool call with an error pointing at *their* file, for a policy
/// they never opted into — a worse outcome than not shipping this at all.
/// Ordering is the entire mitigation, so it gets a test.
#[test]
fn the_users_own_file_is_sourced_before_nounset_is_armed() {
    let body = script_body();
    let set_u = body.find("set -u").expect("nounset must be enabled");
    let chain = body
        .find(&format!(". \"${{{PREV_KEY}}}\""))
        .expect("the chained source must be present");
    assert!(
        chain < set_u,
        "sourcing theirs after `set -u` breaks stock startup files: {body}"
    );
    // The `:-` form costs nothing and stays correct if their file (or a later
    // edit) turns nounset on above this line.
    assert!(
        body.contains(&format!("${{{PREV_KEY}:-}}")),
        "the readability test must use the :- form: {body}"
    );
}

#[test]
fn opt_out_parsing_matches_the_completion_guard_precedent() {
    for truthy in ["1", "true", "YES", " on "] {
        assert!(is_truthy(truthy), "{truthy} should opt out");
    }
    for falsy in ["0", "false", "no", "off", "", "maybe"] {
        assert!(!is_truthy(falsy), "{falsy} should not opt out");
    }
}
