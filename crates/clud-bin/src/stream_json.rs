//! Renderer for claude's `--output-format stream-json` events.
//!
//! When `clud loop` runs claude in subprocess launch mode (Windows default,
//! or any explicit `--subprocess`), claude is invoked with
//! `--output-format stream-json --verbose`, which makes it emit one JSON
//! object per line as the turn progresses. This module turns each line into a
//! single human-readable progress line so the operator can see what claude is
//! doing in real time instead of staring at silence until the iteration
//! finishes.
//!
//! Design goals:
//!
//! * **Pure & cheap to test.** `render_line` takes a `&str`, returns
//!   `Option<String>`, and has no side effects. Tests feed canned event
//!   strings — no subscription key or real claude invocation needed.
//! * **Conservative.** Unknown event types are skipped silently; non-JSON
//!   input is passed through verbatim so error messages still surface.
//! * **One line per event.** Multi-line assistant text collapses to its
//!   first non-empty line plus an ellipsis; tool arguments are truncated.

use serde_json::Value;

/// Maximum characters of free-form text (assistant prose, tool command
/// strings) we'll show on a single progress line before truncating.
const MAX_TEXT_CHARS: usize = 160;

/// Render a single stream-json line into a human-readable progress line.
///
/// Returns:
/// * `Some(rendered)` — print this line to stderr (one line of progress).
/// * `None` — drop the event (uninteresting / noise).
pub fn render_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Non-JSON lines pass through verbatim. claude's stream-json mode emits
    // only JSON, but stderr is merged into the same stream by the runner, so
    // a panic backtrace or `npm WARN ...` line shouldn't be silently dropped.
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Some(trimmed.to_string()),
    };

    let msg_type = value.get("type").and_then(Value::as_str)?;
    match msg_type {
        "system" => render_system(&value),
        "assistant" => render_assistant(&value),
        "result" => render_result(&value),
        // An Anthropic-shaped error was previously skipped by the catch-all
        // below, so a turn that failed for a nameable reason -- a drained
        // account, dead credentials -- rendered as nothing at all, and the
        // only user-visible trace was `[claude] error` with no cause.
        "error" => render_error(&value),
        // `user` events carry tool_result payloads — usually long, often
        // duplicating what the tool_use line already conveyed. Skipped.
        _ => None,
    }
}

fn render_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no detail")
        .trim();
    Some(format!("[claude] {kind} · {message}"))
}

fn render_system(value: &Value) -> Option<String> {
    let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
    if subtype != "init" {
        return None;
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    Some(format!("[claude] session start · model={model}"))
}

fn render_assistant(value: &Value) -> Option<String> {
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)?;
    let mut lines: Vec<String> = Vec::new();
    for item in content {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "text" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    if let Some(first_line) = first_nonempty_line(text) {
                        lines.push(format!(
                            "[claude] {}",
                            truncate(&first_line, MAX_TEXT_CHARS)
                        ));
                    }
                }
            }
            "tool_use" => {
                let name = item.get("name").and_then(Value::as_str).unwrap_or("(tool)");
                let input = item.get("input");
                let summary = summarize_tool_input(name, input);
                if summary.is_empty() {
                    lines.push(format!("[tool] {name}"));
                } else {
                    lines.push(format!(
                        "[tool] {name}: {}",
                        truncate(&summary, MAX_TEXT_CHARS)
                    ));
                }
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn render_result(value: &Value) -> Option<String> {
    let duration_ms = value.get("duration_ms").and_then(Value::as_u64);
    let cost = value.get("total_cost_usd").and_then(Value::as_f64);
    let turns = value.get("num_turns").and_then(Value::as_u64);
    let is_error = value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    parts.push(
        (if is_error {
            "[claude] error"
        } else {
            "[claude] done"
        })
        .to_string(),
    );
    if let Some(ms) = duration_ms {
        parts.push(format!("{:.1}s", ms as f64 / 1000.0));
    }
    if let Some(c) = cost {
        parts.push(format!("${c:.2}"));
    }
    if let Some(t) = turns {
        parts.push(format!("{t} turns"));
    }
    Some(parts.join(" · "))
}

/// Aggregate view of one iteration's stream-json events, rendered as an
/// ASCII panel after the iteration ends.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct IterationSummary {
    pub model: Option<String>,
    pub tool_counts: Vec<(String, u32)>,
    pub text_blocks: u32,
    pub turns: Option<u64>,
    pub duration_ms: Option<u64>,
    pub cost_usd: Option<f64>,
    pub is_error: bool,
    pub final_text: Option<String>,
}

