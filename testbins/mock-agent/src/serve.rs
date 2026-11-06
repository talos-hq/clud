//! `mock-agent serve`: a scripted Anthropic Messages backend for driving the
//! real Claude Code harness in tests (#1323).
//!
//! Claude Code runs unmodified with `ANTHROPIC_BASE_URL` pointed here. Every
//! `POST /v1/messages` is answered from a JSON script:
//!
//! - **Role.** The first role whose `match` substring appears in the
//!   request's system prompt answers it; a role without `match` is the
//!   fallback (normally the main session).
//! - **Step.** The step index is the number of assistant turns already in the
//!   request's `messages`. Requests are therefore self-describing: no server
//!   state, and Claude Code's extra user messages (reminders after a
//!   `tool_result`, background-agent completions) never desynchronize it.
//! - **Expect.** A step may assert on the `tool_result`s the harness sent back
//!   for the previous step. A failed expectation is logged and answered with a
//!   `MOCK_EXPECT_FAILED` text turn, which ends the conversation visibly.
//!
//! Every request is appended to `--log` as one JSON line. See the script
//! format in `src/README.md`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

struct Server {
    script: Value,
    log: Mutex<Option<std::fs::File>>,
    counter: AtomicU64,
}

pub fn run(args: &[String]) -> i32 {
    let mut script_path: Option<PathBuf> = None;
    let mut port: u16 = 0;
    let mut log_path: Option<PathBuf> = None;
    let mut port_file: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--script" => script_path = iter.next().map(PathBuf::from),
            "--port" => port = iter.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            "--log" => log_path = iter.next().map(PathBuf::from),
            "--port-file" => port_file = iter.next().map(PathBuf::from),
            other => {
                eprintln!("mock-agent serve: unknown argument {other}");
                return 2;
            }
        }
    }
    let script = match script_path {
        Some(path) => match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
        {
            Ok(script) => script,
            Err(error) => {
                eprintln!("mock-agent serve: bad script {}: {error}", path.display());
                return 2;
            }
        },
        None => json!({"roles": []}),
    };
    let log = log_path.map(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open --log")
    });
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("mock-agent serve: bind 127.0.0.1:{port}: {error}");
            return 2;
        }
    };
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    if let Some(path) = port_file {
        let _ = std::fs::write(path, bound.to_string());
    }
    println!("listening {bound}");
    let _ = std::io::stdout().flush();

    let server = Arc::new(Server {
        script,
        log: Mutex::new(log),
        counter: AtomicU64::new(0),
    });
    for stream in listener.incoming().flatten() {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            let _ = handle(&server, stream);
        });
    }
    0
}

fn handle(server: &Server, stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let route = path.split('?').next().unwrap_or("");
        log_http_request(server, &method, route, body.len());
        match (method.as_str(), route) {
            ("POST", p) if p.ends_with("/v1/messages") => {
                let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                respond_messages(server, &mut stream, &request, &body)?;
            }
            ("POST", p) if p.ends_with("/count_tokens") => {
                write_json(&mut stream, 200, &json!({"input_tokens": 1}))?;
            }
            ("GET", p) if p.ends_with("/v1/models") => {
                write_json(&mut stream, 200, &json!({"data": [], "has_more": false}))?;
            }
            _ => write_json(
                &mut stream,
                404,
                &json!({"type": "error", "error": {"type": "not_found_error", "message": route}}),
            )?,
        }
    }
}

fn log_http_request(server: &Server, method: &str, path: &str, body_bytes: usize) {
    if server.script.get("audit_http").and_then(Value::as_bool) != Some(true) {
        return;
    }
    if let Ok(mut guard) = server.log.lock() {
        if let Some(file) = guard.as_mut() {
            let entry = json!({
                "kind": "http_audit", "method": method, "path": path, "body_bytes": body_bytes,
            });
            let _ = writeln!(file, "{entry}");
        }
    }
}

