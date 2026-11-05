"""End-to-end coverage for JSON-path payload rules.

Ports CLIProxyAPI's ``requests.payload`` (``config.example.yaml:266-334``): a
rule selects on model (``*`` wildcards) and protocol, and sets (``default`` /
``override``) or removes (``filter``) parameters by dotted JSON path in the
request body the gateway forwards.

The Rust unit tests cover the matcher and path arithmetic; these run the real
binary through its hidden ``__apply-payload-rules`` seam so the process
boundary — stdin document in, rewritten payload out, exit status on a bad rules
file — is exercised too.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any

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


def _apply(
    rules: dict[str, Any],
    payload: dict[str, Any],
    model: str,
    protocol: str = "claude",
    headers: dict[str, str] | None = None,
    from_protocol: str | None = None,
) -> dict[str, Any]:
    command = [
        str(_clud_binary()),
        "__apply-payload-rules",
        "--model",
        model,
        "--protocol",
        protocol,
    ]
    if from_protocol is not None:
        command += ["--from-protocol", from_protocol]
    for name, value in (headers or {}).items():
        command += ["--header", f"{name}: {value}"]
    completed = process.run(
        command,
        input=json.dumps({"rules": rules, "payload": payload}),
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert completed.returncode == 0, completed.stderr
    return json.loads(completed.stdout)


def test_default_fills_absent_params_and_override_replaces_them() -> None:
    rules = {
        "default": [
            {
                "models": [{"name": "claude-*", "protocol": "claude"}],
                "params": {"temperature": 0.2, "max_tokens": 4096},
            }
        ],
        "override": [
            {"models": [{"name": "claude-*"}], "params": {"temperature": 0.0}},
        ],
    }
    result = _apply(rules, {"model": "claude-sonnet-4", "temperature": 1.0}, "claude-sonnet-4")
    payload = result["payload"]
    assert payload["max_tokens"] == 4096, "default fills what the caller omitted"
    assert payload["temperature"] == 0.0, "override wins over both"
    assert result["outcome"] == {
        "defaulted": 1,
        "overridden": 1,
        "filtered": 0,
        "problems": [],
        "changed": True,
    }


def test_filter_removes_a_path_and_a_selector_miss_changes_nothing() -> None:
    rules = {
        "filter": [
            {"models": [{"name": "claude-*"}], "params": ["metadata.user_id"]},
        ]
    }
    hit = _apply(
        rules,
        {"model": "claude-opus-4", "metadata": {"user_id": "probe", "keep": 1}},
        "claude-opus-4",
    )
    assert hit["payload"]["metadata"] == {"keep": 1}
    assert hit["outcome"]["filtered"] == 1

    missed = _apply(rules, {"model": "gpt-6-sol", "metadata": {"user_id": "x"}}, "gpt-6-sol")
    assert missed["payload"]["metadata"] == {"user_id": "x"}
    assert missed["outcome"]["changed"] is False


def test_rules_file_is_loaded_from_disk_and_a_typo_is_refused(tmp_path: Path) -> None:
    rules_file = tmp_path / "payload-rules.json"
    rules_file.write_text(
        json.dumps(
            {
                "override": [
                    {"models": [{"name": "gpt-*", "protocol": "codex"}],
                     "params": {"reasoning.effort": "high"}}
                ]
            }
        ),
        encoding="utf-8",
    )
    completed = process.run(
        [
            str(_clud_binary()),
            "__apply-payload-rules",
            "--rules",
            str(rules_file),
            "--model",
            "gpt-6-sol",
            "--protocol",
            "codex",
        ],
        input=json.dumps({"payload": {"reasoning": {"effort": "low"}}}),
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert completed.returncode == 0, completed.stderr
    assert json.loads(completed.stdout)["payload"]["reasoning"]["effort"] == "high"

    broken = tmp_path / "broken.json"
    broken.write_text('{"overide": []}', encoding="utf-8")
    refused = process.run(
        [str(_clud_binary()), "__apply-payload-rules", "--rules", str(broken)],
        input="{}",
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert refused.returncode == 2, "a typo in a rule key must not be silently ignored"
    assert "unknown field" in refused.stderr


def test_query_selectors_reach_array_elements_by_field(tmp_path: Path) -> None:
    """gjson query selectors, including upstream's chained all-matches form.

    CLIProxyAPI writes payload paths in gjson syntax and its own filter example
    is ``input.#(type=="additional_tools")#.tools.#(name=="functions")#...``
    (``config.example.yaml:334``); a dotted path alone cannot name an array
    element, so this pins the selector grammar end to end through the binary.
    """
    rules_file = tmp_path / "rules.json"
    rules_file.write_text(
        json.dumps(
            {
                "filter": [
                    {"models": [{"name": "gpt-*", "protocol": "codex"}],
                     "params": [
                         'input.#(type=="additional_tools")#.tools'
                         '.#(name=="functions")#.tools.#(name=="apply_patch")#'
                     ]}
                ],
                "override": [
                    {"models": [{"name": "gpt-*", "protocol": "codex"}],
                     "params": {'tools.#(name=="WebSearch").type': "disabled"}}
                ],
            }
        ),
        encoding="utf-8",
    )
    completed = process.run(
        [
            str(_clud_binary()),
            "__apply-payload-rules",
            "--rules",
            str(rules_file),
            "--model",
            "gpt-6-sol",
            "--protocol",
            "codex",
        ],
        input=json.dumps(
            {
                "payload": {
                    "tools": [{"name": "WebSearch"}, {"name": "apply_patch"}],
                    "input": [
                        {"type": "message"},
                        {
                            "type": "additional_tools",
                            "tools": [
                                {
                                    "name": "functions",
                                    "tools": [
                                        {"name": "apply_patch"},
                                        {"name": "shell"},
                                    ],
                                }
                            ],
                        },
                    ],
                }
            }
        ),
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert completed.returncode == 0, completed.stderr
    result = json.loads(completed.stdout)
    payload = result["payload"]
    assert payload["input"][1]["tools"][0]["tools"] == [{"name": "shell"}]
    assert payload["input"][0] == {"type": "message"}
    # The filter addresses the nested Codex envelope only; the flat top-level
    # tools array keeps both entries, one of them freshly typed by the override.
    assert payload["tools"] == [
        {"name": "WebSearch", "type": "disabled"},
        {"name": "apply_patch"},
    ]
    assert result["outcome"]["filtered"] == 1
    assert result["outcome"]["overridden"] == 1
    assert result["outcome"]["problems"] == []


def test_a_malformed_selector_in_a_filter_never_fails_the_request() -> None:
    result = _apply(
        {"filter": [{"params": ["tools.#(name)"]}], "override": [{"params": {"temperature": 0.1}}]},
        {"tools": [{"name": "WebSearch"}]},
        "claude-sonnet-4",
    )
    assert result["payload"]["tools"] == [{"name": "WebSearch"}]
    assert result["payload"]["temperature"] == 0.1
    assert result["outcome"]["filtered"] == 0
    assert len(result["outcome"]["problems"]) == 1
    assert "must compare one field" in result["outcome"]["problems"][0]


def test_a_selector_only_write_path_is_refused_when_the_file_loads(tmp_path: Path) -> None:
    """A selector names an element, not a member, so a write needs a trailing key."""
    rules_file = tmp_path / "selector-only.json"
    rules_file.write_text(
        json.dumps({"override": [{"params": {'tools.#(name=="WebSearch")': "x"}}]}),
        encoding="utf-8",
    )
    completed = process.run(
        [str(_clud_binary()), "__apply-payload-rules", "--rules", str(rules_file)],
        input=json.dumps({"payload": {"tools": []}}),
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert completed.returncode == 2
    assert "add the field to set" in completed.stderr


def test_a_header_condition_gates_a_rule_on_the_callers_own_headers() -> None:
    """Upstream `headers` conditions: every configured header must match."""
    rules = {
        "override": [
            {
                "models": [{"name": "claude-*", "headers": {"X-Client-Tier": "tenant-*-region-*"}}],
                "params": {"metadata.tier_seen": True},
            }
        ]
    }
    matched = _apply(
        rules,
        {"model": "claude-sonnet-4", "metadata": {}},
        "claude-sonnet-4",
        headers={"X-Client-Tier": "tenant-acme-region-eu"},
    )
    assert matched["payload"]["metadata"]["tier_seen"] is True
    assert matched["outcome"]["overridden"] == 1

    # A header the request does not carry cannot match, and neither can a
    # value outside the pattern.
    for headers in ({}, {"X-Client-Tier": "free-tier"}):
        missed = _apply(
            rules, {"model": "claude-sonnet-4", "metadata": {}}, "claude-sonnet-4", headers=headers
        )
        assert "tier_seen" not in missed["payload"]["metadata"]
        assert missed["outcome"]["changed"] is False


def test_match_not_match_exist_and_not_exist_select_on_payload_shape() -> None:
    rules = {
        "override": [
            {"models": [{"name": "claude-*", "match": [{"metadata.client": "codex"}]}],
             "params": {"metadata.matched": True}},
            {"models": [{"name": "claude-*", "not-match": [{"metadata.mode": "dev"}]}],
             "params": {"metadata.not_matched": True}},
            {"models": [{"name": "claude-*", "exist": ["metadata.user_id"]}],
             "params": {"metadata.existed": True}},
            {"models": [{"name": "claude-*", "not-exist": ["metadata.disable_payload"]}],
             "params": {"metadata.not_existed": True}},
        ]
    }
    hit = _apply(
        rules,
        {"model": "claude-sonnet-4", "metadata": {"client": "codex", "user_id": "u"}},
        "claude-sonnet-4",
    )
    metadata = hit["payload"]["metadata"]
    assert metadata["matched"] is True
    assert metadata["not_matched"] is True
    assert metadata["existed"] is True
    assert metadata["not_existed"] is True
    assert hit["outcome"]["overridden"] == 4

    missed = _apply(
        rules,
        {"model": "claude-sonnet-4", "metadata": {"client": "claude", "mode": "dev",
                                                  "disable_payload": 1}},
        "claude-sonnet-4",
    )
    assert missed["payload"]["metadata"] == {"client": "claude", "mode": "dev",
                                             "disable_payload": 1}
    assert missed["outcome"]["changed"] is False


def test_a_malformed_condition_path_is_reported_and_skips_only_its_rule() -> None:
    """A bad condition path must not fail the request nor the good rules."""
    rules = {
        "override": [
            {"models": [{"name": "claude-*", "match": [{"a..b": 1}]}],
             "params": {"metadata.bad": True}},
            {"models": [{"name": "claude-*"}], "params": {"metadata.good": True}},
        ]
    }
    result = _apply(rules, {"model": "claude-sonnet-4", "metadata": {}}, "claude-sonnet-4")
    assert "bad" not in result["payload"]["metadata"]
    assert result["payload"]["metadata"]["good"] is True
    assert len(result["outcome"]["problems"]) == 1
    assert "empty path segment" in result["outcome"]["problems"][0]


def test_a_header_without_a_colon_is_refused_by_the_seam() -> None:
    completed = process.run(
        [str(_clud_binary()), "__apply-payload-rules", "--header", "X-Broken"],
        input=json.dumps({"rules": {}, "payload": {}}),
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert completed.returncode == 2
    assert "not a header" in completed.stderr


def test_raw_rules_set_json_text_and_are_refused_when_unparsable(tmp_path: Path) -> None:
    """``default-raw``/``override-raw`` keep the value as JSON, not as a string.

    CLIProxyAPI's own example (``config.example.yaml:296-321``) pastes a JSON
    schema into a parameter; a plain string value would reach the provider as
    text, which is the bug this pins.
    """
    rules = {
        "default-raw": [
            {
                "models": [{"name": "gemini-*"}],
                "params": {"generationConfig.responseJsonSchema": '{"type":"object"}'},
            }
        ],
        "override-raw": [
            {"models": [{"name": "gemini-*"}], "params": {"candidateCount": "2"}}
        ],
    }
    result = _apply(
        rules,
        {"model": "gemini-2.5-pro", "candidateCount": 1},
        "gemini-2.5-pro",
        protocol="gemini",
    )
    payload = result["payload"]
    assert payload["generationConfig"]["responseJsonSchema"] == {"type": "object"}
    assert payload["candidateCount"] == 2
    assert result["outcome"] == {
        "defaulted": 1,
        "overridden": 1,
        "filtered": 0,
        "problems": [],
        "changed": True,
    }

    broken = tmp_path / "raw-broken.json"
    broken.write_text(
        json.dumps({"default-raw": [{"params": {"schema": "{not json"}}]}),
        encoding="utf-8",
    )
    refused = process.run(
        [str(_clud_binary()), "__apply-payload-rules", "--rules", str(broken)],
        input=json.dumps({"payload": {}}),
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert refused.returncode == 2, "an unparsable raw value must fail the file, not the request"
    assert "raw value is not JSON" in refused.stderr


def test_from_protocol_selects_on_the_shape_the_client_spoke() -> None:
    """CLIProxyAPI's ``from-protocol`` selector (``config_types.go:600``).

    Upstream applies payload rules in the executor with ``protocol`` = the
    target protocol and ``fromProtocol`` = the shape the client spoke, so one
    rules file can rewrite chat-completions callers without touching Messages
    callers that hit the same route. The process seam must expose the same
    distinction, including "unknown source cannot satisfy a pinned pattern".
    """
    rules = {
        "override": [
            {"models": [{"name": "claude-*", "from-protocol": "openai"}],
             "params": {"max_tokens": 32}},
            {"models": [{"name": "claude-*", "from-protocol": "claude"}],
             "params": {"metadata.via": "messages"}},
        ]
    }
    chat = _apply(
        rules,
        {"max_tokens": 1024},
        "claude-sonnet-4",
        from_protocol="openai",
    )
    assert chat["payload"]["max_tokens"] == 32
    assert "metadata" not in chat["payload"], "the Claude-shaped rule must not fire"

    messages = _apply(
        rules,
        {"max_tokens": 1024},
        "claude-sonnet-4",
        from_protocol="claude",
    )
    assert messages["payload"]["max_tokens"] == 1024
    assert messages["payload"]["metadata"]["via"] == "messages"

    unknown = _apply(rules, {"max_tokens": 1024}, "claude-sonnet-4")
    assert unknown["payload"]["max_tokens"] == 1024
    assert "metadata" not in unknown["payload"]
    assert unknown["outcome"]["changed"] is False


def _apply_from_settings(
    settings: dict[str, Any], payload: dict[str, Any], model: str, tmp_path: Path
) -> tuple[int, str, dict[str, Any] | None]:
    settings_file = tmp_path / "settings.json"
    settings_file.write_text(json.dumps(settings))
    completed = process.run(
        [
            str(_clud_binary()),
            "__apply-payload-rules",
            "--settings-file",
            str(settings_file),
            "--model",
            model,
        ],
        input=json.dumps({"payload": payload}),
        capture_output=True,
        text=True,
        timeout=30,
    )
    parsed = json.loads(completed.stdout) if completed.returncode == 0 else None
    return completed.returncode, completed.stderr, parsed


def test_settings_json_owns_the_rules_under_requests_payload(tmp_path: Path) -> None:
    """`~/.clud/settings.json` is the owner when CLUD_PAYLOAD_RULES is unset.

    The key mirrors upstream's own config section, so a rules file an operator
    already has can be pasted under `requests.payload` unchanged.
    """
    settings = {
        "requests": {
            "payload": {
                "override": [
                    {"models": [{"name": "claude-*"}], "params": {"temperature": 0.3}}
                ]
            }
        },
        "model_contexts": {"windows": {"claude-sonnet-4": 200000}},
    }
    code, stderr, result = _apply_from_settings(
        settings, {"model": "claude-sonnet-4", "temperature": 1.0}, "claude-sonnet-4", tmp_path
    )
    assert code == 0, stderr
    assert result is not None
    assert result["payload"]["temperature"] == 0.3
    assert result["outcome"]["overridden"] == 1


def test_settings_without_the_payload_key_is_a_named_refusal(tmp_path: Path) -> None:
    """A settings file that carries no rules must say so, not silently pass."""
    code, stderr, _ = _apply_from_settings(
        {"model_contexts": {"windows": {}}},
        {"model": "claude-sonnet-4"},
        "claude-sonnet-4",
        tmp_path,
    )
    assert code == 2
    assert "requests.payload" in stderr, stderr


def test_settings_payload_with_a_typo_is_refused_rather_than_ignored(tmp_path: Path) -> None:
    code, stderr, _ = _apply_from_settings(
        {"requests": {"payload": {"defualt": []}}},
        {"model": "claude-sonnet-4"},
        "claude-sonnet-4",
        tmp_path,
    )
    assert code == 2
    assert "requests.payload" in stderr, stderr
