use super::*;

const SECRET: &str = "FAKE_VAULT_CREDENTIAL_FOR_GATEWAY_REDACTION";
const FREE_MODEL: &str = "fixture/noncatalog-model:free";

fn gateway(explicit: Option<bool>) -> (UnifiedGatewayConfig, String) {
    let config = UnifiedGatewayConfig::new(None, false)
        .with_openrouter(Some(SECRET.to_string()))
        .with_launch_model(ModelProvider::OpenRouter, Some(FREE_MODEL))
        .with_redaction(explicit)
        .unwrap();
    let token = config
        .redaction
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .redact_text(SECRET)
        .into_owned();
    (config, token)
}

fn message(content: serde_json::Value) -> String {
    serde_json::json!({
        "id":"msg_redaction", "type":"message", "role":"assistant",
        "model":FREE_MODEL, "content":content, "stop_reason":"end_turn",
        "usage":{"input_tokens":1,"output_tokens":1}
    })
    .to_string()
}

fn body(streaming: bool) -> String {
    serde_json::json!({
        "model":FREE_MODEL,"stream":streaming,"max_tokens":100,
        "system":SECRET,
        "messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","id":"call_old","name":"fixture","input":{"nested":[SECRET]}}
            ]},
            {"role":"user","content":[
                {"type":"text","text":SECRET},
                {"type":"tool_result","tool_use_id":"call_old","content":[{"type":"text","text":SECRET}]},
                {"type":"document","source":{"type":"text","media_type":"text/plain","data":SECRET}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}}
            ]}
        ]
    }).to_string()
}

fn assert_tokenized(upstream: &FakeResponses, token: &str) {
    let captured = upstream.requests();
    assert_eq!(captured.len(), 1);
    let sent = captured_body(&captured[0]);
    assert!(
        !sent.contains(SECRET),
        "credential reached model-visible body"
    );
    assert!(sent.contains(token));
    let value: serde_json::Value = serde_json::from_str(sent).unwrap();
    assert_eq!(
        value["messages"][1]["content"][3]["source"]["data"],
        "aW1hZ2U="
    );
}

#[test]
fn free_model_redaction_routes_exactly_and_restores_nonstream_tool_input() {
    let (gateway, token) = gateway(None);
    let reply = message(serde_json::json!([
        {"type":"text","text":token},
        {"type":"tool_use","id":"call_new","name":"fixture","input":{"credential":token}}
    ]));
    let upstream = FakeResponses::start_with_response(Some(raw_response(200, "OK", &reply)));
    let claude = FakeResponses::start();
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let log = dir.path().join("bridge.jsonl");
    let config = gateway
        .with_upstreams(claude.base_url.clone(), String::new())
        .with_openrouter_upstream(upstream.base_url.clone());
    let mut bridge = BridgeHandle::start(
        BridgeConfig::default()
            .with_unified_gateway(config)
            .with_log_path(log.clone()),
    )
    .unwrap();
    let response = unified_request(&bridge, &body(false), "redaction-nonstream", None);
    assert_eq!(status(&response), 200, "{response}");
    let restored: serde_json::Value = response_json(&response);
    assert_eq!(restored["content"][0]["text"], SECRET);
    assert_eq!(restored["content"][1]["input"]["credential"], SECRET);
    assert_tokenized(&upstream, &token);
    let requests = upstream.requests();
    let sent: serde_json::Value = serde_json::from_str(captured_body(&requests[0])).unwrap();
    assert_eq!(sent["model"], FREE_MODEL);
    assert!(
        claude.requests().is_empty(),
        "raw free ID reached Anthropic"
    );
    bridge.shutdown().unwrap();
    assert!(
        !log.exists(),
        "successful turns must not create forensic logs"
    );
}

fn response_json(response: &str) -> serde_json::Value {
    let head = response.split_once("\r\n\r\n").unwrap().0;
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        decode_chunked_body(response)
    } else {
        captured_body(response).to_string()
    };
    serde_json::from_str(&body).unwrap()
}

fn sse(document: serde_json::Value) -> String {
    format!(
        "event: {}\ndata: {document}\n\n",
        document["type"].as_str().unwrap()
    )
}

