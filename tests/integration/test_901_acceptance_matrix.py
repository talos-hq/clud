"""#901 integrated acceptance: auth, discovery, cross-provider effort, logout.

The child issues (#900, #898, #899) each carry focused unit and route tests.
This module is the meta issue's joint acceptance event on the real binary:

* credentials enter through the same vault ``clud auth`` uses (a debug-only,
  integration-gated file vault: CI runners have no OS keyring), and
  ``clud auth status`` / ``clud auth logout`` run through the binary;
* one ``--unified`` launch serves honestly labelled discovery rows, crosses
  Claude -> Codex -> DeepSeek -> Claude with a different ``/effort`` each turn,
  and rejects an unknown reserved model locally;
* logging one provider out removes only its rows on the next launch;
* every provider gets a unique canary credential, and each canary appears
  only in its own upstream's ``Authorization`` header -- never in another
  upstream, a request body, or clud's stdout/stderr.

``clud auth login`` itself prompts on a terminal and probes the provider's
live API, so its prompt and probe stay covered by the injected-store unit
tests in ``provider_auth.rs``; here the vault is seeded with exactly the
record a login writes.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from tests import process
from tests.metered_consent import consent_unified

from .test_mock_agents import _FakeAnthropicServer, _FakeResponsesServer, _run

pytestmark = pytest.mark.integration

DEEPSEEK_CANARY = "sk-canary-deepseek-901"
OPENROUTER_CANARY = "sk-canary-openrouter-901"
AMBIENT_CANARY = "sk-canary-ambient-anthropic-901"
ALL_CANARIES = (DEEPSEEK_CANARY, OPENROUTER_CANARY, AMBIENT_CANARY)

# Mirrors provider_auth::test_vault_path for each provider's vault identifiers.
VAULT_FILES = {
    "deepseek": "clud_deepseek--api_key_v1.secret",
    "kimi": "clud_kimi--api_key_v1.secret",
    "openrouter": "clud_openrouter--api_key_v1.secret",
}
KIMI_CANARY = "sk-canary-kimi-937"


@pytest.fixture
def upstreams():
    responses = _FakeResponsesServer()
    anthropic = {name: _FakeAnthropicServer(name) for name in ("claude", "deepseek", "openrouter")}
    try:
        yield responses, anthropic
    finally:
        responses.close()
        for server in anthropic.values():
            server.close()


def _vault(tmp_path: Path) -> Path:
    vault = tmp_path / "test-vault"
    vault.mkdir(exist_ok=True)
    return vault


def _seed(vault: Path, provider: str, secret: str) -> None:
    """Write exactly the record `clud auth login <provider>` stores."""
    (vault / VAULT_FILES[provider]).write_text(secret, encoding="utf-8")


def _env(mock_env: dict[str, str], vault: Path, upstreams=None) -> dict[str, str]:
    env = mock_env.copy()
    env["CLUD_INTEGRATION_TESTS"] = "1"
    env["CLUD_TEST_SECRET_STORE_DIR"] = str(vault)
    env["ANTHROPIC_API_KEY"] = AMBIENT_CANARY
    if upstreams is not None:
        responses, anthropic = upstreams
        env["CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL"] = responses.base_url
        env["CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL"] = anthropic["claude"].base_url
        env["CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL"] = anthropic["deepseek"].base_url
        env["CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL"] = anthropic["openrouter"].base_url
        env = consent_unified(env)
    return env


def _auth(clud: Path, env: dict[str, str], *args: str) -> dict[str, Any]:
    result = _run(clud, "auth", *args, "--json", env=env)
    assert result.returncode == 0, result.stderr
    for canary in ALL_CANARIES:
        assert canary not in result.stdout + result.stderr, f"auth {args} leaked {canary}"
    return json.loads(result.stdout)


def _status(clud: Path, env: dict[str, str]) -> dict[str, str]:
    return {row["provider"]: row["status"] for row in _auth(clud, env, "status")["providers"]}


def _launch_unified(
    clud: Path, env: dict[str, str], probe: Path, *probe_args: str
) -> tuple[process.CompletedProcess[str], dict[str, Any]]:
    result = _run(
        clud,
        "--unified",
        "--harness",
        "claude",
        "--subprocess",
        "-p",
        "901 acceptance",
        "--",
        "--mock-unified-acceptance-probe",
        str(probe),
        *probe_args,
        env=env,
    )
    assert result.returncode == 0, result.stderr
    return result, json.loads(probe.read_text(encoding="utf-8"))


def _split(raw: bytes) -> tuple[bytes, dict[str, Any]]:
    head, _, body = raw.partition(b"\r\n\r\n")
    return head, json.loads(body)


def _header(head: bytes, name: bytes) -> bytes | None:
    for line in head.split(b"\r\n"):
        candidate, _, value = line.partition(b":")
        if candidate.lower() == name.lower():
            return value.strip()
    return None


def _row_ids(probe: dict[str, Any]) -> set[str]:
    return {row["id"] for row in probe["models"]}


def test_auth_status_and_logout_run_through_the_binary(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """Step 1: status and logout are secret-free and touch only clud's own records."""
    vault = _vault(tmp_path)
    env = _env(mock_env, vault)
    assert _status(clud_binary, env)["deepseek"] == "login_required"

    _seed(vault, "deepseek", DEEPSEEK_CANARY)
    _seed(vault, "openrouter", OPENROUTER_CANARY)
    status = _status(clud_binary, env)
    assert status["deepseek"] == "configured"
    assert status["openrouter"] == "configured"
    assert status["claude"] == "externally_managed"

    assert _auth(clud_binary, env, "logout", "deepseek") == {"removed": True}
    status = _status(clud_binary, env)
    assert status["deepseek"] == "login_required"
    assert status["openrouter"] == "configured", "logout must not touch another provider"
    assert not (vault / VAULT_FILES["deepseek"]).exists()
    assert (vault / VAULT_FILES["openrouter"]).exists()


