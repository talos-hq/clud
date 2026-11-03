//! Compact progress for foreground Claude/Codex print turns.
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Start,
    Thinking,
    Tool,
    Edit,
    Message,
    Done,
    Error,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Thinking => "thinking",
            Self::Tool => "tool",
            Self::Edit => "edit",
            Self::Message => "message",
            Self::Done => "done",
            Self::Error => "error",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Self::Start => "🌱",
            Self::Thinking => "💭",
            Self::Tool => "🔧",
            Self::Edit => "✏",
            Self::Message => "💬",
            Self::Done => "✅",
            Self::Error => "⚠",
        }
    }
}

/// Final answers go to stdout; classified activity goes to stderr.
#[derive(Debug)]
pub struct Output {
    pub answer: bool,
    pub text: String,
    kind: Kind,
}
impl Output {
    fn progress(kind: Kind, text: impl Into<String>) -> Self {
        Self {
            answer: false,
            text: text.into(),
            kind,
        }
    }
    fn answer(text: String) -> Self {
        Self {
            answer: true,
            text,
            kind: Kind::Message,
        }
    }
    pub fn render(&self, utf8: bool, icons: bool) -> String {
        let text = if self.answer {
            self.text.clone()
        } else if utf8 && icons {
            format!("{} [{}] {}", self.kind.icon(), self.kind.label(), self.text)
        } else {
            format!("[{}] {}", self.kind.label(), self.text)
        };
        if utf8 {
            text
        } else {
            text.chars()
                .map(|c| if c.is_ascii() { c } else { '?' })
                .collect()
        }
    }
}

/// Prefer explicit locale declarations; absent declarations use the native
/// console capability. ASCII is deliberately a conservative common subset of
/// legacy encodings: unrepresentable text is replaced, never a formatting panic.
pub fn utf8_output(env: &impl Fn(&str) -> Option<String>, native_utf8: bool) -> bool {
    if env("CLUD_OUTPUT_ASCII").as_deref() == Some("1") {
        return false;
    }
    for name in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(locale) = env(name).filter(|s| !s.is_empty()) {
            let locale = locale.to_ascii_lowercase().replace('-', "");
            return locale.contains("utf8");
        }
    }
    native_utf8
}

/// Stateful decoder: omit tool payloads and private reasoning, deduplicate
/// streamed item updates, and retain the final answer in full.
#[derive(Default)]
pub struct Renderer {
    seen: HashSet<String>,
    completed: HashSet<String>,
    claude_tools: HashMap<String, (String, Value)>,
    patches: crate::prompt_patch::CodexPatches,
    deferred_edits: Vec<Value>,
    codex_completed: bool,
    pending_text: Option<String>,
    thinking: bool,
    done: bool,
    tools: usize,
    edits: usize,
}

impl Renderer {
    pub fn for_process(cwd: std::path::PathBuf) -> Self {
        let root = std::env::var_os("CODEX_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| crate::home::user_home().map(|home| home.join(".codex")));
        Self {
            patches: crate::prompt_patch::CodexPatches::new(root, cwd),
            ..Self::default()
        }
    }

    pub fn observe(&mut self, line: &str) -> Vec<Output> {
        let value = match serde_json::from_str::<Value>(line.trim()) {
            Ok(value) => value,
            Err(_) => {
                return if line.trim().is_empty() {
                    vec![]
                } else {
                    vec![Output::progress(Kind::Message, line.trim())]
                }
            }
        };
        let mut out = Vec::new();
        match string(&value, "type") {
            "system" if string(&value, "subtype") == "init" => out.push(Output::progress(
                Kind::Start,
                format!("Session: {}", string(&value, "model")),
            )),
            "assistant" => self.claude_assistant(&value, &mut out),
            "stream_event" => self.claude_stream(&value, &mut out),
            "user" => self.claude_results(&value, &mut out),
            "result" => {
                let error = value["is_error"].as_bool().unwrap_or(false);
                if error {
                    let detail = value
                        .get("errors")
                        .map(Value::to_string)
                        .unwrap_or_else(|| string(&value, "result").to_string());
                    out.push(Output::progress(Kind::Error, bounded(&detail)));
                } else {
                    let answer = value["result"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| self.pending_text.take());
                    if let Some(answer) = answer {
                        out.push(Output::answer(answer));
                    }
                }
                self.pending_text = None;
                self.finish(error, &mut out);
            }
            "item.started" | "item.updated" | "item.completed" => self.codex_item(&value, &mut out),
            "thread.started" => {
                self.patches.set_thread(string(&value, "thread_id"));
                out.push(Output::progress(Kind::Start, "Codex session"));
            }
            "turn.completed" => {
                self.codex_completed = true;
            }
            "turn.failed" | "error" => {
                let detail = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| string(&value, "message"));
                out.push(Output::progress(Kind::Error, bounded(detail)));
                if string(&value, "type") == "turn.failed" {
                    self.finish(true, &mut out);
                }
            }
            _ => {}
        }
        out
    }