fn echoed_tool_stream(token: &str) -> String {
    let mut events = sse(serde_json::json!({"type":"message_start","message":{
        "id":"msg_stream","type":"message","role":"assistant","model":FREE_MODEL,
        "content":[],"usage":{"input_tokens":1,"output_tokens":0}}}));
    events.push_str(&sse(
        serde_json::json!({"type":"content_block_start","index":0,
        "content_block":{"type":"tool_use","id":"call_stream","name":"fixture","input":{}}}),
    ));
    let input = serde_json::json!({"credential":token}).to_string();
    let split = input.find(token).unwrap() + 7;
    for fragment in [&input[..split], &input[split..]] {
        events.push_str(&sse(
            serde_json::json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":fragment}}),
        ));
    }
    events.push_str(&sse(
        serde_json::json!({"type":"content_block_stop","index":0}),
    ));
    events.push_str(&sse(
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},
        "usage":{"output_tokens":1}}),
    ));
    events.push_str(&sse(serde_json::json!({"type":"message_stop"})));
    events
}

#[test]
fn free_model_stream_restores_a_tool_token_split_across_events() {
    let (gateway, token) = gateway(None);
    let upstream =
        FakeResponses::start_with_response(Some(response_with_events(&echoed_tool_stream(&token))));
    let config = gateway.with_openrouter_upstream(upstream.base_url.clone());
    let bridge = BridgeHandle::start(BridgeConfig::default().with_unified_gateway(config)).unwrap();
    let response = unified_request(&bridge, &body(true), "redaction-stream", None);
    assert_eq!(status(&response), 200, "{response}");
    let events = decode_chunked_body(&response);
    let fragments: String = events
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|event| event["delta"]["partial_json"].as_str().map(str::to_string))
        .collect();
    let input: serde_json::Value = serde_json::from_str(&fragments).unwrap();
    assert_eq!(input["credential"], SECRET);
    assert!(!events.contains(&token));
    assert_tokenized(&upstream, &token);
}

#[test]
fn free_model_failover_keeps_redaction_on_the_paid_claude_rung() {
    let (gateway, token) = gateway(None);
    let upstream = FakeResponses::start_with_response(Some(raw_response(
        429,
        "Too Many Requests",
        r#"{"error":{"message":"Rate limit exceeded: free-models-per-day-stealth"}}"#,
    )));
    let reply = message(serde_json::json!([{"type":"text","text":token}]));
    let claude = FakeResponses::start_with_response(Some(raw_response(200, "OK", &reply)));
    let config = gateway
        .with_openrouter_upstream(upstream.base_url.clone())
        .with_upstreams(claude.base_url.clone(), String::new())
        .with_failover(FailoverLadder::parse("claude-opus-4-1", true).unwrap());
    let bridge = BridgeHandle::start(BridgeConfig::default().with_unified_gateway(config)).unwrap();
    let response = unified_request(&bridge, &body(false), "redaction-failover", None);
    assert_eq!(status(&response), 200, "{response}");
    let restored: serde_json::Value = response_json(&response);
    assert_eq!(restored["content"][0]["text"], SECRET);
    assert_tokenized(&upstream, &token);
    assert_tokenized(&claude, &token);
}