@pytest.mark.parametrize("flag", ["--kimi", "--deepseek", "--openrouter"])
def test_dry_run_makes_zero_vault_calls(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, flag: str
) -> None:
    """#936: dry-run never touches the vault. Each provider's record in the
    test vault is a directory, so any read fails as "vault unavailable" on
    every OS (a file under a regular file reads as NotFound on Windows,
    which would look like "not configured" instead). The live control
    launch proves the poison works; the dry run must not notice it."""
    vault = _vault(tmp_path)
    for record in VAULT_FILES.values():
        (vault / record).mkdir()
    env = _env(mock_env, vault)

    live = _run(clud_binary, flag, "-p", "hello", env=env)
    assert live.returncode == 2, live.stderr
    assert "the native credential vault is unavailable" in live.stderr

    dry = _run(clud_binary, "--dry-run", flag, "-p", "hello", env=env)
    assert dry.returncode == 0, dry.stderr
    assert json.loads(dry.stdout)["model_provider"] == flag.removeprefix("--")
    assert "vault" not in dry.stderr


def test_unified_session_discovers_switches_routes_effort_and_isolates_credentials(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, upstreams
) -> None:
    """Steps 4-8 plus the canary sweep, in one launched session."""
    responses, anthropic = upstreams
    vault = _vault(tmp_path)
    _seed(vault, "deepseek", DEEPSEEK_CANARY)
    _seed(vault, "openrouter", OPENROUTER_CANARY)
    env = _env(mock_env, vault, upstreams)

    result, probe = _launch_unified(clud_binary, env, tmp_path / "probe.json")
    assert probe["error"] is None, probe

    # Step 5: discovery is served, and every row names its provider honestly.
    assert probe["models_status"] == 200
    labels = {
        "codex": ("OpenAI", "Codex"),
        "deepseek": ("DeepSeek",),
        "openrouter": ("OpenRouter",),
    }
    routed = {row["id"].split("-")[2] for row in probe["models"]}
    assert {"codex", "deepseek", "openrouter"} <= routed, probe["models"]
    for row in probe["models"]:
        provider = row["id"].split("-")[2]
        assert any(word in row["display_name"] for word in labels.get(provider, (provider,))), row

    # Steps 6-8: the four turns route by model; the unknown id fails locally.
    assert probe["turn_statuses"] == [200, 200, 200, 200]
    assert 400 <= probe["unknown_model_status"] < 500
    claude = [_split(raw) for raw in anthropic["claude"].requests]
    deepseek = [_split(raw) for raw in anthropic["deepseek"].requests]
    codex = [_split(raw) for raw in responses.requests]
    assert len(claude) == 2
    assert len(deepseek) == 1
    assert len(codex) == 1
    assert not anthropic["openrouter"].requests, "nothing was routed to OpenRouter"

    # Step 7: effort is provider-correct and changes between turns.
    assert [body["output_config"]["effort"] for _, body in claude] == ["low", "medium"]
    assert codex[0][1]["reasoning"]["effort"] == "high"
    assert "output_config" not in codex[0][1], "Codex receives Responses-shaped effort"
    assert json.dumps(deepseek[0][1]).count('"max"') >= 1

    # Security: each canary reaches only its own upstream's Authorization.
    assert _header(deepseek[0][0], b"authorization") == f"Bearer {DEEPSEEK_CANARY}".encode()
    everything = [raw for raw in anthropic["claude"].requests] + list(responses.requests)
    for raw in everything:
        assert DEEPSEEK_CANARY.encode() not in raw
        assert OPENROUTER_CANARY.encode() not in raw
    for raw in anthropic["deepseek"].requests:
        assert OPENROUTER_CANARY.encode() not in raw
        assert AMBIENT_CANARY.encode() not in raw
    for raw in everything + list(anthropic["deepseek"].requests):
        head, body = _split(raw)
        assert _header(head, b"x-clud-gateway-token") is None
        assert DEEPSEEK_CANARY not in json.dumps(body)
    for canary in ALL_CANARIES:
        assert canary not in result.stdout + result.stderr, f"launch output leaked {canary}"