    fn think(&mut self, out: &mut Vec<Output>) {
        if !self.thinking {
            self.thinking = true;
            out.push(Output::progress(Kind::Thinking, "Thinking"));
        }
    }
    fn flush_commentary(&mut self, out: &mut Vec<Output>) {
        if let Some(text) = self.pending_text.take() {
            out.push(Output::progress(Kind::Message, bounded(&text)));
        }
        self.thinking = false;
    }
    fn finish(&mut self, error: bool, out: &mut Vec<Output>) {
        if self.done {
            return;
        }
        self.done = true;
        out.push(Output::progress(
            if error { Kind::Error } else { Kind::Done },
            format!(
                "{} tool call{}, {} edit{}{}",
                self.tools,
                if self.tools == 1 { "" } else { "s" },
                self.edits,
                if self.edits == 1 { "" } else { "s" },
                if error { "; failed" } else { "" }
            ),
        ));
    }
    pub fn end_of_stream(&mut self) -> Vec<Output> {
        let mut out = Vec::new();
        self.patches.load(self.deferred_edits.len());
        for item in std::mem::take(&mut self.deferred_edits) {
            let paths = item["changes"]
                .as_array()
                .map(|changes| {
                    changes
                        .iter()
                        .map(|c| string(c, "path").to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let counts = self.patches.take(&paths);
            out.push(Output::progress(
                Kind::Edit,
                codex_changes(&item, counts.as_deref()),
            ));
        }
        if !self.done {
            if let Some(text) = self.pending_text.take() {
                out.push(Output::answer(text));
            }
            if self.codex_completed {
                self.finish(false, &mut out);
            }
        }
        out
    }
    fn claude_stream(&mut self, value: &Value, out: &mut Vec<Output>) {
        let event = &value["event"];
        if string(event, "type") == "content_block_start"
            && string(&event["content_block"], "type") == "thinking"
        {
            self.think(out);
        }
    }
    fn claude_assistant(&mut self, value: &Value, out: &mut Vec<Output>) {
        let Some(items) = value.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for item in items {
            match string(item, "type") {
                "thinking" | "redacted_thinking" => self.think(out),
                "text" => {
                    self.pending_text = Some(string(item, "text").to_string());
                }
                "tool_use" => {
                    let id = string(item, "id");
                    if !id.is_empty() && !self.seen.insert(id.to_string()) {
                        continue;
                    }
                    self.flush_commentary(out);
                    let name = string(item, "name");
                    let input = &item["input"];
                    self.tools += 1;
                    if !id.is_empty() {
                        self.claude_tools
                            .insert(id.to_string(), (name.to_string(), input.clone()));
                    }
                    let edit = matches!(name, "Edit" | "MultiEdit" | "Write" | "NotebookEdit");

                    let kind = if edit { Kind::Edit } else { Kind::Tool };
                    out.push(Output::progress(kind, claude_tool(name, input)));
                }
                _ => {}
            }
        }
    }
    fn claude_results(&mut self, value: &Value, out: &mut Vec<Output>) {
        let Some(items) = value.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for item in items.iter().filter(|i| string(i, "type") == "tool_result") {
            let Some((name, input)) = self.claude_tools.remove(string(item, "tool_use_id")) else {
                continue;
            };
            let failed = item["is_error"].as_bool() == Some(true);
            let lines = content_lines(item.get("content"))
                .map(output_lines)
                .unwrap_or_else(|| "output line count unavailable".into());
            if failed {
                out.push(Output::progress(
                    Kind::Error,
                    format!(
                        "{name}: {lines}; failed: {}",
                        bounded(&tool_error(&item["content"]))
                    ),
                ));
            } else if matches!(
                name.as_str(),
                "Edit" | "MultiEdit" | "Write" | "NotebookEdit"
            ) {
                self.edits += 1;
                let counts = claude_edit_counts(&name, &input, &value["tool_use_result"]);
                let detail = counts
                    .map(|(a, r)| format!("+{a}/-{r} lines"))
                    .unwrap_or_else(|| "line counts unavailable".into());
                out.push(Output::progress(
                    Kind::Edit,
                    format!(
                        "{name}: {} ({detail}; {})",
                        bounded(string(&input, "file_path")),
                        lines
                    ),
                ));
            } else {
                out.push(Output::progress(Kind::Tool, format!("{name}: {lines}")));
            }
        }
    }
    fn codex_item(&mut self, value: &Value, out: &mut Vec<Output>) {
        let item = &value["item"];
        let complete = string(value, "type") == "item.completed";
        let kind = string(item, "type");
        if kind == "agent_message" {
            if complete {
                self.pending_text = Some(string(item, "text").to_string());
            }
            return;
        }
        if kind == "error" {
            out.push(Output::progress(
                Kind::Error,
                bounded(string(item, "message")),
            ));
            return;
        }
        if kind == "reasoning" {
            self.think(out);
            return;
        }
        if !matches!(
            kind,
            "command_execution"
                | "file_change"
                | "mcp_tool_call"
                | "web_search"
                | "collab_tool_call"
        ) {
            return;
        }
        let id = string(item, "id");
        if complete && !id.is_empty() && !self.completed.insert(id.to_string()) {
            return;
        }
        let fresh = id.is_empty() || self.seen.insert(id.to_string());
        if fresh {
            self.flush_commentary(out);
            self.tools += 1;
            if kind != "file_change" {
                let description = match kind {
                    "command_execution" => format!("Shell: {}", string(item, "command")),
                    "web_search" => format!("Search: {}", string(item, "query")),
                    _ => format!("{}: {}", string(item, "server"), string(item, "tool")),
                };
                out.push(Output::progress(Kind::Tool, bounded(&description)));
            }
        }
        if complete && kind == "file_change" && string(item, "status") != "failed" {
            self.edits += 1;
            self.deferred_edits.push(item.clone());
        }
        let failed = item["exit_code"].as_i64().is_some_and(|c| c != 0)
            || matches!(string(item, "status"), "failed" | "declined")
            || !item["error"].is_null();
        if complete && kind != "file_change" {
            let payload = match kind {
                "command_execution" => item.get("aggregated_output"),
                "mcp_tool_call" => item.pointer("/result/content"),
                "web_search" => item.get("results"),
                _ => item.get("agents_states"),
            };
            let count = content_lines(payload);
            let detail = count
                .map(output_lines)
                .unwrap_or_else(|| "output line count unavailable".into());
            out.push(Output::progress(
                Kind::Tool,
                format!("{}: {detail}", codex_tool_name(item)),
            ));
        }
        if complete && failed {
            let cause = item
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| item["error"].as_str())
                .unwrap_or("");
            let cause = if cause.is_empty() {
                String::new()
            } else {
                format!(": {}", bounded(cause))
            };
            out.push(Output::progress(
                Kind::Error,
                format!(
                    "{kind} {}{}{cause}",
                    if string(item, "status") == "declined" {
                        "declined"
                    } else {
                        "failed"
                    },
                    item["exit_code"]
                        .as_i64()
                        .map(|n| format!(" (exit {n})"))
                        .unwrap_or_default()
                ),
            ));
        }
    }
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
fn bounded(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 120 {
        format!("{}...", line.chars().take(117).collect::<String>())
    } else {
        line
    }
}
fn line_count(text: &str) -> usize {
    text.lines().count()
}
fn claude_tool(name: &str, input: &Value) -> String {
    let detail = match name {
        "Edit" | "MultiEdit" | "Write" | "Read" | "NotebookEdit" => {
            string(input, "file_path").to_string()
        }
        "Bash" => string(input, "command").to_string(),
        "Grep" | "Glob" => string(input, "pattern").to_string(),
        "WebSearch" => string(input, "query").to_string(),
        "WebFetch" => string(input, "url").to_string(),
        _ => string(input, "description").to_string(),
    };
    bounded(&format!("{name}: {detail}"))
}
fn tool_error(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}
fn codex_changes(item: &Value, recorded: Option<&[(usize, usize)]>) -> String {
    let Some(changes) = item["changes"].as_array() else {
        return "Edit: line counts unavailable".into();
    };
    let counts = changes
        .iter()
        .enumerate()
        .map(|(i, change)| {
            change["diff"]
                .as_str()
                .map(crate::prompt_patch::unified_counts)
                .or_else(|| recorded.and_then(|c| c.get(i)).copied())
        })
        .try_fold((0, 0), |(a, r), counts| counts.map(|(b, s)| (a + b, r + s)));
    let detail = counts
        .map(|(a, r)| format!("+{a}/-{r} lines"))
        .unwrap_or_else(|| "line counts unavailable".into());
    if changes.len() == 1 {
        return format!("{} ({detail})", bounded(string(&changes[0], "path")));
    }
    let paths = changes
        .iter()
        .map(|change| string(change, "path"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} files ({detail}): {}", changes.len(), bounded(&paths))
}

fn claude_edit_counts(name: &str, input: &Value, result: &Value) -> Option<(usize, usize)> {
    if let Some(hunks) = result["structuredPatch"]
        .as_array()
        .filter(|h| !h.is_empty())
    {
        return Some(
            hunks
                .iter()
                .flat_map(|h| h["lines"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .fold((0, 0), |(a, r), line| {
                    (
                        a + usize::from(line.starts_with('+')),
                        r + usize::from(line.starts_with('-')),
                    )
                }),
        );
    }
    let original = if name == "Write" && result["type"] == "create" {
        ""
    } else {
        result["originalFile"].as_str()?
    };
    let after = match name {
        "Write" => string(input, "content").to_string(),
        "Edit" => apply_replacement(original, input)?,
        "MultiEdit" => input["edits"]
            .as_array()?
            .iter()
            .try_fold(original.to_string(), |text, edit| {
                apply_replacement(&text, edit)
            })?,
        _ => return None,
    };
    Some(crate::prompt_patch::replacement_counts(original, &after))
}

/// Explicit native output requests remain raw, including machine-readable and
/// file-output modes. Prompt ordering is unchanged.
pub fn configure_command(
    command: &mut Vec<String>,
    backend: crate::backend::Backend,
    passthrough: &[String],
) -> bool {
    use crate::backend::Backend;
    let explicit = passthrough.iter().any(|arg| {
        [
            "--output-format",
            "--json",
            "--experimental-json",
            "--output-last-message",
            "-o",
            "--json-schema",
            "--output-schema",
        ]
        .iter()
        .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
    });
    if explicit {
        return false;
    }
    match backend {
        Backend::Claude => {
            let Some(index) = command.iter().position(|a| a == "-p" || a == "--print") else {
                return false;
            };
            command.splice(
                index..index,
                [
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--include-partial-messages",
                ]
                .map(str::to_string),
            );
        }
        Backend::Codex => {
            let Some(index) = command.iter().position(|a| a == "exec") else {
                return false;
            };
            command.insert(index + 1, "--json".to_string());
        }
        _ => return false,
    }
    true
}

/// Daemon turns store raw JSONL and configure the native protocol themselves.
pub fn remove_auto_flags(command: &mut Vec<String>, backend: crate::backend::Backend) {
    match backend {
        crate::backend::Backend::Claude => {
            if let Some(index) = command.iter().position(|a| a == "--output-format") {
                command.drain(index..index + 4);
            }
        }
        crate::backend::Backend::Codex => {
            if let Some(index) = command.iter().position(|a| a == "--json") {
                command.remove(index);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod accounting_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn actual_patch_beats_replacement_estimates_and_errors_do_not_claim_edits() {
        let mut renderer = Renderer::default();
        renderer.observe(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"e","name":"Edit","input":{"file_path":"a","old_string":"x","new_string":"y","replace_all":true}}]}}).to_string());
        let out=renderer.observe(&json!({"type":"user","tool_use_result":{"structuredPatch":[{"lines":[" context","-x","+y"," context","-x","+y"]}]},"message":{"content":[{"type":"tool_result","tool_use_id":"e","content":"ok"}]}}).to_string());
        assert!(out[0].text.contains("+2/-2 lines"), "{:?}", out);
        renderer.observe(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"f","name":"Edit","input":{"file_path":"a","old_string":"x","new_string":"y"}}]}}).to_string());
        let out=renderer.observe(&json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"f","is_error":true,"content":"not found"}]}}).to_string());
        assert!(out[0].text.contains("failed"));
        assert!(!out[0].text.contains("+1/-1"));
    }
    #[test]
    fn locale_policy_and_lossy_input_never_require_unicode() {
        assert!(!utf8_output(
            &|n| if n == "LC_ALL" {
                Some("C".into())
            } else {
                None
            },
            true
        ));
        assert!(utf8_output(
            &|n| if n == "LANG" {
                Some("en_US.UTF-8".into())
            } else {
                None
            },
            false
        ));
        assert!(!utf8_output(
            &|n| if n == "CLUD_OUTPUT_ASCII" {
                Some("1".into())
            } else {
                None
            },
            true
        ));
        let mut renderer = Renderer::default();
        let output = renderer.observe(&String::from_utf8_lossy(b"warning: bad \xff"));
        assert_eq!(output[0].render(false, false), "[message] warning: bad ?");
    }
}

