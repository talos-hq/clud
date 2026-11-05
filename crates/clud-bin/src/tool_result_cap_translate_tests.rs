//! Boundary tests and an opt-in replay of externally captured shell results.
use super::*;
use serde_json::{json, Value};

fn request(name: &str, content: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"messages": [
        {"role":"assistant", "content":[{"type":"tool_use", "id":"call_shell", "name":name, "input":{}}]},
        {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"call_shell", "content":content}]}
    ]})).unwrap()
}

fn output(body: &[u8]) -> Value {
    let translated = translate_bytes(body, &TranslateOptions::default()).unwrap();
    serde_json::to_value(&translated.request.input).unwrap()[1]["output"].clone()
}

#[test]
fn shell_only_normal_and_pending_translation() {
    let text = "source line\n".repeat(1000);
    let body = request("Bash", json!(text));
    let normal = output(&body);
    assert!(normal
        .as_str()
        .unwrap()
        .contains("bytes omitted; re-read with"));
    let pending = pending_input_items_as_values(&body).unwrap();
    assert_eq!(pending[0]["output"], normal);
    assert_eq!(output(&request("Read", json!(text))), text);
    // A result without its originating shell call cannot be classified safely.
    let unknown = serde_json::to_vec(&json!({"messages":[{"role":"user", "content":[
        {"type":"tool_result", "tool_use_id":"unknown", "content":text}
    ]}]}))
    .unwrap();
    let translated = translate_bytes(&unknown, &TranslateOptions::default()).unwrap();
    assert_eq!(
        serde_json::to_value(translated.request.input).unwrap()[0]["output"],
        text
    );
}

#[test]
fn structured_text_shares_budget_and_preserves_images() {
    let text = "abcdefghij\n".repeat(300);
    let body = request(
        "Bash",
        json!([
            {"type":"text", "text":text},
            {"type":"image", "source":{"type":"url", "url":"https://example.test/image.png"}},
            {"type":"text", "text":text}
        ]),
    );
    let parts = output(&body);
    assert_eq!(parts.as_array().unwrap().len(), 2);
    assert!(parts[0]["text"].as_str().unwrap().contains("bytes omitted"));
    assert_eq!(parts[1]["image_url"], "https://example.test/image.png");
}

/// Supply a JSON array of UTF-8 shell outputs through CLUD_CAP_REPLAY_INPUT;
/// emitted strings are written to CLUD_CAP_REPLAY_OUTPUT. No provider call.
#[test]
#[ignore = "requires externally captured benchmark outputs"]
fn replay_shell_results() {
    let path = std::env::var("CLUD_CAP_REPLAY_INPUT").unwrap();
    let captured: Vec<String> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let emitted = captured
        .iter()
        .map(|text| output(&request("Bash", json!(text))))
        .collect::<Vec<_>>();
    std::fs::write(
        std::env::var("CLUD_CAP_REPLAY_OUTPUT").unwrap(),
        serde_json::to_vec(&emitted).unwrap(),
    )
    .unwrap();
    println!(
        "replayed {} shell results through translate_bytes",
        emitted.len()
    );
}
