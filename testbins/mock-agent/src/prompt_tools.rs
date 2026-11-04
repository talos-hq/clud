//! File-editing fixture for the foreground prompt output integration tests.
use serde_json::Value;
use std::io::{self, Write};
use std::path::Path;

pub fn run(path: &Path) {
    let script: Value = serde_json::from_slice(&std::fs::read(path).expect("read tool script"))
        .expect("parse tool script");
    emit(&script["before_events"]);
    for edit in script["edits"].as_array().expect("edit list") {
        std::fs::write(
            edit["path"].as_str().expect("edit path"),
            edit["content"].as_str().expect("edit content"),
        )
        .expect("write edited file");
    }
    if let Some(log) = script["session_log"].as_str() {
        let path = Path::new(log);
        std::fs::create_dir_all(path.parent().expect("session directory"))
            .expect("create session directory");
        let records = script["session_records"]
            .as_array()
            .expect("session records")
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, format!("{records}\n")).expect("write native patch record");
    }
    emit(&script["after_events"]);
}
fn emit(events: &Value) {
    for event in events.as_array().expect("event list") {
        writeln!(io::stdout(), "{event}").expect("emit native event");
        io::stdout().flush().expect("flush event");
    }
}

/// The generic recording report is the mock's final answer. Honor the native
/// stream protocol so launcher tests inspect the same report through rendering.
pub fn emit_report(report: &str, args: &[String]) {
    let claude_stream = args
        .windows(2)
        .any(|p| p[0] == "--output-format" && p[1] == "stream-json");
    let codex_stream = args.iter().any(|a| a == "exec") && args.iter().any(|a| a == "--json");
    if claude_stream {
        println!(
            "{}",
            serde_json::json!({"type":"result","is_error":false,"result":report})
        );
    } else if codex_stream {
        println!(
            "{}",
            serde_json::json!({"type":"item.completed","item":{"id":"recording-report","type":"agent_message","text":report}})
        );
        println!(
            "{}",
            serde_json::json!({"type":"turn.completed","usage":{}})
        );
    } else {
        println!("{report}");
    }
}