fn output_lines(count: usize) -> String {
    format!(
        "{count} output {}",
        if count == 1 { "line" } else { "lines" }
    )
}
fn codex_tool_name(item: &Value) -> String {
    match string(item, "type") {
        "command_execution" => "Shell".into(),
        "web_search" => "Search".into(),
        "mcp_tool_call" => format!("{}/{}", string(item, "server"), string(item, "tool")),
        _ => string(item, "tool").to_string(),
    }
}

fn apply_replacement(original: &str, input: &Value) -> Option<String> {
    let old = input["old_string"].as_str()?;
    let new = input["new_string"].as_str()?;
    if !original.contains(old) {
        return None;
    }
    Some(if input["replace_all"].as_bool() == Some(true) {
        original.replace(old, new)
    } else {
        original.replacen(old, new, 1)
    })
}

fn content_lines(content: Option<&Value>) -> Option<usize> {
    let content = content.filter(|v| !v.is_null())?;
    if let Some(text) = content.as_str() {
        return Some(line_count(text));
    }
    if let Some(blocks) = content.as_array() {
        return Some(
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .map(line_count)
                .sum(),
        );
    }
    Some(0)
}

#[cfg(test)]
mod result_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn count_each_text_block_and_distinguish_missing_payload() {
        let content = json!([{"type":"text","text":"one"},{"type":"text","text":"two\n\nthree\n"},{"type":"image","data":"ignored"}]);
        assert_eq!(content_lines(Some(&content)), Some(4));
        assert_eq!(content_lines(None), None);
        assert_eq!(content_lines(Some(&Value::Null)), None);
        assert_eq!(content_lines(Some(&json!([]))), Some(0));
    }
    #[test]
    fn declined_commands_are_explicit_errors_without_a_fake_exit_code() {
        let mut renderer = Renderer::default();
        let out=renderer.observe(&json!({"type":"item.completed","item":{"id":"d","type":"command_execution","command":"deploy","status":"declined","exit_code":null}}).to_string());
        assert!(out
            .iter()
            .any(|o| matches!(o.kind, Kind::Error) && o.text.contains("declined")));
        assert!(out.iter().any(|o| o.text.contains("unavailable")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_classifies_without_dumping_thoughts_arguments_or_results() {
        let mut renderer = Renderer::default();
        let events = [
            json!({"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"private reasoning"},
                {"type":"tool_use","id":"e1","name":"Edit","input":{
                    "file_path":"src/a.rs","old_string":"old\nline\n","new_string":"new\n"}}
            ]}}),
            json!({"type":"user","tool_use_result":{"originalFile":"old\nline\n"},"message":{"content":[{"type":"tool_result","tool_use_id":"e1","content":"huge result"}]}}),
            json!({"type":"result","result":"Final answer\nSecond line","is_error":false}),
        ];
        let mut output: Vec<_> = events
            .iter()
            .flat_map(|v| renderer.observe(&v.to_string()))
            .collect();
        output.extend(renderer.end_of_stream());
        let progress = output
            .iter()
            .filter(|o| !o.answer)
            .map(|o| o.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(progress.contains("Thinking"));
        assert!(
            progress.contains("Edit: src/a.rs (+1/-2 lines; 1 output line)"),
            "{progress}"
        );
        assert!(!progress.contains("private reasoning"));
        assert!(!progress.contains("huge result"));
        assert_eq!(
            output
                .iter()
                .filter(|o| o.answer)
                .map(|o| o.text.as_str())
                .collect::<Vec<_>>(),
            ["Final answer\nSecond line"]
        );
    }

    #[test]
    fn codex_deduplicates_tools_and_preserves_final_answer() {
        let mut renderer = Renderer::default();
        let events = [
            json!({"type":"item.completed","item":{"id":"r","type":"reasoning","text":"private"}}),
            json!({"type":"item.started","item":{"id":"c","type":"command_execution","command":"soldr cargo test","status":"in_progress"}}),
            json!({"type":"item.completed","item":{"id":"c","type":"command_execution","command":"soldr cargo test","aggregated_output":"LOTS OF OUTPUT","exit_code":0}}),
            json!({"type":"item.completed","item":{"id":"f","type":"file_change","changes":[{"path":"a.rs","kind":"update"}]}}),
            json!({"type":"item.completed","item":{"id":"a","type":"agent_message","text":"Complete\nDetails"}}),
            json!({"type":"turn.completed","usage":{"input_tokens":42}}),
        ];
        let mut output: Vec<_> = events
            .iter()
            .flat_map(|v| renderer.observe(&v.to_string()))
            .collect();
        output.extend(renderer.end_of_stream());
        let text = output
            .iter()
            .map(|o| o.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches("soldr cargo test").count(), 1, "{text}");
        assert!(text.contains("a.rs"));
        assert!(!text.contains("LOTS OF OUTPUT"));
        assert_eq!(
            output
                .iter()
                .filter(|o| o.answer)
                .map(|o| o.text.as_str())
                .collect::<Vec<_>>(),
            ["Complete\nDetails"]
        );
    }

    #[test]
    fn ascii_policy_replaces_all_non_ascii_payloads_and_uses_named_fallbacks() {
        let line = Output::progress(Kind::Edit, "café/猫.rs (+1/-2 lines)");
        let rendered = line.render(false, false);
        assert_eq!(rendered, "[edit] caf?/?.rs (+1/-2 lines)");
        assert!(rendered.is_ascii());
        assert!(line.render(true, true).contains("✏"));
        assert!(line.render(true, true).contains("café/猫.rs"));
    }
}