impl IterationSummary {
    /// Fold one raw stream-json line into the summary. Non-JSON lines and
    /// unknown events are ignored.
    pub fn observe(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("system") => {
                if let Some(m) = value.get("model").and_then(Value::as_str) {
                    self.model = Some(m.to_string());
                }
            }
            Some("assistant") => self.observe_assistant(&value),
            Some("result") => {
                self.turns = value.get("num_turns").and_then(Value::as_u64);
                self.duration_ms = value.get("duration_ms").and_then(Value::as_u64);
                self.cost_usd = value.get("total_cost_usd").and_then(Value::as_f64);
                self.is_error = value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let Some(r) = value.get("result").and_then(Value::as_str) {
                    self.final_text = first_nonempty_line(r);
                }
            }
            _ => {}
        }
    }

    fn observe_assistant(&mut self, value: &Value) {
        let Some(items) = value.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => self.text_blocks += 1,
                Some("tool_use") => {
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
                    match self.tool_counts.iter_mut().find(|(n, _)| n == name) {
                        Some((_, c)) => *c += 1,
                        None => self.tool_counts.push((name.to_string(), 1)),
                    }
                }
                _ => {}
            }
        }
    }

    /// Build a summary from a whole captured stdout buffer.
    pub fn from_output(raw: &str) -> Self {
        let mut s = Self::default();
        for line in raw.lines() {
            s.observe(line);
        }
        s
    }

    /// Render a fixed-width ASCII panel (pure; no ANSI escapes).
    pub fn render_panel(&self, iteration: u32) -> String {
        const W: usize = 56;
        let mut rows: Vec<String> = Vec::new();
        let status = if self.is_error { "ERROR" } else { "ok" };
        rows.push(format!("iteration {iteration}  status: {status}"));
        if let Some(m) = &self.model {
            rows.push(format!("model: {m}"));
        }
        let mut stats = Vec::new();
        if let Some(t) = self.turns {
            stats.push(format!("{t} turns"));
        }
        if let Some(ms) = self.duration_ms {
            stats.push(format!("{:.1}s", ms as f64 / 1000.0));
        }
        if let Some(c) = self.cost_usd {
            stats.push(format!("${c:.2}"));
        }
        if !stats.is_empty() {
            rows.push(stats.join(" | "));
        }
        if !self.tool_counts.is_empty() {
            let tools: Vec<String> = self
                .tool_counts
                .iter()
                .map(|(n, c)| format!("{n} x{c}"))
                .collect();
            rows.push(format!("tools: {}", tools.join(", ")));
        }
        if let Some(t) = &self.final_text {
            rows.push(format!("result: {t}"));
        }
        let border = format!("+{}+", "-".repeat(W + 2));
        let mut out = border.clone();
        for row in rows {
            out.push('\n');
            out.push_str(&format!("| {:<W$} |", truncate(&row, W)));
        }
        out.push('\n');
        out.push_str(&border);
        out
    }
}