fn response_status(server: &Server, step: &Value, n: u64) -> u16 {
    let status = step
        .get("error")
        .map(|error| error.get("status").and_then(Value::as_u64).unwrap_or(500))
        .or_else(|| {
            (n == 1)
                .then(|| {
                    server
                        .script
                        .get("first_response_status")
                        .and_then(Value::as_u64)
                })
                .flatten()
        })
        .unwrap_or(200);
    u16::try_from(status).unwrap_or(500)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn write_json(stream: &mut TcpStream, status: u16, value: &Value) -> std::io::Result<()> {
    let body = value.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    )?;
    stream.flush()
}

fn system_text(request: &Value) -> String {
    match request.get("system") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn orchestration_tool_schemas(request: &Value) -> Vec<Value> {
    request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| {
            matches!(
                tool.get("name").and_then(Value::as_str),
                Some("Workflow" | "Agent")
            )
        })
        .map(|tool| json!({"name": tool["name"], "input_schema": tool["input_schema"]}))
        .collect()
}

/// Tool results sent since the last assistant turn.
fn recent_tool_results(messages: &[Value]) -> Vec<Value> {
    let mut results = Vec::new();
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            break;
        }
        if let Some(Value::Array(blocks)) = message.get("content") {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let content = match block.get("content") {
                        Some(Value::String(text)) => text.clone(),
                        Some(Value::Array(parts)) => parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    results.push(json!({
                        "is_error": block.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        "content": content,
                    }));
                }
            }
        }
    }
    results.reverse();
    results
}

/// Pick the role, step and action for one request.
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn plan(script: &Value, request: &Value) -> (String, usize, Value, Option<String>) {
    let system = system_text(request);
    let tools: Vec<&str> = request
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let turn = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .count();
    let roles = script
        .get("roles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // `match_prompt`, when present, must also appear in the conversation's
    // messages: it tells apart several agents of one role (one per goal).
    let conversation = serde_json::to_string(&messages).unwrap_or_default();
    // A string, or a list of strings that must all appear.
    let prompt_ok = |r: &Value| match r.get("match_prompt") {
        None => true,
        Some(Value::String(m)) => conversation.contains(m.as_str()),
        Some(Value::Array(all)) => all
            .iter()
            .all(|m| m.as_str().is_some_and(|m| conversation.contains(m))),
        Some(_) => false,
    };
    let matched = roles
        .iter()
        .find(|r| {
            r.get("match")
                .and_then(Value::as_str)
                .is_some_and(|m| system.contains(m))
                && prompt_ok(r)
        })
        .or_else(|| {
            roles
                .iter()
                .find(|r| r.get("match").is_none() && prompt_ok(r))
        });
    let default_text = script
        .get("default_text")
        .and_then(Value::as_str)
        .unwrap_or("MOCK_DONE");
    let Some(role) = matched else {
        return (
            "unmatched".into(),
            turn,
            json!({"text": default_text}),
            None,
        );
    };
    let name = role
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("role")
        .to_string();
    // A role that needs a tool the request does not offer answers with text,
    // so a background title/summary request never receives a tool_use.
    let steps = role
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(step) = steps.get(turn).cloned() else {
        return (name, turn, json!({"text": default_text}), None);
    };
    if let Some(needed) = step
        .get("tool_use")
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
    {
        if !tools.contains(&needed) && needed != "StructuredOutput" {
            return (
                name,
                turn,
                json!({"text": default_text}),
                Some(format!("tool {needed} not offered")),
            );
        }
    }
    let expect_failure = step.get("expect").and_then(|expect| {
        let results = recent_tool_results(&messages);
        let joined = results
            .iter()
            .filter_map(|r| r.get("content").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(want) = expect.get("is_error").and_then(Value::as_bool) {
            let got = results
                .iter()
                .any(|r| r.get("is_error").and_then(Value::as_bool) == Some(true));
            if got != want {
                return Some(format!("expected is_error={want}, got {got}: {joined}"));
            }
        }
        if let Some(needle) = expect.get("content_contains").and_then(Value::as_str) {
            if !joined.contains(needle) {
                return Some(format!(
                    "expected tool_result to contain {needle:?}, got {joined:?}"
                ));
            }
        }
        None
    });
    if let Some(failure) = expect_failure {
        return (
            name,
            turn,
            json!({"text": format!("MOCK_EXPECT_FAILED: {failure}")}),
            Some(failure),
        );
    }
    match resolve_tool_input(step) {
        Ok(step) => (name, turn, step, None),
        Err(error) => (
            name,
            turn,
            json!({"text": format!("MOCK_EXPECT_FAILED: {error}")}),
            Some(error),
        ),
    }
}

const MAX_TOOL_INPUT_BYTES: u64 = 1024 * 1024;

fn read_tool_input(reader: impl Read) -> Result<Value, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_TOOL_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("tool input file: {error}"))?;
    if bytes.len() as u64 > MAX_TOOL_INPUT_BYTES {
        return Err("tool input file exceeds 1 MiB".into());
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("tool input JSON: {error}"))?;
    if !value.is_object() {
        return Err("tool input file must contain a JSON object".into());
    }
    Ok(value)
}

