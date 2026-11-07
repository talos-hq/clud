"""End-to-end coverage for the OpenAI Chat Completions ingress translator.

Ports CLIProxyAPI
``internal/translator/claude/openai/chat-completions/claude_openai_request.go``.
The Rust unit tests cover the mapping table; these run the real binary through
its hidden ``__openai-chat-translate`` seam, so the process boundary (stdin in,
Messages body out, exit status on refusal) is exercised too.
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
    candidate = Path(__file__).resolve().parents[1] / "target" / "debug" / _binary_name("clud")
    assert candidate.is_file(), f"clud test binary not found at {candidate}"
    return candidate


def _translate(request: dict[str, object]) -> process.CompletedProcess[object]:
    return process.run(
        [str(_clud_binary()), "__openai-chat-translate"],
        input=json.dumps(request),
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_openai_chat_request_becomes_a_messages_request() -> None:
    completed = _translate(
        {
            "model": "claude-sonnet",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "hello"},
                {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "read_file", "arguments": '{"path":"a.rs"}'},
                        }
                    ],
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "fn main() {}"},
            ],
            "max_completion_tokens": 512,
            "tool_choice": "required",
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "read a file",
                        "parameters": {
                            "type": "object",
                            "properties": {"path": {"type": "string"}},
                        },
                    },
                }
            ],
        }
    )

    assert completed.returncode == 0, completed.stderr
    translated = json.loads(completed.stdout)

    assert translated["model"] == "claude-sonnet"
    assert translated["max_tokens"] == 512
    assert translated["stream"] is False
    assert translated["system"][0]["text"] == "be terse"
    assert translated["messages"][0]["role"] == "user"
    assert translated["messages"][0]["content"][0]["text"] == "hello"
    assert translated["messages"][1]["content"][0]["type"] == "tool_use"
    assert translated["messages"][1]["content"][0]["input"] == {"path": "a.rs"}
    assert translated["messages"][2]["content"][0]["type"] == "tool_result"
    assert translated["messages"][2]["content"][0]["tool_use_id"] == "call_1"
    assert translated["tools"][0]["name"] == "read_file"
    assert translated["tool_choice"]["type"] == "any"


def _translate_response(
    payload: dict[str, object] | str, *args: str
) -> process.CompletedProcess[object]:
    body = payload if isinstance(payload, str) else json.dumps(payload)
    return process.run(
        [str(_clud_binary()), "__openai-chat-translate", "--response", *args],
        input=body,
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_messages_response_becomes_a_chat_completion() -> None:
    completed = _translate_response(
        {
            "id": "msg_42",
            "type": "message",
            "model": "claude-sonnet",
            "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "pick the tool"},
                {"type": "text", "text": "reading it"},
                {
                    "type": "tool_use",
                    "id": "toolu_42",
                    "name": "read_file",
                    "input": {"path": "a.rs"},
                },
            ],
            "usage": {
                "input_tokens": 11,
                "output_tokens": 3,
                "cache_read_input_tokens": 4,
            },
        }
    )

    assert completed.returncode == 0, completed.stderr
    translated = json.loads(completed.stdout)
    assert translated["object"] == "chat.completion"
    assert translated["id"] == "msg_42"
    assert translated["model"] == "claude-sonnet"
    message = translated["choices"][0]["message"]
    assert message["content"] == "reading it"
    assert message["reasoning_content"] == "pick the tool"
    assert message["tool_calls"][0]["function"]["name"] == "read_file"
    assert json.loads(message["tool_calls"][0]["function"]["arguments"]) == {"path": "a.rs"}
    assert translated["choices"][0]["finish_reason"] == "tool_calls"
    assert translated["usage"]["prompt_tokens"] == 15
    assert translated["usage"]["total_tokens"] == 18


def test_for_model_names_the_reply_when_the_body_has_no_model() -> None:
    completed = _translate_response(
        {"content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn"},
        "--for-model",
        "my-alias",
    )

    assert completed.returncode == 0, completed.stderr
    assert json.loads(completed.stdout)["model"] == "my-alias"


def test_sse_stream_becomes_openai_chunks_with_a_trailing_usage_frame() -> None:
    events = [
        {"type": "message_start", "message": {"id": "msg_s", "model": "claude-sonnet",
                                              "usage": {"input_tokens": 2}}},
        {"type": "content_block_delta", "index": 0,
         "delta": {"type": "text_delta", "text": "hello"}},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
         "usage": {"output_tokens": 1}},
        {"type": "message_stop"},
    ]
    body = "".join(f"event: x\ndata: {json.dumps(event)}\n\n" for event in events)
    completed = _translate_response(body, "--stream")

    assert completed.returncode == 0, completed.stderr
    chunks = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
    assert len(chunks) == 4
    assert all(chunk["object"] == "chat.completion.chunk" for chunk in chunks)
    assert chunks[0]["choices"][0]["delta"]["role"] == "assistant"
    assert chunks[1]["choices"][0]["delta"]["content"] == "hello"
    assert chunks[2]["choices"][0]["finish_reason"] == "stop"
    assert chunks[3]["choices"] == []
    assert chunks[3]["usage"]["total_tokens"] == 3


def test_http_streaming_framing_matches_what_the_route_writes() -> None:
    """The route writes SSE frames plus the ``[DONE]`` sentinel an OpenAI
    client waits for; ``--http`` renders exactly those bytes."""
    events = [
        {"type": "message_start", "message": {"id": "msg_h", "model": "claude-sonnet",
                                              "usage": {"input_tokens": 2}}},
        {"type": "content_block_delta", "index": 0,
         "delta": {"type": "text_delta", "text": "hello"}},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
         "usage": {"output_tokens": 1}},
        {"type": "message_stop"},
    ]
    body = "".join(f"event: x\ndata: {json.dumps(event)}\n\n" for event in events)
    completed = _translate_response(body, "--stream", "--http", "--for-model", "alias-m")

    assert completed.returncode == 0, completed.stderr
    # The capture adapter re-joins lines, so the blank-line frame separator is
    # asserted in Rust (``render_chat_stream``); here every frame must still
    # carry its ``data: `` prefix and the stream must end with the sentinel.
    frames = [line for line in completed.stdout.splitlines() if line.strip()]
    assert all(frame.startswith("data: ") for frame in frames), frames
    assert frames[-1] == "data: [DONE]", frames[-1]
    chunks = [json.loads(frame[len("data: "):]) for frame in frames[:-1]]
    assert all(chunk["object"] == "chat.completion.chunk" for chunk in chunks)
    assert chunks[0]["model"] == "claude-sonnet"
    assert chunks[-1]["choices"] == []


def test_http_buffered_body_is_one_completion_object() -> None:
    completed = _translate_response(
        {
            "id": "msg_b",
            "type": "message",
            "model": "claude-sonnet",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "buffered"}],
            "usage": {"input_tokens": 1, "output_tokens": 1},
        },
        "--http",
    )

    assert completed.returncode == 0, completed.stderr
    translated = json.loads(completed.stdout)
    assert translated["object"] == "chat.completion"
    assert translated["choices"][0]["message"]["content"] == "buffered"
    assert "data:" not in completed.stdout


def test_response_body_that_is_not_json_exits_non_zero() -> None:
    completed = _translate_response("not json")

    assert completed.returncode == 2
    assert "JSON" in completed.stderr


def test_unsupported_content_part_is_refused_with_its_name() -> None:
    completed = _translate(
        {
            "model": "claude-sonnet",
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "input_audio", "input_audio": {"data": "AAAA"}}],
                }
            ],
        }
    )

    assert completed.returncode == 2
    assert "input_audio" in completed.stderr


def test_missing_model_is_refused() -> None:
    completed = _translate({"messages": [{"role": "user", "content": "hi"}]})

    assert completed.returncode == 2
    assert "model" in completed.stderr


def test_client_cache_breakpoints_survive_translation() -> None:
    control = {"type": "ephemeral", "ttl": "1h"}
    result = _translate(
        {
            "model": "claude-test",
            "messages": [
                {
                    "role": "system",
                    "content": [{"type": "text", "text": "stable", "cache_control": control}],
                },
                {"role": "user", "content": "hello"},
            ],
        }
    )
    assert result.returncode == 0, result.stderr
    translated = json.loads(result.stdout)
    assert translated["system"][0]["cache_control"] == control
    assert "cache_control" not in translated["messages"][0]["content"][0]
