use super::*;

fn sub(command: &str) -> Option<String> {
    ghd_substitute(command, ShellDialect::Posix)
}

#[test]
fn absolute_real_gh_becomes_the_shim() {
    assert_eq!(
        sub("/run/current-system/sw/bin/gh pr view 1 --json state").as_deref(),
        Some("gh pr view 1 --json state")
    );
}

#[test]
fn session_variables_naming_the_real_gh_become_the_shim() {
    assert_eq!(
        sub("$CLUD_GH_SHIM_TARGET api user").as_deref(),
        Some("gh api user")
    );
    assert_eq!(
        sub("\"${GHD_REAL_GH}\" pr list").as_deref(),
        Some("gh pr list")
    );
}

#[test]
fn every_use_in_a_compound_is_rewritten() {
    assert_eq!(
        sub("git fetch && /usr/bin/gh pr view 1; (cd x && /usr/bin/gh run list)").as_deref(),
        Some("git fetch && gh pr view 1; (cd x && gh run list)")
    );
}

#[test]
fn plain_gh_and_ghd_are_left_alone() {
    assert_eq!(sub("gh pr view 1"), None);
    assert_eq!(sub("ghd gh pr view 1"), None);
    assert_eq!(sub("/usr/local/bin/ghd gh pr view 1"), None);
}

#[test]
fn a_real_gh_path_used_as_an_argument_declines_the_rewrite() {
    assert_eq!(sub("ls -l /usr/bin/gh"), None);
    assert_eq!(sub("/usr/bin/gh --version; file /usr/bin/gh"), None);
}

#[test]
fn other_dialects_are_not_rewritten() {
    assert_eq!(
        ghd_substitute("C:/tools/gh.exe pr view 1", ShellDialect::PowerShell),
        None
    );
}