fn resolve_tool_input(mut step: Value) -> Result<Value, String> {
    let Some(path) = step.pointer("/tool_use/input_file").and_then(Value::as_str) else {
        return Ok(step);
    };
    let file = std::fs::File::open(path).map_err(|error| format!("tool input file: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("tool input file is not a regular file".into());
    }
    step["tool_use"]["input"] = read_tool_input(file)?;
    Ok(step)
}

/// Counts from the received HTTP body. Section sizes are compact JSON bytes,
/// not provider tokens; the fingerprint compares schemas without logging them.
fn context_bytes(request: &Value, body: &[u8]) -> Value {
    use std::hash::{Hash, Hasher};

    let section = |name: &str| {
        request
            .get(name)
            .map(|value| serde_json::to_vec(value).expect("serialize parsed JSON"))
            .unwrap_or_default()
    };
    let tools = section("tools");
    let system = section("system");
    let mut fingerprint = std::collections::hash_map::DefaultHasher::new();
    tools.hash(&mut fingerprint);
    let hash = |bytes: &[u8]| {
        let mut state = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut state);
        format!("{:016x}", state.finish())
    };
    json!({
        "request": body.len(),
        "body_fingerprint": hash(body),
        "system": system.len(),
        "system_fingerprint": hash(&system),
        "tools": tools.len(),
        "messages": section("messages").len(),
        "tools_fingerprint": format!("{:016x}", fingerprint.finish()),
    })
}

fn has_forbidden_context(script: &Value, request: &Value) -> bool {
    script
        .get("forbidden_context_markers")
        .and_then(Value::as_array)
        .is_some_and(|markers| {
            markers
                .iter()
                .filter_map(Value::as_str)
                .any(|marker| !marker.is_empty() && contains_context_marker(request, marker))
        })
}

fn contains_context_marker(value: &Value, marker: &str) -> bool {
    match value {
        Value::String(text) => text.contains(marker),
        Value::Array(values) => values
            .iter()
            .any(|value| contains_context_marker(value, marker)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains(marker) || contains_context_marker(value, marker)),
        _ => false,
    }
}

