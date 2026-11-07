//! Route epochs follow the caller's selection, not auxiliary traffic or retries.
use super::*;

fn reasoning_reply() -> Vec<u8> {
    let item = serde_json::json!({"type":"reasoning", "id":"rs_route",
        "encrypted_content":"opaque-route-reasoning", "summary":[]});
    response_with_events(&format!(
        "event: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        serde_json::json!({"type":"response.output_item.done", "item":item}),
        serde_json::json!({"type":"response.completed", "response":{"usage":{
            "input_tokens":3,"output_tokens":1}}})
    ))
}

fn turn(bridge: &BridgeHandle, model: &str, text: &str) -> String {
    replay_turn(
        bridge,
        model,
        serde_json::json!([{"role":"user", "content":text}]),
    )
}

fn replay_turn(bridge: &BridgeHandle, model: &str, messages: serde_json::Value) -> String {
    let body = serde_json::json!({"model":model,"messages":messages,"stream":false}).to_string();
    unified_request(bridge, &body, "route-history", None)
}

fn limited() -> Vec<u8> {
    raw_response(
        429,
        "Too Many Requests",
        r#"{"error":{"message":"usage_limit_reached"}}"#,
    )
}

#[test]
fn unified_side_call_preserves_codex_reasoning() {
    let codex = FakeResponses::start_with_responses(vec![Some(reasoning_reply()), None]);
    let claude = FakeResponses::start();
    let deepseek = FakeResponses::start();
    let bridge = BridgeHandle::start(unified_config(&claude, &codex, &deepseek)).unwrap();
    assert_eq!(
        status(&turn(&bridge, "clud-claude-codex-sol", "first")),
        200
    );
    let side = r#"{"model":"claude-haiku-4-5","system":"Name the session","tools":[],"messages":[{"role":"user","content":"SIDE-TRANSCRIPT"}],"stream":false}"#;
    assert_eq!(
        status(&unified_request(&bridge, side, "route-history", None)),
        200
    );
    assert_eq!(status(&turn(&bridge, "clud-claude-codex-sol", "next")), 200);
    let sent = codex.requests();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].contains("opaque-route-reasoning"), "{}", sent[1]);
    assert!(!sent[1].contains("SIDE-TRANSCRIPT"));
    assert_eq!(claude.requests().len(), 1);
}

#[test]
fn unified_failover_bounce_preserves_codex_reasoning() {
    let codex = FakeResponses::start_with_responses(vec![Some(reasoning_reply()), None]);
    let claude = FakeResponses::start();
    let deepseek =
        FakeResponses::start_with_responses(vec![Some(limited()), None, Some(limited())]);
    let config = BridgeConfig::default()
        .with_test_upstream_url(Some(codex.base_url.clone()))
        .with_unified_gateway(
            UnifiedGatewayConfig::new(Some("test-key".into()), true)
                .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                .with_failover(FailoverLadder::parse("codex-sol", true).unwrap()),
        );
    let bridge = BridgeHandle::start(config).unwrap();
    let mut messages = Vec::new();
    for text in ["first fallback", "primary recovers", "fallback again"] {
        messages.push(serde_json::json!({"role":"user", "content":text}));
        let response = replay_turn(
            &bridge,
            "clud-claude-deepseek-flash",
            serde_json::json!(messages),
        );
        assert_eq!(status(&response), 200, "{response}");
        messages
            .push(serde_json::json!({"role":"assistant", "content":format!("answer to {text}")}));
    }
    let sent = codex.requests();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].contains("opaque-route-reasoning"), "{}", sent[1]);
    assert!(sent[1].contains("primary recovers"), "{}", sent[1]);
    assert!(
        sent[1].contains("answer to primary recovers"),
        "{}",
        sent[1]
    );
    assert_eq!(deepseek.requests().len(), 3);
}

