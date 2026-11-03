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
