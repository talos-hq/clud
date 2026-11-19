//! Routing of one in-session `gh` call: the read broker in front of
//! `gh.backend = "ghd"`, and the `gh run watch` upgrade.

use std::ffi::OsString;
use std::path::PathBuf;

use super::dispatch::GhRoute;
use super::gh_shim::{plan, run_watch_argv};

fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn ghd() -> GhRoute {
    GhRoute::Ghd(PathBuf::from("/opt/ghd/bin/ghd"))
}

/// The bug: under the ghd backend every call went to ghd before the broker
/// was consulted, and ghd relays REST uncached, so picking ghd switched off
/// the only REST cache and agents drained the 5,000/h core limit.
#[test]
fn ghd_backend_still_sends_rest_gets_through_the_read_broker() {
    for words in [
        &["api", "repos/o/r/actions/runs/7", "--jq", ".status"][..],
        &["api", "repos/o/r/pulls/9"],
        &["api", "-X", "GET", "repos/o/r/commits/abc/check-runs"],
    ] {
        let route = ghd();
        let plan = plan(&route, true, &args(words));
        assert!(plan.read.is_some(), "{words:?} must try the broker first");
        assert!(plan.ghd.is_some(), "a broker miss still goes to ghd");
        assert!(!plan.invalidate, "{words:?} is a read");
    }
}

#[test]
fn ghd_backend_keeps_what_ghd_caches_natively_on_ghd() {
    for words in [
        &["api", "graphql", "-f", "query={viewer{login}}"][..],
        &["pr", "view", "12", "--json", "state"],
        &["api", "repos/{owner}/{repo}/pulls"],
    ] {
        let route = ghd();
        let plan = plan(&route, true, &args(words));
        assert_eq!(plan.read, None, "{words:?} is not a brokerable GET");
        assert!(plan.ghd.is_some(), "{words:?} stays on ghd");
    }
}

#[test]
fn writes_under_ghd_still_invalidate_the_broker() {
    for words in [
        &["pr", "merge", "7", "--squash"][..],
        &["api", "-X", "POST", "repos/o/r/issues", "-f", "title=x"],
        &["run", "rerun", "99"],
    ] {
        let route = ghd();
        let plan = plan(&route, true, &args(words));
        assert_eq!(plan.read, None);
        assert!(plan.ghd.is_some());
        assert!(plan.invalidate, "{words:?} may write");
    }
}

#[test]
fn without_a_broker_nothing_is_brokered_or_invalidated() {
    for route in [ghd(), GhRoute::Gh] {
        let get = plan(&route, false, &args(&["api", "repos/o/r"]));
        assert_eq!(get.read, None);
        let write = plan(&route, false, &args(&["pr", "merge", "7"]));
        assert!(!write.invalidate);
    }
}

#[test]
fn the_gh_backend_routes_as_before() {
    let route = GhRoute::Gh;
    let get = plan(&route, true, &args(&["api", "repos/o/r"]));
    assert!(get.read.is_some() && get.ghd.is_none() && !get.invalidate);
    let write = plan(&route, true, &args(&["pr", "merge", "7"]));
    assert!(write.read.is_none() && write.ghd.is_none() && write.invalidate);
}

fn watch(words: &[&str]) -> Result<Option<Vec<String>>, String> {
    run_watch_argv(words).map(|argv| {
        argv.map(|argv| {
            argv.into_iter()
                .map(|arg| arg.into_string().unwrap())
                .collect()
        })
    })
}

fn tool(rest: &[&str]) -> Option<Vec<String>> {
    let mut argv = vec!["tool", "run", "github/run_watch.py"];
    argv.extend(rest);
    Some(argv.into_iter().map(String::from).collect())
}

/// `gh run watch` re-polls REST every 3 s for the whole run; the bundled
/// watcher polls at least 30 s apart with ETag revalidation.
#[test]
fn run_watch_is_upgraded_to_the_bundled_run_watcher() {
    assert_eq!(watch(&["run", "watch", "123"]), Ok(tool(&["123"])));
    assert_eq!(
        watch(&[
            "run",
            "watch",
            "123",
            "--exit-status",
            "--compact",
            "-R",
            "o/r",
            "-i",
            "5",
        ]),
        Ok(tool(&["123", "--repo", "o/r", "--interval", "5"]))
    );
    assert_eq!(
        watch(&["--repo=o/r", "run", "watch", "--interval=60", "42"]),
        Ok(tool(&["42", "--repo", "o/r", "--interval", "60"]))
    );
}

#[test]
fn run_watch_that_cannot_be_upgraded_is_refused_not_relayed() {
    for words in [
        // No run id: gh would prompt and then poll.
        &["run", "watch"][..],
        &["run", "watch", "--exit-status"],
        &["run", "watch", "abc"],
        &["run", "watch", "1", "2"],
        &["run", "watch", "1", "--unknown"],
        &["run", "watch", "1", "--repo"],
        &["run", "watch", "1", "-i", "-5"],
    ] {
        let refused = watch(words);
        let reason = refused.expect_err(&format!("{words:?} must be refused"));
        assert!(!reason.is_empty());
    }
}

#[test]
fn other_commands_are_not_run_watches() {
    for words in [
        &["run", "view", "123"][..],
        &["run", "list"],
        &["pr", "checks", "1", "--watch"],
        &["api", "repos/o/r/actions/runs/1"],
        &["run", "view", "watch"],
        &[],
    ] {
        assert_eq!(watch(words), Ok(None), "{words:?}");
    }
}

#[test]
fn the_refusal_points_at_the_budget_safe_waiters() {
    let message = super::gh_shim::run_watch_refusal("no run id");
    assert!(message.contains("github/run_watch.py"), "{message}");
    assert!(message.contains("github/pr_merge_watch.py"), "{message}");
    assert!(message.contains("no run id"), "{message}");
}