/// Build a short summary string from a tool's `input` object. Empty string
/// means "no informative argument" — the caller falls back to printing just
/// the tool name.
fn summarize_tool_input(name: &str, input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let lookup = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");

    // A `description` field, when present, is the most human-readable
    // summary the agent can provide. Honor it before tool-specific logic.
    let description = lookup("description");

    let body = match name {
        "Bash" => {
            let cmd = lookup("command");
            if !cmd.is_empty() {
                cmd.to_string()
            } else {
                description.to_string()
            }
        }
        "Read" | "Edit" | "Write" | "NotebookEdit" => lookup("file_path").to_string(),
        "Grep" | "Glob" => lookup("pattern").to_string(),
        "WebFetch" | "WebSearch" => lookup("url").to_string(),
        _ => description.to_string(),
    };

    // Collapse newlines/whitespace so the progress line stays single-line.
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_nonempty_line(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

fn truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_line_returns_none() {
        assert!(render_line("").is_none());
        assert!(render_line("   \t\n").is_none());
    }

    #[test]
    fn non_json_line_passes_through_verbatim() {
        // stderr (npm warnings, panics) is merged into the same stream by
        // the subprocess runner. We must not swallow it.
        let line = "npm WARN deprecated foo@1.0";
        assert_eq!(render_line(line).as_deref(), Some(line));
    }

    #[test]
    fn json_array_at_root_passes_through() {
        // serde_json parses `[1,2,3]` successfully but it has no "type"
        // field. The renderer must drop it (it's not a turn event) rather
        // than panic.
        assert!(render_line("[1, 2, 3]").is_none());
    }

    #[test]
    fn assistant_text_renders_as_claude_line() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking at the issue, I'll start by reading the file."}]}}"#;
        assert_eq!(
            render_line(line).as_deref(),
            Some("[claude] Looking at the issue, I'll start by reading the file.")
        );
    }

    #[test]
    fn assistant_text_collapses_to_first_nonempty_line() {
        // Multi-line assistant text would blow up a one-line progress
        // display. We keep only the first non-empty line.
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"\n\nStep 1: read file\n\nStep 2: edit"}]}}"#;
        assert_eq!(
            render_line(line).as_deref(),
            Some("[claude] Step 1: read file")
        );
    }

    #[test]
    fn assistant_text_is_truncated_when_too_long() {
        let long = "x".repeat(500);
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{long}"}}]}}}}"#
        );
        let rendered = render_line(&line).expect("text event must render");
        // Format prefix + ellipsis + bounded chars; assert we didn't dump
        // all 500 chars.
        assert!(rendered.chars().count() < 200, "got {}", rendered.len());
        assert!(
            rendered.ends_with('…'),
            "must end with ellipsis: {rendered}"
        );
        assert!(rendered.starts_with("[claude] "));
    }

    #[test]
    fn tool_use_bash_renders_command_snippet() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test --release","description":"run tests"}}]}}"#;
        assert_eq!(
            render_line(line).as_deref(),
            Some("[tool] Bash: cargo test --release"),
            "Bash should prefer command over description"
        );
    }

    #[test]
    fn tool_use_read_renders_file_path() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"x","name":"Read","input":{"file_path":"/tmp/foo.rs"}}]}}"#;
        assert_eq!(
            render_line(line).as_deref(),
            Some("[tool] Read: /tmp/foo.rs")
        );
    }

    #[test]
    fn tool_use_grep_renders_pattern() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"x","name":"Grep","input":{"pattern":"fn main","path":"src/"}}]}}"#;
        assert_eq!(render_line(line).as_deref(), Some("[tool] Grep: fn main"));
    }

    #[test]
    fn tool_use_unknown_tool_with_description() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"x","name":"SomeCustom","input":{"description":"do a thing"}}]}}"#;
        assert_eq!(
            render_line(line).as_deref(),
            Some("[tool] SomeCustom: do a thing")
        );
    }

    #[test]
    fn tool_use_unknown_tool_without_useful_input() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"x","name":"SomeCustom","input":{}}]}}"#;
        assert_eq!(render_line(line).as_deref(), Some("[tool] SomeCustom"));
    }

    #[test]
    fn assistant_with_both_text_and_tool_use_emits_both_lines() {
        // Real claude turns often pack a text thought + a tool call into
        // one assistant event. Both should surface so the operator sees
        // the chain of reasoning.
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"I'll run the tests."},
            {"type":"tool_use","id":"x","name":"Bash","input":{"command":"cargo test"}}
        ]}}"#;
        let rendered = render_line(line).expect("must render combined event");
        assert!(
            rendered.contains("[claude] I'll run the tests."),
            "{rendered}"
        );
        assert!(rendered.contains("[tool] Bash: cargo test"), "{rendered}");
    }

    #[test]
    fn result_event_renders_summary() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":12500,"num_turns":7,"total_cost_usd":0.1234}"#;
        let rendered = render_line(line).expect("result event must render");
        assert!(rendered.starts_with("[claude] done"), "{rendered}");
        assert!(rendered.contains("12.5s"), "{rendered}");
        assert!(rendered.contains("$0.12"), "{rendered}");
        assert!(rendered.contains("7 turns"), "{rendered}");
    }

    #[test]
    fn result_event_with_is_error_renders_error_summary() {
        let line = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"duration_ms":4000,"num_turns":3}"#;
        let rendered = render_line(line).expect("error result must render");
        assert!(rendered.starts_with("[claude] error"), "{rendered}");
    }

    #[test]
    fn system_init_event_renders_model() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc","tools":["Bash"],"model":"claude-opus-4-7","permissionMode":"default"}"#;
        let rendered = render_line(line).expect("system init must render");
        assert!(rendered.contains("session start"), "{rendered}");
        assert!(rendered.contains("claude-opus-4-7"), "{rendered}");
    }

    #[test]
    fn user_tool_result_event_is_skipped() {
        // claude-stream-json's "user" events carry the bytes the tool
        // returned. They're typically large and redundant with the
        // tool_use line that already printed. Skipped.
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"x","content":"file contents..."}]}}"#;
        assert!(render_line(line).is_none());
    }

    #[test]
    fn unknown_event_type_is_skipped() {
        let line = r#"{"type":"some_future_event","payload":42}"#;
        assert!(render_line(line).is_none());
    }

    /// A failed turn must name its cause. The catch-all used to swallow this
    /// shape, leaving `[claude] error` as the entire user-visible signal.
    #[test]
    fn error_events_are_rendered_with_their_type_and_message() {
        let line = r#"{"type":"error","error":{"type":"billing_error","message":"upstream account quota exhausted"}}"#;
        let rendered = render_line(line).expect("an error line must render");
        assert!(rendered.contains("billing_error"), "{rendered}");
        assert!(rendered.contains("quota exhausted"), "{rendered}");
    }

    #[test]
    fn assistant_event_with_no_content_is_skipped() {
        let line = r#"{"type":"assistant","message":{"content":[]}}"#;
        assert!(render_line(line).is_none());
    }

    #[test]
    fn iteration_summary_aggregates_events() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init","model":"claude-opus-5-5"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Bash","input":{}}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}},{"type":"tool_use","name":"Read","input":{}}]}}"#,
            "\n",
            "not json\n",
            r#"{"type":"result","num_turns":3,"duration_ms":4200,"total_cost_usd":0.12,"is_error":false,"result":"All done\nmore"}"#,
            "\n",
        );
        let s = IterationSummary::from_output(raw);
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(s.tool_counts, vec![("Bash".into(), 2), ("Read".into(), 1)]);
        assert_eq!(s.text_blocks, 1);
        assert_eq!(s.turns, Some(3));
        assert_eq!(s.final_text.as_deref(), Some("All done"));
        let panel = s.render_panel(2);
        assert!(panel.contains("iteration 2  status: ok"), "{panel}");
        assert!(panel.contains("tools: Bash x2, Read x1"), "{panel}");
        assert!(panel.contains("3 turns | 4.2s | $0.12"), "{panel}");
        let widths: Vec<usize> = panel.lines().map(|l| l.chars().count()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "ragged panel: {panel}"
        );
    }

    #[test]
    fn iteration_summary_flags_error_and_empty_output() {
        let s = IterationSummary::from_output(r#"{"type":"result","is_error":true}"#);
        assert!(s.render_panel(1).contains("status: ERROR"));
        assert_eq!(
            IterationSummary::from_output(""),
            IterationSummary::default()
        );
    }
}
