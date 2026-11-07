//! Prompt-cache defaults for translated OpenAI clients on explicit-cache routes.
use serde_json::{json, Map, Value};

/// Preserve the client's breakpoint verbatim on its translated block.
pub(crate) fn preserve(mut block: Value, source: &Map<String, Value>) -> Value {
    if let Some(control) = source.get("cache_control") {
        block["cache_control"] = control.clone();
    }
    block
}

fn blocks_have_breakpoint(value: Option<&Value>) -> bool {
    value.and_then(Value::as_array).is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block.get("cache_control").is_some()
                || (block["type"] == "tool_result" && blocks_have_breakpoint(block.get("content")))
        })
    })
}

fn has_breakpoint(request: &Value) -> bool {
    ["tools", "system"]
        .iter()
        .any(|key| blocks_have_breakpoint(request.get(key)))
        || request
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| blocks_have_breakpoint(message.get("content")))
            })
}

/// Never supplement client breakpoints. Three defaults leave one spare slot.
pub(crate) fn add_defaults(request: &mut Value) {
    if has_breakpoint(request) {
        return;
    }
    for key in ["tools", "system"] {
        if let Some(block) = request
            .get_mut(key)
            .and_then(Value::as_array_mut)
            .and_then(|v| v.last_mut())
        {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let previous_user = messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message["role"] == "user")
        .nth(1)
        .map(|(index, _)| index);
    if let Some(index) = previous_user {
        if let Some(block) = messages[index]
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|v| v.last_mut())
        {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Value {
        json!({
            "tools":[
                {"name":"first"},
                {"name":"last", "input_schema":{"type":"object", "properties":{
                    "cache_control":{"type":"string"}
                }}}
            ],
            "system":[{"type":"text", "text":"first"}, {"type":"text", "text":"last"}],
            "messages":[
                {"role":"user", "content":[{"type":"text", "text":"first"}, {"type":"text", "text":"last"}]},
                {"role":"assistant", "content":[{"type":"tool_use", "id":"call", "name":"last", "input":{"cache_control":"ordinary data"}}]},
                {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"call", "content":[{"type":"text", "text":"result"}]}]}
            ]
        })
    }

    #[test]
    fn openai_ingress_cache_control_uses_metadata_locations_and_last_blocks() {
        let mut body = request();
        add_defaults(&mut body);
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            body["messages"][0]["content"][1]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(body["tools"][0].get("cache_control").is_none());
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
        assert_eq!(
            body.to_string()
                .matches("\"cache_control\":{\"type\":\"ephemeral\"}")
                .count(),
            3
        );
    }

    #[test]
    fn openai_ingress_cache_control_preserves_nested_result_breakpoints_verbatim() {
        let mut body = request();
        body["messages"][2]["content"][0]["content"][0]["cache_control"] =
            json!({"type":"ephemeral", "ttl":"1h"});
        let original = body.clone();
        add_defaults(&mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn openai_ingress_cache_control_empty_and_single_turn_have_no_previous_user() {
        let mut empty = json!({"messages":[]});
        add_defaults(&mut empty);
        assert_eq!(empty, json!({"messages":[]}));
        let mut single =
            json!({"messages":[{"role":"user", "content":[{"type":"text", "text":"hello"}]}]});
        let original = single.clone();
        add_defaults(&mut single);
        assert_eq!(single, original);
    }
}