#[test]
fn unified_codex_429_and_return_preserve_reasoning_without_proxy_failover() {
    let codex =
        FakeResponses::start_with_responses(vec![Some(reasoning_reply()), Some(limited()), None]);
    let claude = FakeResponses::start();
    let deepseek = FakeResponses::start();
    let bridge = BridgeHandle::start(unified_config(&claude, &codex, &deepseek)).unwrap();
    assert_eq!(
        status(&turn(&bridge, "clud-claude-codex-sol", "first")),
        200
    );
    assert_eq!(
        status(&turn(&bridge, "clud-claude-codex-sol", "limited")),
        429
    );
    assert_eq!(
        status(&turn(&bridge, "clud-claude-codex-sol", "return")),
        200
    );
    let sent = codex.requests();
    assert!(sent.last().unwrap().contains("opaque-route-reasoning"));
    assert!(claude.requests().is_empty());
}

fn proxy_replay_bridge(
    codex: &FakeResponses,
    claude: &FakeResponses,
    deepseek: &FakeResponses,
) -> BridgeHandle {
    BridgeHandle::start(
        BridgeConfig::default()
            .with_test_upstream_url(Some(codex.base_url.clone()))
            .with_unified_gateway(
                UnifiedGatewayConfig::new(Some("test-key".into()), true)
                    .with_upstreams(claude.base_url.clone(), deepseek.base_url.clone())
                    .with_failover(FailoverLadder::parse("codex-sol", true).unwrap()),
            ),
    )
    .unwrap()
}

fn context_control(bridge: &BridgeHandle, path: &str, event: &str) {
    let body = serde_json::json!({"session_id":"route-history", "hook_event_name":event,
        "trigger":"manual", "source":"compact"})
    .to_string();
    let response = request(
        bridge.socket_addr(),
        &authorized_with_headers(
            "POST",
            path,
            bridge.bearer_token(),
            &body,
            &[(UNIFIED_GATEWAY_TOKEN_HEADER, bridge.bearer_token())],
        ),
    );
    assert_eq!(status(&response), 204, "{response}");
}

#[test]
fn unified_pending_proxy_replay_prevents_incomplete_provider_compaction() {
    let codex = FakeResponses::start_with_responses(vec![
        Some(reasoning_reply()),
        Some(compact_success_response()),
    ]);
    let claude = FakeResponses::start();
    let deepseek = FakeResponses::start_with_responses(vec![Some(limited()), None]);
    let bridge = proxy_replay_bridge(&codex, &claude, &deepseek);
    assert_eq!(
        status(&turn(&bridge, "clud-claude-deepseek-flash", "fallback")),
        200
    );
    assert_eq!(
        status(&turn(
            &bridge,
            "clud-claude-deepseek-flash",
            "primary context"
        )),
        200
    );
    context_control(&bridge, "/_clud/context/compact", "PreCompact");
    assert_eq!(
        codex.requests().len(),
        1,
        "provider compaction would omit proxy turns"
    );
}

#[test]
fn unified_proxy_replay_compaction_reseeds_the_installed_summary() {
    let codex = FakeResponses::start_with_responses(vec![Some(reasoning_reply()), None]);
    let claude = FakeResponses::start();
    let deepseek =
        FakeResponses::start_with_responses(vec![Some(limited()), None, Some(limited())]);
    let bridge = proxy_replay_bridge(&codex, &claude, &deepseek);
    assert_eq!(
        status(&turn(&bridge, "clud-claude-deepseek-flash", "fallback")),
        200
    );
    assert_eq!(
        status(&turn(
            &bridge,
            "clud-claude-deepseek-flash",
            "primary context"
        )),
        200
    );
    context_control(&bridge, "/_clud/context/compact", "PreCompact");
    context_control(&bridge, "/_clud/context/compact-finished", "SessionStart");
    let response = turn(
        &bridge,
        "clud-claude-deepseek-flash",
        "INSTALLED SUMMARY includes primary context",
    );
    assert_eq!(status(&response), 200, "{response}");
    let sent = codex.requests();
    assert_eq!(
        sent.len(),
        2,
        "canonical snapshot must not be compacted without proxy context"
    );
    assert!(sent[1].contains("INSTALLED SUMMARY includes primary context"));
    assert!(!sent[1].contains("opaque-route-reasoning"));
}