def test_logging_out_one_provider_removes_only_its_discovery_rows(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, upstreams
) -> None:
    """Step 10: logout, relaunch, compare the row sets."""
    vault = _vault(tmp_path)
    _seed(vault, "deepseek", DEEPSEEK_CANARY)
    _seed(vault, "openrouter", OPENROUTER_CANARY)
    env = _env(mock_env, vault, upstreams)

    _, before = _launch_unified(clud_binary, env, tmp_path / "before.json")
    assert _auth(clud_binary, env, "logout", "deepseek") == {"removed": True}
    _, after = _launch_unified(clud_binary, env, tmp_path / "after.json")

    deepseek_rows = {row for row in _row_ids(before) if row.startswith("clud-claude-deepseek-")}
    assert deepseek_rows, "DeepSeek rows were advertised while logged in"
    assert _row_ids(after) == _row_ids(before) - deepseek_rows


def test_kimi_joins_the_unified_gateway_from_its_own_vault_record(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, upstreams
) -> None:
    """#937 Phase 4 through the binary: the Kimi key `clud auth` stored is
    what advertises `clud-claude-kimi-k3`, reaches only the Kimi upstream, and
    disappears from discovery on logout."""
    kimi = _FakeAnthropicServer("kimi")
    try:
        vault = _vault(tmp_path)
        _seed(vault, "deepseek", DEEPSEEK_CANARY)
        _seed(vault, "kimi", KIMI_CANARY)
        env = _env(mock_env, vault, upstreams)
        env["CLUD_TEST_UNIFIED_KIMI_UPSTREAM_URL"] = kimi.base_url
        assert _status(clud_binary, env)["kimi"] == "configured"

        result, probe = _launch_unified(
            clud_binary,
            env,
            tmp_path / "kimi.json",
            "--mock-acceptance-extra-turn",
            "clud-claude-kimi-k3",
        )
        assert probe["error"] is None, probe
        rows = {row["id"]: row["display_name"] for row in probe["models"]}
        assert rows.get("clud-claude-kimi-k3") == "Kimi K3", rows
        assert probe["turn_statuses"] == [200, 200, 200, 200, 200]
        assert "Kimi models unavailable" not in result.stderr

        assert len(kimi.requests) == 1
        head, body = _split(kimi.requests[0])
        assert body["model"] == "kimi-k3[1m]"
        assert _header(head, b"authorization") == f"Bearer {KIMI_CANARY}".encode()
        assert _header(head, b"x-clud-gateway-token") is None
        for canary in (DEEPSEEK_CANARY, OPENROUTER_CANARY, AMBIENT_CANARY):
            assert canary.encode() not in kimi.requests[0], canary
        responses, anthropic = upstreams
        for raw in list(responses.requests) + [
            raw for server in anthropic.values() for raw in server.requests
        ]:
            assert KIMI_CANARY.encode() not in raw
        assert KIMI_CANARY not in result.stdout + result.stderr

        assert _auth(clud_binary, env, "logout", "kimi") == {"removed": True}
        result, after = _launch_unified(clud_binary, env, tmp_path / "after.json")
        assert "clud-claude-kimi-k3" not in _row_ids(after)
        assert "clud auth login kimi" in result.stderr
        assert {row for row in _row_ids(after) if "deepseek" in row}, "DeepSeek kept"
    finally:
        kimi.close()
