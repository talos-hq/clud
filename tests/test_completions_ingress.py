"""End-to-end coverage for the legacy OpenAI completions ingress.

Ports CLIProxyAPI ``sdk/api/handlers/openai/openai_handlers.go`` (``Completions``,
``convertCompletionsRequestToChatCompletions``,
``convertChatCompletionsResponseToCompletions``), served at
``internal/api/server_routes.go:74``. The Rust unit tests cover the mapping
table; these run the real binary through its hidden
``__openai-completions-translate`` seam, so the process boundary (stdin in,
Messages body / ``text_completion`` body out, exit status on refusal) is
exercised too.
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


def _translate(request: dict[str, object]) -> process.CompletedProcess[object]:
    return process.run(
        [str(_clud_binary()), "__openai-completions-translate"],
        input=json.dumps(request),
        capture_output=True,
        text=True,
        timeout=30,
    )


def _translate_response(
    body: object, *flags: str
) -> process.CompletedProcess[object]:
    return process.run(
        [str(_clud_binary()), "__openai-completions-translate", "--response", *flags],
        input=body if isinstance(body, str) else json.dumps(body),
        capture_output=True,
        text=True,
        timeout=30,
    )


MESSAGES_BODY = {
    "id": "msg_completions_1",
    "type": "message",
    "role": "assistant",
    "model": "claude-wire-name",
    "content": [{"type": "text", "text": "hello from messages"}],
    "stop_reason": "end_turn",
    "usage": {"input_tokens": 11, "output_tokens": 7},
}

MESSAGES_STREAM = "\n".join(
    [
        "event: message_start",
        'data: {"type":"message_start","message":{"id":"msg_completions_2",'
        '"type":"message","role":"assistant","model":"claude-wire-name",'
        '"content":[],"stop_reason":null,"usage":{"input_tokens":9,"output_tokens":1}}}',
        "",
        "event: content_block_start",
        'data: {"type":"content_block_start","index":0,'
        '"content_block":{"type":"text","text":""}}',
        "",
        "event: content_block_delta",
        'data: {"type":"content_block_delta","index":0,'
        '"delta":{"type":"text_delta","text":"hello "}}',
        "",
        "event: content_block_stop",
        'data: {"type":"content_block_stop","index":0}',
        "",
        "event: message_delta",
        'data: {"type":"message_delta","delta":{"stop_reason":"end_turn",'
        '"stop_sequence":null},"usage":{"output_tokens":5}}',
        "",
        "event: message_stop",
        'data: {"type":"message_stop"}',
        "",
    ]
)


def test_a_prompt_becomes_one_messages_user_turn() -> None:
    completed = _translate(
        {
            "model": "clud-claude-sonnet",
            "prompt": "say hi",
            "max_tokens": 64,
            "temperature": 0.2,
            "stop": "END",
        }
    )
    assert completed.returncode == 0, completed.stderr
    translated = json.loads(completed.stdout)
    assert translated["model"] == "clud-claude-sonnet"
    assert translated["max_tokens"] == 64
    assert translated["temperature"] == 0.2
    assert translated["stop_sequences"] == ["END"]
    assert translated["messages"][0]["role"] == "user"
    # The chat translator emits content as text blocks, not a bare string.
    assert translated["messages"][0]["content"][0]["text"] == "say hi"
    assert translated["stream"] is False


def test_a_prompt_array_keeps_every_line() -> None:
    completed = _translate({"model": "m", "prompt": ["first", "second"]})
    assert completed.returncode == 0, completed.stderr
    content = json.loads(completed.stdout)["messages"][0]["content"]
    assert content[0]["text"] == "first\nsecond"


def test_a_token_id_prompt_is_refused_by_name() -> None:
    completed = _translate({"model": "m", "prompt": [[1, 2, 3]]})
    assert completed.returncode == 2
    assert "prompt:token_ids" in completed.stderr


def test_an_empty_prompt_is_refused_rather_than_invented() -> None:
    completed = _translate({"model": "m", "prompt": ""})
    assert completed.returncode == 2
    assert "no prompt" in completed.stderr
    assert completed.stdout == ""


def test_multiple_choices_are_refused_not_narrowed() -> None:
    completed = _translate({"model": "m", "prompt": "p", "n": 4})
    assert completed.returncode == 2
    assert "`n`" in completed.stderr


def test_a_buffered_reply_becomes_one_text_completion() -> None:
    completed = _translate_response(MESSAGES_BODY, "--for-model", "asked-for")
    assert completed.returncode == 0, completed.stderr
    translated = json.loads(completed.stdout)
    assert translated["object"] == "text_completion"
    assert translated["id"] == "msg_completions_1"
    assert translated["model"] == "claude-wire-name"
    assert translated["choices"][0]["text"] == "hello from messages"
    assert translated["choices"][0]["finish_reason"] == "stop"
    assert translated["choices"][0]["logprobs"] is None
    assert translated["usage"]["completion_tokens"] == 7


def test_a_streamed_reply_becomes_text_completion_chunks() -> None:
    completed = _translate_response(
        MESSAGES_STREAM, "--stream", "--for-model", "asked-for"
    )
    assert completed.returncode == 0, completed.stderr
    chunks = [json.loads(line) for line in completed.stdout.splitlines() if line]
    assert chunks, completed.stdout
    assert all(chunk["object"] == "text_completion" for chunk in chunks)
    # The stream echoes the model the upstream reported, like the chat door.
    assert {chunk["model"] for chunk in chunks} == {"claude-wire-name"}
    text = "".join(
        choice["text"] for chunk in chunks for choice in chunk["choices"]
    )
    assert text == "hello "
    assert any(
        choice["finish_reason"] == "stop"
        for chunk in chunks
        for choice in chunk["choices"]
    )


def test_the_http_stream_body_ends_with_the_done_sentinel() -> None:
    completed = _translate_response(
        MESSAGES_STREAM, "--stream", "--http", "--for-model", "asked-for"
    )
    assert completed.returncode == 0, completed.stderr
    body = completed.stdout.rstrip()
    assert body.startswith("data: "), body
    assert body.endswith("data: [DONE]"), body
    assert "chat.completion.chunk" not in body