fn log_request(server: &Server, request: &Value, body: &[u8], mut entry: Value) {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let tools: Vec<Value> = request
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("name").cloned())
                .collect()
        })
        .unwrap_or_default();
    entry["unexpected_context"] = json!(has_forbidden_context(&server.script, request));
    entry["context_bytes"] = context_bytes(request, body);
    entry["system_blocks"] = request["system"].clone();
    entry["model"] = request["model"].clone();
    entry["stream"] = request["stream"].clone();
    entry["tools"] = json!(tools);
    entry["provider"] = request["provider"].clone();
    entry["orchestration_tools"] = json!(orchestration_tool_schemas(request));
    entry["tool_results"] = json!(recent_tool_results(&messages));
    entry["system"] = json!(system_text(request));
    entry["messages"] = json!(messages);
    if let Ok(mut guard) = server.log.lock() {
        if let Some(file) = guard.as_mut() {
            let _ = writeln!(file, "{entry}");
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn respond_messages(
    server: &Server,
    stream: &mut TcpStream,
    request: &Value,
    received_body: &[u8],
) -> std::io::Result<()> {
    let (role, turn, step, note) = plan(&server.script, request);
    let n = server.counter.fetch_add(1, Ordering::SeqCst) + 1;
    let t_start = now_ms();
    let status = response_status(server, &step, n);
    // `delay_ms` holds the reply open, so concurrent agents visibly overlap
    // in the log's `t_start`/`t_end` (the /grind concurrency tests).
    if let Some(ms) = step.get("delay_ms").and_then(Value::as_u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("claude");
    log_request(
        server,
        request,
        received_body,
        json!({
            "n": n, "role": role, "turn": turn, "step": step, "note": note,
            "t_start": t_start, "t_end": now_ms(), "response_status": status,
        }),
    );

    if step.get("error").is_some() || status >= 400 {
        return write_json(
            stream,
            status,
            &json!({"type": "error", "error": {"type": "api_error", "message": "mock error"}}),
        );
    }

    let id = format!("msg_mock_{n}");
    let (block, stop) = if let Some(tool) = step.get("tool_use") {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("Bash");
        let input = tool.get("input").cloned().unwrap_or_else(|| json!({}));
        (
            json!({"type": "tool_use", "id": format!("toolu_mock_{n}"), "name": name, "input": input}),
            "tool_use",
        )
    } else if let Some(structured) = step.get("structured") {
        (
            json!({"type": "tool_use", "id": format!("toolu_mock_{n}"), "name": "StructuredOutput", "input": structured}),
            "tool_use",
        )
    } else {
        let text = step
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("MOCK_DONE");
        (json!({"type": "text", "text": text}), "end_turn")
    };

    if request.get("stream").and_then(Value::as_bool) != Some(true) {
        let message = json!({
            "id": id, "type": "message", "role": "assistant", "model": model,
            "content": [block], "stop_reason": stop, "stop_sequence": null,
            "usage": {"input_tokens": 1, "output_tokens": 1},
        });
        return write_json(stream, 200, &message);
    }

    let mut events: Vec<(&str, Value)> = vec![(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": id, "type": "message", "role": "assistant", "model": model, "content": [],
            "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    )];
    if block["type"] == "text" {
        events.push((
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ));
        events.push((
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": block["text"]}}),
        ));
    } else {
        events.push((
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {
                "type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}}),
        ));
        events.push((
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {
                "type": "input_json_delta", "partial_json": block["input"].to_string()}}),
        ));
    }
    events.push((
        "content_block_stop",
        json!({"type": "content_block_stop", "index": 0}),
    ));
    events.push((
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null}, "usage": {"output_tokens": 1}}),
    ));
    events.push(("message_stop", json!({"type": "message_stop"})));

    let mut body = String::new();
    for (name, data) in events {
        body.push_str(&format!("event: {name}\ndata: {data}\n\n"));
    }
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_metrics_measure_received_bytes_without_retaining_content() {
        let raw = br#"{ "system": "private", "tools": [{"name":"Bash"}], "messages": [] }"#;
        let request: Value = serde_json::from_slice(raw).unwrap();
        let metrics = context_bytes(&request, raw);
        assert_eq!(metrics["request"], raw.len());
        assert_eq!(metrics["system"], 9);
        assert_eq!(metrics["messages"], 2);
        assert!(!metrics.to_string().contains("private"));
        assert_eq!(
            metrics["tools_fingerprint"],
            context_bytes(&request, b"")["tools_fingerprint"]
        );
        let changed = json!({"tools": [{"name": "Read"}]});
        assert_ne!(
            metrics["tools_fingerprint"],
            context_bytes(&changed, b"")["tools_fingerprint"]
        );
        assert_eq!(context_bytes(&json!({}), b"{}")["tools"], 0);
        let changed_body = raw
            .iter()
            .map(|byte| if *byte == b'v' { b'w' } else { *byte })
            .collect::<Vec<_>>();
        assert_eq!(raw.len(), changed_body.len());
        assert_ne!(
            metrics["body_fingerprint"],
            context_bytes(&request, &changed_body)["body_fingerprint"]
        );
    }

    #[test]
    fn forbidden_context_checks_every_request_field() {
        let script = json!({"forbidden_context_markers": ["PRIVATE_CANARY"]});
        for request in [
            json!({"system": [{"text": "PRIVATE_CANARY"}]}),
            json!({"system": [{"metadata": "PRIVATE_CANARY"}]}),
            json!({"tools": [{"description": "PRIVATE_CANARY"}]}),
            json!({"metadata": "PRIVATE_CANARY"}),
        ] {
            assert!(has_forbidden_context(&script, &request));
        }
        assert!(!has_forbidden_context(&script, &json!({"messages": []})));
    }

    #[test]
    fn forbidden_context_detects_escaped_text_and_object_keys() {
        let marker = "private \"quote\" \\ path\n雪🙂";
        let script = json!({"forbidden_context_markers": ["", marker]});
        for request in [
            json!({"metadata": [null, false, 17, {"text": format!("prefix {marker} suffix")}]}),
            json!({marker: "unrelated value"}),
        ] {
            assert!(has_forbidden_context(&script, &request));
        }
        assert!(!has_forbidden_context(&script, &json!({"messages": []})));
    }

    #[test]
    fn context_metrics_use_utf8_bytes_instead_of_character_counts() {
        let request = json!({"system": "é漢", "messages": [], "tools": []});
        let bytes = serde_json::to_vec(&request).unwrap();
        let metrics = context_bytes(&request, &bytes);
        assert_eq!(metrics["request"], bytes.len());
        assert_eq!(metrics["system"], 7);
        assert_eq!(metrics["messages"], 2);
    }

    #[test]
    fn dynamic_tool_input_is_a_bounded_json_object() {
        let value =
            read_tool_input(&br#"{"resumeFromRunId":"wf_actual","args":{"gateEpoch":1}}"#[..])
                .unwrap();
        assert_eq!(value["resumeFromRunId"], "wf_actual");
        for bytes in [&b"[]"[..], &b"not json"[..]] {
            assert!(read_tool_input(bytes).is_err());
        }
        assert!(read_tool_input(std::io::repeat(b' ').take(MAX_TOOL_INPUT_BYTES + 1)).is_err());
    }

    #[test]
    fn a_missing_dynamic_input_fails_the_script_instead_of_emitting_empty_arguments() {
        let script = json!({"roles": [{"name": "main", "steps": [{"tool_use": {
            "name": "Workflow", "input_file": format!("missing-workflow-input-{}.json", now_ms())
        }}]}]});
        let (_, _, step, note) = plan(&script, &request("", json!([]), &["Workflow"]));
        assert!(note.unwrap().contains("tool input file"));
        assert!(step.get("tool_use").is_none());
    }

    #[test]
    fn orchestration_schema_log_keeps_only_the_two_native_interfaces() {
        let request = json!({"tools": [
            {"name": "Workflow", "input_schema": {"properties": {"name": {"type": "string"}}}},
            {"name": "Bash", "input_schema": {"private": "unrelated"}},
            {"name": "Agent", "input_schema": {"properties": {"cwd": {"type": "string"}}}}
        ]});
        let schemas = orchestration_tool_schemas(&request);
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0]["name"], "Workflow");
        assert_eq!(
            schemas[1]["input_schema"]["properties"]["cwd"]["type"],
            "string"
        );
        assert!(!serde_json::to_string(&schemas)
            .unwrap()
            .contains("unrelated"));
        assert!(orchestration_tool_schemas(&json!({})).is_empty());
    }

    fn request(system: &str, messages: Value, tools: &[&str]) -> Value {
        json!({
            "system": [{"type": "text", "text": system}],
            "messages": messages,
            "tools": tools.iter().map(|t| json!({"name": t})).collect::<Vec<_>>(),
        })
    }

    fn script() -> Value {
        json!({
            "default_text": "END",
            "roles": [
                {"name": "worker", "match": "You are the /grind editor", "steps": [
                    {"tool_use": {"name": "Bash", "input": {"command": "cargo build"}}},
                    {"expect": {"is_error": true, "content_contains": "BLOCKED"}, "text": "denied as expected"}
                ]},
                {"name": "main", "steps": [
                    {"tool_use": {"name": "Bash", "input": {"command": "echo hi"}}}
                ]}
            ]
        })
    }

    #[test]
    fn role_is_chosen_by_system_prompt_marker_with_a_fallback() {
        let (role, ..) = plan(
            &script(),
            &request("You are the /grind editor.", json!([]), &["Bash"]),
        );
        assert_eq!(role, "worker");
        let (role, ..) = plan(&script(), &request("main session", json!([]), &["Bash"]));
        assert_eq!(role, "main");
    }

    #[test]
    fn step_is_the_number_of_assistant_turns() {
        let first = plan(
            &script(),
            &request(
                "main",
                json!([{"role": "user", "content": "go"}]),
                &["Bash"],
            ),
        );
        assert_eq!(first.2["tool_use"]["name"], "Bash");
        // A reminder message after the tool result does not advance the step.
        let later = json!([
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "tool_use"}]},
            {"role": "user", "content": [{"type": "tool_result", "content": "hi"}]},
            {"role": "user", "content": "<system-reminder>"}
        ]);
        let second = plan(&script(), &request("main", later, &["Bash"]));
        assert_eq!(second.1, 1);
        assert_eq!(second.2["text"], "END");
    }

    #[test]
    fn expect_checks_the_previous_tool_results() {
        let denied = json!([
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "tool_use"}]},
            {"role": "user", "content": [{"type": "tool_result", "is_error": true, "content": "BLOCKED by caps"}]}
        ]);
        let ok = plan(
            &script(),
            &request("You are the /grind editor", denied, &["Bash"]),
        );
        assert_eq!(ok.2["text"], "denied as expected");
        assert!(ok.3.is_none());

        let allowed = json!([
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "tool_use"}]},
            {"role": "user", "content": [{"type": "tool_result", "content": "Compiling"}]}
        ]);
        let failed = plan(
            &script(),
            &request("You are the /grind editor", allowed, &["Bash"]),
        );
        assert!(failed.2["text"]
            .as_str()
            .unwrap()
            .starts_with("MOCK_EXPECT_FAILED"));
    }

    #[test]
    fn match_prompt_separates_agents_of_one_role() {
        let script = json!({"roles": [
            {"name": "plan-a", "match": "planner", "match_prompt": "Goal A:", "steps": [{"text": "A"}]},
            {"name": "plan-b", "match": "planner", "match_prompt": "Goal B:", "steps": [{"text": "B"}]}
        ]});
        let ask = |goal: &str| {
            request(
                "You are the planner",
                json!([{"role": "user", "content": format!("Goal {goal}: do it")}]),
                &[],
            )
        };
        assert_eq!(plan(&script, &ask("A")).0, "plan-a");
        assert_eq!(plan(&script, &ask("B")).0, "plan-b");
    }

    #[test]
    fn a_tool_the_request_does_not_offer_becomes_text() {
        let (_, _, step, note) = plan(&script(), &request("main", json!([]), &[]));
        assert_eq!(step["text"], "END");
        assert!(note.unwrap().contains("not offered"));
    }
}
