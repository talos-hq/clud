//! #2565: Failed-turn boundaries must come from bridge state, never error text.
use super::*;
use serde_json::{json, Value};

fn message(role: &str, text: &str) -> Value {
    json!({"role": role, "content": text})
}

fn send(bridge: &BridgeHandle, messages: &[Value], streaming: bool) -> String {
    let body = json!({
        "model": "claude-sonnet-4-6", "max_tokens": 64, "stream": streaming,
        "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        "messages": messages,
    });
    request(
        bridge.socket_addr(),
        &authorized_with_headers(
            "POST",
            "/v1/messages",
            bridge.bearer_token(),
            &body.to_string(),
            &[("X-Claude-Code-Session-Id", "failed-turn-prompts")],
        ),
    )
}

fn inputs(upstream: &FakeResponses) -> Vec<Value> {
    upstream
        .requests()
        .iter()
        .map(|raw| serde_json::from_str::<Value>(captured_body(raw)).unwrap()["input"].clone())
        .collect()
}

fn text_count(input: &Value, text: &str) -> usize {
    input
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| {
            item["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["text"] == text))
        })
        .count()
}

fn recover_prompt(existing: bool, streaming: bool) {
    let mut replies = Vec::new();
    if existing {
        replies.push(Some(recovery_success_response()));
    }
    replies.extend([
        Some(visible_transport_failure_response()),
        Some(recovery_success_response()),
        None,
    ]);
    let upstream = FakeResponses::start_with_responses(replies);
    let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
    let mut messages = Vec::new();
    if existing {
        messages.push(message("user", "already seen"));
        assert_eq!(status(&send(&bridge, &messages, streaming)), 200);
        messages.push(message("assistant", "recovered"));
    }
    messages.push(message("user", "do X"));
    let failed = send(&bridge, &messages, streaming);
    assert!(failed.contains("error"), "{failed}");
    messages.extend([
        message("assistant", "API Error: synthetic"),
        message("user", "retry"),
    ]);
    assert_eq!(status(&send(&bridge, &messages, streaming)), 200);
    messages.extend([message("assistant", "recovered"), message("user", "next")]);
    assert_eq!(status(&send(&bridge, &messages, streaming)), 200);
    let captured = inputs(&upstream);
    for input in &captured[captured.len() - 2..] {
        assert_eq!(text_count(input, "do X"), 1, "lost failed prompt: {input}");
        assert_eq!(
            text_count(input, "API Error: synthetic"),
            0,
            "synthetic reply stored: {input}"
        );
        if existing {
            assert_eq!(text_count(input, "already seen"), 1, "duplicated: {input}");
        }
    }
    bridge.shutdown().unwrap();
}

#[test]
fn failed_turn_prompt_with_existing_history() {
    for streaming in [false, true] {
        recover_prompt(true, streaming);
    }
}

#[test]
fn failed_turn_prompt_on_first_turn_excludes_synthetic_reply() {
    for streaming in [false, true] {
        recover_prompt(false, streaming);
    }
}

#[test]
fn failed_turn_prompt_two_failures_recover_every_unseen_message() {
    for streaming in [false, true] {
        let upstream = FakeResponses::start_with_responses(vec![
            Some(recovery_success_response()),
            Some(visible_transport_failure_response()),
            Some(visible_transport_failure_response()),
            Some(recovery_success_response()),
        ]);
        let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
        let mut messages = vec![message("user", "already seen")];
        assert_eq!(status(&send(&bridge, &messages, streaming)), 200);
        messages.extend([message("assistant", "recovered"), message("user", "do X")]);
        assert!(send(&bridge, &messages, streaming).contains("error"));
        messages.extend([
            message("assistant", "first failure"),
            message("user", "do Y"),
        ]);
        assert!(send(&bridge, &messages, streaming).contains("error"));
        messages.extend([
            message("assistant", "second failure"),
            message("user", "retry"),
        ]);
        assert_eq!(status(&send(&bridge, &messages, streaming)), 200);
        let captured = inputs(&upstream);
        let input = captured.last().unwrap();
        for text in ["already seen", "do X", "do Y", "retry"] {
            assert_eq!(text_count(input, text), 1, "{text}: {input}");
        }
        for text in ["first failure", "second failure"] {
            assert_eq!(text_count(input, text), 0, "{text}: {input}");
        }
        bridge.shutdown().unwrap();
    }
}

#[test]
fn failed_turn_prompt_keeps_a_real_reply_mentioning_api_error() {
    let reply = String::from_utf8(recovery_success_response()).unwrap();
    let events = reply
        .split_once("\r\n\r\n")
        .unwrap()
        .1
        .replace("recovered", "API Error is a diagnostic");
    let upstream =
        FakeResponses::start_with_responses(vec![Some(response_with_events(&events)), None]);
    let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
    let mut messages = vec![message("user", "explain errors")];
    assert_eq!(status(&send(&bridge, &messages, false)), 200);
    messages.extend([
        message("assistant", "API Error is a diagnostic"),
        message("user", "next"),
    ]);
    assert_eq!(status(&send(&bridge, &messages, false)), 200);
    assert_eq!(
        text_count(
            inputs(&upstream).last().unwrap(),
            "API Error is a diagnostic"
        ),
        1
    );
    bridge.shutdown().unwrap();
}

#[test]
fn failed_turn_prompt_success_keeps_the_next_real_reply() {
    let reply = String::from_utf8(recovery_success_response()).unwrap();
    let events = reply
        .split_once("\r\n\r\n")
        .unwrap()
        .1
        .replace("recovered", "API Error is a diagnostic");
    let upstream = FakeResponses::start_with_responses(vec![
        Some(visible_transport_failure_response()),
        Some(response_with_events(&events)),
        None,
    ]);
    let mut bridge = BridgeHandle::start(bridged_config(&upstream)).unwrap();
    let mut messages = vec![message("user", "do X")];
    assert!(send(&bridge, &messages, false).contains("error"));
    messages.extend([
        message("assistant", "synthetic failure"),
        message("user", "retry"),
    ]);
    assert_eq!(status(&send(&bridge, &messages, false)), 200);
    messages.extend([
        message("assistant", "API Error is a diagnostic"),
        message("user", "next"),
    ]);
    assert_eq!(status(&send(&bridge, &messages, false)), 200);
    let captured = inputs(&upstream);
    let input = captured.last().unwrap();
    for text in ["do X", "retry", "API Error is a diagnostic", "next"] {
        assert_eq!(text_count(input, text), 1, "{text}: {input}");
    }
    assert_eq!(text_count(input, "synthetic failure"), 0);
    bridge.shutdown().unwrap();
}
