//! `SubagentStart`: give each Claude Code subagent its own bulletin-board
//! identity (docs/architecture/agent-board.md#sub-agents).
//!
//! A Claude Code subagent shares its parent's MCP connections, so the `clud`
//! bridge it calls carries the *parent's* `CLUD_BOARD_ID`. This hook mints a
//! child id under the parent's and hands it to the subagent as
//! `additionalContext`, together with its cwd and parent; the subagent passes
//! it back as the board tools' `agent_id` argument and fills its "working on
//! now" slot with `board_status`.

use serde_json::{json, Value};

/// The harness event this module serves.
pub const SUBAGENT_START_EVENT: &str = "SubagentStart";

/// Longest slice of the harness's agent id kept in the board id.
const AGENT_ID_CHARS: usize = 8;

/// The parent's board id: `CLUD_BOARD_ID`, else `CLUD_SESSION_ID`.
fn parent_board_id() -> String {
    ["CLUD_BOARD_ID", "CLUD_SESSION_ID"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_default()
}

fn slug(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// `<parent>/<agent_type>-<agent id prefix>`; a stable function of the
/// payload, so a re-fired hook names the same agent.
#[must_use]
pub fn subagent_board_id(parent: &str, agent_type: &str, agent_id: &str) -> String {
    let kind = match slug(agent_type) {
        kind if kind.is_empty() => "agent".to_string(),
        kind => kind,
    };
    let short: String = slug(agent_id).chars().take(AGENT_ID_CHARS).collect();
    let leaf = if short.is_empty() {
        kind
    } else {
        format!("{kind}-{short}")
    };
    if parent.is_empty() {
        leaf
    } else {
        format!("{parent}/{leaf}")
    }
}

/// The context handed to the subagent.
#[must_use]
pub fn context(id: &str, parent: &str, cwd: &str) -> String {
    let parent_line = if parent.is_empty() {
        "Your parent has no board id.".to_string()
    } else {
        format!("Your parent agent's board id is `{parent}`; report to it there.")
    };
    format!(
        "You are connected to clud's agent bulletin board (the `clud` MCP server, \
`mcp__clud__board_*` tools). Your board id is `{id}`; your cwd is `{cwd}`. \
{parent_line} You share your parent's MCP connection, so ALWAYS pass \
`agent_id=\"{id}\"` to board_whoami, board_status, board_read and board_post, \
otherwise you speak as your parent. First call \
`board_status(agent_id=\"{id}\", cwd=\"{cwd}\", working_on=\"<one line>\")` to fill \
your \"what I'm working on now\" slot, update it when your step changes, and \
call `board_read(agent_id=\"{id}\")` between major steps for instructions."
    )
}

/// Handle one `SubagentStart` payload; prints the hook output. Always exits 0:
/// a board identity is a convenience, never a reason to block a subagent.
#[must_use]
pub fn run(stdin: &str) -> i32 {
    let Some(output) = output_for(stdin, &parent_board_id()) else {
        return 0;
    };
    println!("{output}");
    0
}

/// The hook's stdout for `stdin`, or `None` when the payload is unusable.
#[must_use]
pub fn output_for(stdin: &str, parent: &str) -> Option<Value> {
    let payload: Value = serde_json::from_str(stdin).ok()?;
    let text = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or("");
    let id = subagent_board_id(parent, text("agent_type"), text("agent_id"));
    let cwd = text("cwd");
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": SUBAGENT_START_EVENT,
            "additionalContext": context(&id, parent, cwd),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_nest_under_the_parent_and_are_slugged() {
        assert_eq!(
            subagent_board_id("sess-1", "Explore", "a1b2c3d4e5f6"),
            "sess-1/explore-a1b2c3d4"
        );
        assert_eq!(subagent_board_id("", "", ""), "agent");
        assert_eq!(
            subagent_board_id("p", "grind editor!", "x"),
            "p/grind-editor-x"
        );
    }

    #[test]
    fn output_carries_id_cwd_and_parent() {
        let stdin = r#"{"hook_event_name":"SubagentStart","agent_id":"abcdef0123","agent_type":"general-purpose","cwd":"/repo","session_id":"s"}"#;
        let out = output_for(stdin, "main-7").unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["hookEventName"],
            SUBAGENT_START_EVENT
        );
        let ctx = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("`main-7/general-purpose-abcdef01`"));
        assert!(ctx.contains("/repo"));
        assert!(ctx.contains("`main-7`"));
        assert!(ctx.contains("board_status(agent_id="));
        assert!(output_for("not json", "p").is_none());
    }
}
