"""End-to-end coverage for the Claude -> Gemini request translator.

Ports CLIProxyAPI
``internal/translator/gemini/claude/gemini_claude_request.go``
(``ConvertClaudeRequestToGemini``). The Rust unit tests cover the mapping table;
these drive the real binary through its hidden ``__claude-gemini-translate``
seam, so the process boundary (stdin in, ``generateContent`` body out, exit
status on refusal) is exercised too.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from tests import process


def _binary_name(name: str) -> str:
    return f"{name}.exe" if sys.platform == "win32" else name


def _clud_binary() -> Path:
    configured = os.environ.get("CLUD_TEST_BINARY")
    if configured:
        return Path(configured)
    candidate = (
        Path(__file__).resolve().parents[1] / "target" / "debug" / _binary_name("clud")
    )
    assert candidate.is_file(), f"clud test binary not found at {candidate}"
    return candidate


def _translate(
    request: dict[str, object], *extra: str
) -> process.CompletedProcess[object]:
    return process.run(
        [str(_clud_binary()), "__claude-gemini-translate", *extra],
        input=json.dumps(request),
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_messages_request_becomes_a_generate_content_request() -> None:
    completed = _translate(
        {
            "model": "claude-sonnet-4",
            "max_tokens": 2048,
            "system": [{"type": "text", "text": "be brief"}],
            "messages": [
                {"role": "user", "content": "hi"},
                {
                    "role": "assistant",
                    "content": (
                        [{"type": "text", "text": "hello"}]
                    ),
                },
            ],
        },
        "--model",
        "gemini-2.5-pro",
    )
    assert completed.returncode == 0, completed.stderr
    body = json.loads(completed.stdout)
    assert body["model"] == "gemini-2.5-pro"
    assert body["systemInstruction"]["parts"][0]["text"] == "be brief"
    assert [turn["role"] for turn in body["contents"]] == ["user", "model"]
    assert body["generationConfig"]["maxOutputTokens"] == 2048
    assert len(body["safetySettings"]) == 5


def test_a_tool_call_round_trips_through_the_translator() -> None:
    completed = _translate(
        {
            "model": "claude-sonnet-4",
            "messages": [
                {"role": "user", "content": "read a.rs"},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "Read.File",
                            "input": {"path": "a.rs"},
                        }
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_1",
                            "content": "fn main() {}",
                        }
                    ],
                },
            ],
            "tools": [
                {
                    "name": "Read.File",
                    "description": "read",
                    "input_schema": {"type": "object"},
                }
            ],
            "tool_choice": {"type": "any"},
        }
    )
    assert completed.returncode == 0, completed.stderr
    body = json.loads(completed.stdout)
    call = body["contents"][1]["parts"][0]["functionCall"]
    assert call["name"] == "Read_File"
    assert call["args"] == {"path": "a.rs"}
    response = body["contents"][2]["parts"][0]["functionResponse"]
    assert response["name"] == "Read_File"
    assert response["response"]["result"] == "fn main() {}"
    declaration = body["tools"][0]["functionDeclarations"][0]
    assert declaration["name"] == "Read_File"
    assert declaration["parametersJsonSchema"] == {"type": "object"}
    assert body["toolConfig"]["functionCallingConfig"]["mode"] == "ANY"


def test_an_untranslatable_part_is_a_nonzero_exit_with_a_named_reason() -> None:
    completed = _translate(
        {
            "model": "claude-sonnet-4",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "image",
                            "source": {"type": "url", "url": "https://example.test/a.png"},
                        }
                    ],
                }
            ],
        }
    )
    assert completed.returncode == 2
    assert "url" in completed.stderr
    assert "Gemini part" in completed.stderr


def test_a_request_without_a_model_is_refused() -> None:
    completed = _translate({"messages": [{"role": "user", "content": "hi"}]})
    assert completed.returncode == 2
    assert "names no model" in completed.stderr


# --- the response direction: generateContent -> Anthropic Messages ---------

_GEMINI_RESPONSE = {
    "responseId": "resp_7",
    "modelVersion": "gemini-3-pro",
    "candidates": [
        {
            "content": {
                "parts": [
                    {"text": "thinking it over", "thought": True},
                    {"text": "here you go"},
                    {"functionCall": {"name": "Read.File", "args": {"path": "a.rs"}}},
                ]
            },
            "finishReason": "STOP",
        }
    ],
    "usageMetadata": {
        "promptTokenCount": 100,
        "cachedContentTokenCount": 40,
        "candidatesTokenCount": 12,
    },
}


def _translate_response(payload: str, *extra: str) -> process.CompletedProcess[object]:
    return process.run(
        [str(_clud_binary()), "__claude-gemini-translate", "--response", *extra],
        input=payload,
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_a_generate_content_response_becomes_a_messages_response() -> None:
    completed = _translate_response(json.dumps(_GEMINI_RESPONSE))
    assert completed.returncode == 0, completed.stderr
    body = json.loads(completed.stdout)
    assert body["id"] == "resp_7"
    assert body["type"] == "message"
    assert body["model"] == "gemini-3-pro"
    assert body["content"][0] == {"type": "thinking", "thinking": "thinking it over"}
    assert body["content"][1] == {"type": "text", "text": "here you go"}
    tool = body["content"][2]
    assert tool["type"] == "tool_use"
    assert tool["name"] == "Read_File"
    assert tool["input"] == {"path": "a.rs"}
    assert body["stop_reason"] == "tool_use"
    assert body["usage"] == {
        "input_tokens": 60,
        "output_tokens": 12,
        "cache_read_input_tokens": 40,
    }


def test_a_malformed_function_call_is_refused_by_name() -> None:
    completed = _translate_response(
        json.dumps({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL"}]})
    )
    assert completed.returncode == 2
    assert "MALFORMED_FUNCTION_CALL" in completed.stderr


def test_a_streamed_response_becomes_anthropic_sse_events() -> None:
    stream = "\n\n".join(
        f"data: {json.dumps(chunk)}"
        for chunk in (
            {
                "responseId": "resp_8",
                "modelVersion": "gemini-3-pro",
                "candidates": [{"content": {"parts": [{"text": "hi"}]}}],
            },
            {"candidates": [{"content": {"parts": [{"text": " there"}]}}]},
            {
                "candidates": [
                    {
                        "content": {
                            "parts": [{"functionCall": {"name": "Bash", "args": {"command": "ls"}}}]
                        },
                        "finishReason": "STOP",
                    }
                ],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 4},
            },
        )
    )
    completed = _translate_response(f"{stream}\n\ndata: [DONE]\n\n", "--stream")
    assert completed.returncode == 0, completed.stderr
    events = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
    kinds = [event["type"] for event in events]
    assert kinds[0] == "message_start"
    assert events[0]["message"]["id"] == "resp_8"
    assert kinds[-1] == "message_stop"
    assert kinds.count("message_delta") == 1
    text = "".join(
        event["delta"]["text"]
        for event in events
        if event.get("delta", {}).get("type") == "text_delta"
    )
    assert text == "hi there"
    delta = next(event for event in events if event["type"] == "message_delta")
    assert delta["delta"]["stop_reason"] == "tool_use"
    assert delta["usage"]["input_tokens"] == 10


def test_the_http_stream_frames_name_their_event() -> None:
    stream = (
        'data: {"responseId":"resp_9","candidates":[{"content":{"parts":[{"text":"ok"}]},'
        '"finishReason":"STOP"}]}\n\ndata: [DONE]\n\n'
    )
    completed = _translate_response(stream, "--stream", "--http")
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout.startswith("event: message_start\ndata: {")
    assert "event: content_block_delta\ndata: " in completed.stdout
    assert completed.stdout.rstrip().endswith(
        "event: message_stop\ndata: {\"type\":\"message_stop\"}"
    )
