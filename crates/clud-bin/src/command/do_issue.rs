//! Read-only target-issue preflight for `/do` (#2503).
use serde::Deserialize;

use super::do_kind;

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct Comment {
    #[serde(alias = "user")]
    author: Option<Login>,
    body: String,
}

#[derive(Deserialize)]
struct Issue {
    state: String,
    assignees: Vec<Login>,
    labels: Vec<Label>,
    #[serde(default)]
    comments: Vec<Comment>,
}

fn read<T: serde::de::DeserializeOwned>(args: &[&str]) -> Result<T, String> {
    let (code, output) = crate::loop_spec::run_gh_capture(args)?;
    if code != 0 {
        return Err(format!("gh {} failed (exit {code})", args.join(" ")));
    }
    serde_json::from_str(&output).map_err(|error| format!("invalid GitHub data: {error}"))
}

pub(super) fn gather(target: &str) -> Result<String, String> {
    let Some(issue) = do_kind::github_issue(target) else {
        return Err("target is not a GitHub issue URL".into());
    };
    let number = issue.number;
    let slug = format!("{}/{}", issue.owner, issue.repo);
    let mut issue: Issue = read(&[
        "issue",
        "view",
        &issue.number.to_string(),
        "--repo",
        &slug,
        "--json",
        "state,assignees,labels",
    ])?;
    let endpoint = format!("repos/{slug}/issues/{number}/comments?per_page=100");
    let pages: Vec<Vec<Comment>> = read(&["api", "--paginate", "--slurp", &endpoint])?;
    issue.comments = pages.into_iter().flatten().collect();
    let login: Login = read(&["api", "user"])?;
    // An exact repository label is a deliberately conservative readiness signal.
    let labels: Vec<Label> = read(&[
        "label", "list", "--repo", &slug, "--search", "ready", "--limit", "1000", "--json", "name",
    ])?;
    if labels.len() == 1000 {
        return Err("repository ready-label lookup may be truncated".into());
    }
    evaluate(&issue, &login.login, &labels)
}

fn has_ready(labels: &[Label]) -> bool {
    labels
        .iter()
        .any(|label| label.name.eq_ignore_ascii_case("ready"))
}

fn claim(comment: &Comment) -> bool {
    comment.body.lines().any(|line| {
        line.trim_start()
            .to_ascii_lowercase()
            .starts_with("claiming:")
    })
}

fn evaluate(issue: &Issue, login: &str, labels: &[Label]) -> Result<String, String> {
    let reason = if issue.state.eq_ignore_ascii_case("closed") {
        Some("issue is closed (including possible duplicate or not planned)")
    } else if !issue.state.eq_ignore_ascii_case("open") {
        return Err(format!("unrecognized issue state: {}", issue.state));
    } else if issue
        .assignees
        .iter()
        .any(|a| !a.login.eq_ignore_ascii_case(login))
    {
        Some("issue is assigned to someone else")
    } else if issue.comments.iter().filter(|c| claim(c)).any(|c| {
        c.author
            .as_ref()
            .is_some_and(|a| !a.login.eq_ignore_ascii_case(login))
    }) {
        Some("issue has a Claiming: comment from someone else")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Ok(format!(
            "Issue verdict: `blocked`. State: {}. Stop and report: {reason}.",
            issue.state
        ));
    }
    if issue.comments.iter().any(claim) {
        return Err("Claiming: comment ownership is ambiguous (agents can share a login); check manually with the repository claim protocol".into());
    }
    if has_ready(labels) && !has_ready(&issue.labels) {
        return Ok("Issue verdict: `needs-ready`. State: OPEN. Repository has a ready label; stop and report the missing ready label before work.".into());
    }
    Ok("Issue verdict: `ok`. State: OPEN. No conflicting assignee or Claiming: comment found; follow the repository claim protocol before work.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn issue(state: &str, assignee: &str, author: &str, body: &str, ready: bool) -> Issue {
        serde_json::from_value(json!({
            "state": state,
            "assignees": if assignee.is_empty() { vec![] } else { vec![json!({"login":assignee})] },
            "labels": if ready { vec![json!({"name":"ready"})] } else { vec![] },
            "comments": if body.is_empty() { vec![] } else { vec![json!({"author":{"login":author},"body":body})] },
        })).unwrap()
    }

    #[test]
    fn closed_fixture_renders_blocked_in_starting_point() {
        let status = evaluate(&issue("CLOSED", "", "", "", false), "me", &[]);
        let rendered = super::super::do_prompt::render_with_issue(
            "https://github.com/o/r/issues/7",
            &Ok(super::super::do_kind::DoKind::Single),
            &Ok(super::super::do_prompt::BranchFacts::default()),
            &status,
        );
        let starting = rendered.split("## What to do").next().unwrap();
        assert!(starting.contains("Issue verdict: `blocked`"));
        assert!(starting.contains("State: CLOSED"));
        assert!(starting.contains("Stop and report"));
    }

    #[test]
    fn closed_assigned_and_other_claims_block() {
        for fixture in [
            issue("CLOSED", "", "", "", false),
            issue("OPEN", "other", "", "", false),
            issue("OPEN", "", "other", "Claiming: fix", false),
        ] {
            assert!(evaluate(&fixture, "me", &[]).unwrap().contains("`blocked`"));
        }
    }

    #[test]
    fn ready_label_is_required_only_when_repository_has_it() {
        let labels = vec![Label {
            name: "ready".into(),
        }];
        assert!(evaluate(&issue("OPEN", "", "", "", false), "me", &labels)
            .unwrap()
            .contains("`needs-ready`"));
        assert!(evaluate(&issue("OPEN", "me", "", "", true), "me", &labels)
            .unwrap()
            .contains("`ok`"));
        assert!(evaluate(&issue("OPEN", "", "", "", false), "me", &[])
            .unwrap()
            .contains("`ok`"));
    }

    #[test]
    fn shared_login_claims_and_unknown_states_require_manual_check() {
        assert!(evaluate(&issue("OPEN", "", "me", "Claiming: fix", true), "me", &[]).is_err());
        assert!(evaluate(&issue("unexpected", "", "", "", true), "me", &[]).is_err());
    }
}