#[test]
fn native_token_counting_redacts_and_model_discovery_is_unchanged() {
    let (mut gateway, token) = gateway(Some(true));
    let expected = unified_catalog_ids(&gateway);
    gateway.redaction_override = Some(false);
    assert_eq!(unified_catalog_ids(&gateway), expected);
    gateway.redaction_override = Some(true);
    let upstream =
        FakeResponses::start_with_response(Some(raw_response(200, "OK", r#"{"input_tokens":7}"#)));
    gateway.anthropic_base_url = upstream.base_url.clone();
    let bridge =
        BridgeHandle::start(BridgeConfig::default().with_unified_gateway(gateway)).unwrap();
    let mut payload: serde_json::Value = serde_json::from_str(&body(false)).unwrap();
    payload["model"] = serde_json::json!("claude-opus-4-1");
    let response = request(
        bridge.socket_addr(),
        &authorized_with_headers(
            "POST",
            "/v1/messages/count_tokens",
            "native-count-canary",
            &payload.to_string(),
            &[(UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token())],
        ),
    );
    assert_eq!(status(&response), 200, "{response}");
    assert_tokenized(&upstream, &token);
}

#[test]
fn clean_free_model_http_body_keeps_original_spacing_and_order() {
    let (gateway, _) = gateway(None);
    let reply = message(serde_json::json!([]));
    let upstream = FakeResponses::start_with_response(Some(raw_response(200, "OK", &reply)));
    let config = gateway.with_openrouter_upstream(upstream.base_url.clone());
    let bridge = BridgeHandle::start(BridgeConfig::default().with_unified_gateway(config)).unwrap();
    let body = format!("{{ \"stream\" : false, \"model\" : \"{FREE_MODEL}\", \"messages\" : [] }}");
    let response = unified_request(&bridge, &body, "redaction-clean", None);
    assert_eq!(status(&response), 200, "{response}");
    let captured = upstream.requests();
    assert_eq!(captured_body(&captured[0]), body);
}

#[test]
fn gateway_debug_and_status_never_serialize_the_secret_mapping() {
    let (gateway, token) = gateway(Some(true));
    let debug = format!("{gateway:?}");
    let session = gateway.redaction.as_ref().unwrap().lock().unwrap();
    let status = serde_json::to_string(&session.counts()).unwrap();
    for visible in [debug, status] {
        assert!(!visible.contains(SECRET));
        assert!(!visible.contains(&token));
    }
}

#[test]
fn codex_gateway_restores_text_after_responses_protocol_translation() {
    let (mut gateway, token) = gateway(Some(true));
    gateway.codex_available = true;
    let events = [
        serde_json::json!({"type":"response.created"}),
        serde_json::json!({"type":"response.output_text.delta","output_index":0,
            "content_index":0,"delta":token}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{
            "type":"message","id":"msg_codex_redaction","role":"assistant",
            "content":[{"type":"output_text","text":token}]}}),
        serde_json::json!({"type":"response.completed","response":{
            "usage":{"input_tokens":1,"output_tokens":1}}}),
    ]
    .into_iter()
    .map(sse)
    .collect::<String>();
    let upstream = FakeResponses::start_with_response(Some(response_with_events(&events)));
    let config = BridgeConfig::default()
        .with_unified_gateway(gateway)
        .with_test_upstream_url(Some(upstream.base_url.clone()));
    let bridge = BridgeHandle::start(config).unwrap();
    let body = serde_json::json!({"model":"clud-claude-codex-sol","stream":false,
        "messages":[{"role":"user","content":SECRET}]})
    .to_string();
    let response = unified_request(&bridge, &body, "codex-redaction", None);
    assert_eq!(status(&response), 200, "{response}");
    let restored: serde_json::Value = response_json(&response);
    assert_eq!(restored["content"][0]["text"], SECRET);
    assert!(!response.contains(&token));
    let captured = upstream.requests();
    assert_eq!(captured.len(), 1);
    let sent = captured_body(&captured[0]);
    assert!(!sent.contains(SECRET));
    assert!(sent.contains(&token));
}

#[test]
fn paid_to_free_failover_enables_redaction_unless_explicitly_disabled() {
    for disabled in [false, true] {
        let (mut gateway, token) = gateway(None);
        if disabled {
            gateway = gateway.with_redaction(Some(false)).unwrap();
        }
        let echoed = if disabled { SECRET } else { &token };
        let reply = message(serde_json::json!([{"type":"text","text":echoed}]));
        let free = FakeResponses::start_with_response(Some(raw_response(200, "OK", &reply)));
        let paid = FakeResponses::start_with_response(Some(raw_response(
            200,
            "OK",
            r#"{"error":{"message":"fixture provider fault"}}"#,
        )));
        // The scheduler receives resolved rungs, independently of catalog discovery.
        let ladder = FailoverLadder::from_test_rungs(vec![crate::failover::FailoverRung {
            spec: FREE_MODEL.into(),
            wire_id: FREE_MODEL.into(),
            provider: ModelProvider::OpenRouter,
            route: ConversationRoute::OpenRouter,
            cost: crate::failover::CostOwner::Metered,
        }]);
        let config = gateway
            .with_openrouter_upstream(free.base_url.clone())
            .with_upstreams(paid.base_url.clone(), String::new())
            .with_failover(ladder);
        let bridge =
            BridgeHandle::start(BridgeConfig::default().with_unified_gateway(config)).unwrap();
        let mut payload: serde_json::Value = serde_json::from_str(&body(false)).unwrap();
        payload["model"] = serde_json::json!("claude-opus-4-1");
        let response = unified_request(&bridge, &payload.to_string(), "paid-to-free", None);
        assert_eq!(status(&response), 200, "{response}");
        let captured = free.requests();
        assert_eq!(captured.len(), 1);
        let sent = captured_body(&captured[0]);
        assert_eq!(sent.contains(SECRET), disabled);
        assert_eq!(sent.contains(&token), !disabled);
        let restored: serde_json::Value = response_json(&response);
        assert_eq!(restored["content"][0]["text"], SECRET);
    }
}
