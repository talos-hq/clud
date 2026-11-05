"""End-to-end coverage for the OpenAI Responses ingress translator.

Ports CLIProxyAPI
``internal/translator/claude/openai/responses/claude_openai-responses_request.go``.
The Rust unit tests cover the mapping table; these run the real binary through its
hidden ``__openai-responses-translate`` seam, so the process boundary (stdin in,
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
        [str(_clud_binary()), "__openai-responses-translate"],
        input=json.dumps(request),
        capture_output=True,
        text=True,
        timeout=30,
    )


def _translate_response(
    body: object, *flags: str
) -> process.CompletedProcess[object]:
    return process.run(
        [str(_clud_binary()), "__openai-responses-translate", "--response", *flags],
        input=body if isinstance(body, str) else json.dumps(body),
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_a_responses_conversation_becomes_a_messages_request() -> None:
    completed = _translate(
        {
            "model": "claude-sonnet",
            "instructions": "be terse",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read_file",
                    "arguments": '{"path":"a.rs"}',
                },
                {"type": "function_call_output", "call_id": "call_1", "output": "fn main() {}"},
            ],
            "max_output_tokens": 512,
            "tools": [
                {
                    "type": "function",
                    "name": "read_file",
                    "description": "read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                }
            ],
            "tool_choice": "required",
        }
    )

    assert completed.returncode == 0, completed.stderr
    body = json.loads(completed.stdout)
    assert body["model"] == "claude-sonnet"
    assert body["max_tokens"] == 512
    assert body["stream"] is False
    assert body["system"][0]["text"] == "be terse"
    assert body["messages"][0]["role"] == "user"
    tool_use = body["messages"][1]["content"][0]
    assert tool_use["type"] == "tool_use"
    assert tool_use["input"] == {"path": "a.rs"}
    tool_result = body["messages"][2]["content"][0]
    assert tool_result["type"] == "tool_result"
    assert tool_result["tool_use_id"] == "call_1"
    assert body["tool_choice"] == {"type": "any"}


def test_streaming_is_carried_through() -> None:
    completed = _translate({"model": "claude-sonnet", "input": "hi", "stream": True})

    assert completed.returncode == 0, completed.stderr
    assert json.loads(completed.stdout)["stream"] is True


def test_a_reasoning_item_with_a_signature_becomes_thinking() -> None:
    completed = _translate(
        {
            "model": "claude-sonnet",
            "input": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "weighing options"}],
                    "encrypted_content": "sig-abc",
                },
                {"role": "user", "content": "go"},
            ],
        }
    )

    assert completed.returncode == 0, completed.stderr
    block = json.loads(completed.stdout)["messages"][0]["content"][0]
    assert block == {
        "type": "thinking",
        "thinking": "weighing options",
        "signature": "sig-abc",
    }


def test_a_builtin_server_tool_is_refused_loudly() -> None:
    completed = _translate(
        {"model": "claude-sonnet", "input": "hi", "tools": [{"type": "web_search"}]}
    )

    assert completed.returncode == 2
    assert completed.stdout == ""
    assert "tool:web_search" in completed.stderr


def test_a_request_without_a_model_is_refused() -> None:
    completed = _translate({"input": "hi"})

    assert completed.returncode == 2
    assert "missing a model" in completed.stderr


# ---------------------------------------------------------------------------
# Response half: an upstream Messages answer becomes the Responses shape the
# client asked for (CLIProxyAPI claude_openai-responses_response.go).
# ---------------------------------------------------------------------------


def _messages_answer() -> dict[str, object]:
    return {
        "id": "msg_py_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-x",
        "content": [
            {"type": "thinking", "thinking": "short plan", "signature": "sig-py"},
            {"type": "text", "text": "the answer"},
            {
                "type": "tool_use",
                "id": "toolu_py",
                "name": "read_file",
                "input": {"path": "b.rs"},
            },
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 4, "cache_read_input_tokens": 2},
    }


def test_a_messages_answer_is_rendered_as_a_response_object() -> None:
    completed = _translate_response(_messages_answer())

    assert completed.returncode == 0, completed.stderr
    response = json.loads(completed.stdout)
    assert response["object"] == "response"
    assert response["status"] == "completed"
    assert response["model"] == "claude-x"
    kinds = [item["type"] for item in response["output"]]
    assert kinds == ["reasoning", "message", "function_call"], kinds
    assert response["output"][1]["content"][0] == {
        "type": "output_text",
        "annotations": [],
        "logprobs": [],
        "text": "the answer",
    }
    assert response["output"][2]["arguments"] == '{"path":"b.rs"}'
    assert response["usage"]["input_tokens"] == 7
    assert response["usage"]["input_tokens_details"]["cached_tokens"] == 2
    assert response["usage"]["total_tokens"] == 11


def test_a_max_tokens_stop_is_reported_incomplete() -> None:
    answer = _messages_answer()
    answer["stop_reason"] = "max_tokens"
    completed = _translate_response(answer)

    assert completed.returncode == 0, completed.stderr
    response = json.loads(completed.stdout)
    assert response["status"] == "incomplete"
    assert response["incomplete_details"] == {"reason": "max_output_tokens"}


def test_a_streamed_answer_emits_response_events() -> None:
    frames = [
        {
            "type": "message_start",
            "message": {"id": "msg_sse", "model": "claude-x", "usage": {"input_tokens": 3}},
        },
        {
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        },
        {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hi"},
        },
        {"type": "content_block_stop", "index": 0},
        {
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 1},
        },
        {"type": "message_stop"},
    ]
    body = "".join(f"event: {frame['type']}\ndata: {json.dumps(frame)}\n\n" for frame in frames)
    completed = _translate_response(body, "--stream")

    assert completed.returncode == 0, completed.stderr
    events = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
    kinds = [event["type"] for event in events]
    assert kinds[-1] == "response.completed", kinds
    assert "response.output_text.delta" in kinds, kinds
    assert [event["sequence_number"] for event in events] == list(range(len(events)))
    assert events[-1]["response"]["output"][0]["content"][0]["text"] == "hi"


def test_the_http_stream_frames_events_with_their_names() -> None:
    frames = [
        {"type": "message_start", "message": {"id": "msg_http"}},
        {"type": "message_stop"},
    ]
    body = "".join(f"data: {json.dumps(frame)}\n\n" for frame in frames)
    completed = _translate_response(body, "--stream", "--http")

    assert completed.returncode == 0, completed.stderr
    text = completed.stdout
    assert text.startswith("event: response.created\n"), text
    assert "event: response.completed\n" in text, text
    for frame in text.rstrip("\n").split("\n\n"):
        name = next(line for line in frame.splitlines() if line.startswith("event: "))[7:]
        payload = json.loads(
            next(line for line in frame.splitlines() if line.startswith("data: "))[6:]
        )
        assert payload["type"] == name, frame


def test_a_body_that_ends_without_message_stop_fails_the_response() -> None:
    frames = [
        {"type": "message_start", "message": {"id": "msg_cut"}},
        {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
        {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ha"}},
    ]
    body = "".join(f"data: {json.dumps(frame)}\n\n" for frame in frames)
    completed = _translate_response(body, "--stream")

    assert completed.returncode == 0, completed.stderr
    events = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
    assert events[-1]["type"] == "response.failed", events[-1]
    assert events[-1]["response"]["status"] == "failed"
    assert events[-1]["response"]["error"]["code"] == "server_error"


def test_a_response_body_that_is_not_json_is_refused() -> None:
    completed = _translate_response("definitely not json")

    assert completed.returncode == 2
    assert completed.stdout == ""
    assert "JSON" in completed.stderr
