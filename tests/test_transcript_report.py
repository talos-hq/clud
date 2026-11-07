"""Tests for the bundled transcript analyzer (issue #1276).

The fixture is synthetic but shaped like the incident: two capped responses
(32,000 output tokens each, usage repeated on every row) that emit 294 and
371 copies of one Bash call, three compact_boundary rows and a terminal
context error.
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "crates" / "clud-bin" / "assets" / "tools" / "diagnostics" / "transcript_report.py"

SECRET_CMD_A = "cat /private/bench/SECRET_ARTIFACT_A.json | grep sk-or-v1-TOPSECRET"
SECRET_CMD_B = "rg --no-heading PRIVATE_SYMBOL_B /home/user/secret-project"
SECRET_OUTPUT = "RESULT-BODY-SHOULD-NOT-LEAK " * 60
SECRET_PROMPT = "USER-PROMPT-SHOULD-NOT-LEAK investigate mimalloc"
SESSION_ID = "622e60e1-5376-44bd-be2c-059a72786a16"


@pytest.fixture(autouse=True)
def _temp_state(tmp_path, monkeypatch):
    """Never read the real ~/.clud: point HOME and the state dir at a temp."""
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("USERPROFILE", str(home))
    monkeypatch.setenv("CLUD_DAEMON_STATE_DIR", str(tmp_path / "state"))


@pytest.fixture
def tr():
    name = "clud_test_transcript_report"
    spec = importlib.util.spec_from_file_location(name, SCRIPT)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    try:
        yield module
    finally:
        sys.modules.pop(name, None)


def _assistant(mid, ts, blocks, *, out, inp, cache=0, stop=None):
    return {
        "type": "assistant",
        "sessionId": SESSION_ID,
        "timestamp": ts,
        "message": {
            "id": mid,
            "role": "assistant",
            "content": blocks,
            "stop_reason": stop,
            "usage": {
                "input_tokens": inp,
                "cache_read_input_tokens": cache,
                "output_tokens": out,
            },
        },
    }


def _result(ts, text):
    return {
        "type": "user",
        "sessionId": SESSION_ID,
        "timestamp": ts,
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t", "content": text}],
        },
    }


def _compact(ts, pre):
    return {
        "type": "system",
        "subtype": "compact_boundary",
        "timestamp": ts,
        "content": "Conversation compacted",
        "compactMetadata": {"trigger": "auto", "preTokens": pre},
    }


def _burst(mid, ts, cmd, n):
    rows = []
    for i in range(n):
        block = {"type": "tool_use", "id": f"{mid}-{i}", "name": "Bash", "input": {"command": cmd}}
        rows.append(_assistant(mid, ts, [block], out=32_000, inp=150_000, stop="max_tokens"))
        rows.append(_result(ts, SECRET_OUTPUT))
    return rows


def _text(t):
    return {"type": "text", "text": t}


def incident_rows():
    rows = [
        {
            "type": "user",
            "timestamp": "2026-09-22T08:30:00Z",
            "message": {"role": "user", "content": SECRET_PROMPT},
        },
        _assistant("msg_small", "2026-09-22T08:30:05Z", [_text("ok")], out=40, inp=20_000),
        _compact("2026-09-22T08:59:24Z", 180_000),
        _assistant("msg_post1", "2026-09-22T09:00:00Z", [_text("x")], out=10, inp=30_000),
    ]
    rows += _burst("msg_burst_a", "2026-09-22T09:01:13Z", SECRET_CMD_A, 294)
    rows.append(_compact("2026-09-22T09:06:16Z", 190_000))
    rows += _burst("msg_burst_b", "2026-09-22T09:07:20Z", SECRET_CMD_B, 371)
    rows.append(_compact("2026-09-22T09:12:59Z", 195_000))
    rows.append(
        {
            "type": "system",
            "timestamp": "2026-09-22T09:13:22Z",
            "content": (
                "Autocompact is thrashing: the context refilled to the previous"
                " compact, 3 times in a row."
            ),
        }
    )
    rows.append(
        {
            "type": "system",
            "timestamp": "2026-09-22T09:13:22Z",
            "content": "Goal cleared after an unrecoverable error (context limit reached)",
        }
    )
    return rows


def test_incident_bursts_reported_with_tokens_counted_once(tr):
    report = tr.analyze(incident_rows())
    bursts = sorted(report["repeated_call_bursts"], key=lambda b: b["copies"])
    assert [b["copies"] for b in bursts] == [294, 371]
    assert all(b["tool"] == "Bash" for b in bursts)
    assert all(b["longest_streak"] == b["copies"] for b in bursts)
    assert all(b["output_tokens"] == 32_000 for b in bursts)
    assert report["responses"] == 4
    # 40 + 10 + 32,000 + 32,000: never 32,000 x 665.
    assert report["output_tokens_total"] == 64_050
    assert report["max_output_tokens_single_response"] == 32_000
    assert report["tool_calls_total"] == 665


def test_incident_compactions_errors_and_results(tr):
    report = tr.analyze(incident_rows())
    assert [c["timestamp"] for c in report["compactions"]] == [
        "2026-09-22T08:59:24Z",
        "2026-09-22T09:06:16Z",
        "2026-09-22T09:12:59Z",
    ]
    kinds = [e["kind"] for e in report["context_errors"]]
    assert kinds == ["autocompact_thrashing", "context_limit_reached"]
    assert report["tool_results"]["count"] == 665
    assert report["context_jumps"], "the 30k -> 150k refill must show as a jump"
    assert report["max_context_tokens"]["value"] is None
    assert "unavailable" in report["max_context_tokens"]["source"]


def _context_rows(contexts):
    """One assistant row per context size; `inp` is the whole context."""
    return [
        _assistant(f"msg_ctx_{i}", f"2026-09-22T09:{i:02d}:00Z", [_text("ok")], out=10, inp=ctx)
        for i, ctx in enumerate(contexts)
    ]


def test_sustained_context_charges_the_history_premium(tr):
    report = tr.analyze(_context_rows([20_000, 120_000, 200_000, 300_000]))
    s = report["sustained_context"]
    assert s["responses"] == 4
    assert s["responses_over_floor"] == 3
    assert s["premium_tokens"] == 20_000 + 100_000 + 200_000
    assert s["median_context_tokens"] == 120_000
    assert s["peak_context_tokens"] == 300_000


def test_sustained_context_floor_is_tunable(tr):
    rows = _context_rows([50_000, 60_000])
    assert tr.analyze(rows)["sustained_context"]["premium_tokens"] == 0
    lowered = tr.analyze(rows, context_floor=40_000)["sustained_context"]
    assert lowered["premium_tokens"] == 10_000 + 20_000
    assert lowered["floor_tokens"] == 40_000


def test_sustained_context_is_empty_for_a_short_cheap_session(tr):
    s = tr.analyze(_context_rows([12_000, 15_000]))["sustained_context"]
    assert s["premium_tokens"] == 0
    assert s["responses_over_floor"] == 0
    assert s["median_context_tokens"] == 12_000


def test_text_report_names_the_premium_and_the_compaction_lever(tr, tmp_path, capsys):
    path = tmp_path / "big.jsonl"
    path.write_text("\n".join(json.dumps(r) for r in _context_rows([250_000] * 60)))
    assert tr.main([str(path)]) == 0
    out = capsys.readouterr().out
    assert "sustained context: 60/60 responses" in out
    assert "premium 9.0M tokens" in out
    assert "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE" in out


def test_text_report_stays_quiet_below_the_premium_threshold(tr, tmp_path, capsys):
    path = tmp_path / "small.jsonl"
    path.write_text("\n".join(json.dumps(r) for r in _context_rows([11_000] * 3)))
    assert tr.main([str(path)]) == 0
    out = capsys.readouterr().out
    assert "sustained context: 0/3 responses" in out
    assert "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE" not in out


def test_small_normal_session_has_no_bursts(tr):
    rows = [
        _assistant(
            "m1",
            "t",
            [
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_use", "name": "Read", "input": {"file_path": "a"}},
            ],
            out=100,
            inp=1000,
        )
    ]
    report = tr.analyze(rows)
    assert report["repeated_call_bursts"] == []
    assert report["tool_calls_total"] == 3


def test_report_never_leaks_raw_content(tr, tmp_path, capsys):
    path = tmp_path / "session.jsonl"
    body = "\n".join(json.dumps(r) for r in incident_rows()) + "\nnot json\n"
    path.write_text(body, encoding="utf-8")
    before = sorted(p.name for p in tmp_path.iterdir())
    for argv in ([str(path)], [str(path), "--json"]):
        assert tr.main(argv) == 0
        out, err = capsys.readouterr()
        blob = out + err
        for secret in (
            "SECRET_ARTIFACT_A",
            "sk-or-v1",
            "PRIVATE_SYMBOL_B",
            "secret-project",
            "RESULT-BODY",
            "USER-PROMPT",
            SESSION_ID,
            "msg_burst_a",
            "thrashing: the context",
        ):
            assert secret not in blob, f"{secret!r} leaked into the report"
        assert "371" in blob
        assert "294" in blob
    assert sorted(p.name for p in tmp_path.iterdir()) == before, "the tool must write no files"


def test_fingerprints_are_salted_per_invocation(tr):
    a = tr.analyze(incident_rows(), salt=b"a" * 16)
    b = tr.analyze(incident_rows(), salt=b"b" * 16)
    fa = {x["call_fingerprint"] for x in a["repeated_call_bursts"]}
    fb = {x["call_fingerprint"] for x in b["repeated_call_bursts"]}
    assert len(fa) == 2
    assert fa.isdisjoint(fb)


def test_missing_file_exits_one(tr, tmp_path, capsys):
    assert tr.main([str(tmp_path / "nope.jsonl")]) == 1
    assert "cannot read transcript" in capsys.readouterr().err


# --- launch-context join (#1675) -------------------------------------------

# The writer's documented vector (crates/clud-bin/src/launch_context_tests.rs
# ::session_hash_is_the_documented_vector asserts the same literal).
WRITER_KEY = "eaf004849717de56"


def _record(**overrides):
    record = {
        "v": 1,
        "session": WRITER_KEY,
        "launched_at": 1,
        "clud_version": "2.0.0",
        "harness": "claude",
        "harness_version": None,
        "route": "direct",
        "provider": "openrouter",
        "wire_model": "xiaomi/mimo-v2.6-flash",
        "max_context_tokens": {"value": 1048576, "source": "served"},
        "auto_compact_window": {"value": None, "source": "unset"},
        "autocompact_pct_override": None,
    }
    record.update(overrides)
    return record


def _write_record(state, record):
    d = state / "launch-context"
    d.mkdir(parents=True, exist_ok=True)
    (d / f"{WRITER_KEY}.json").write_text(json.dumps(record), encoding="utf-8")


def _transcript(tmp_path):
    path = tmp_path / "s.jsonl"
    path.write_text("\n".join(json.dumps(r) for r in incident_rows()), encoding="utf-8")
    return path


def test_join_key_matches_the_writer(tr):
    assert tr.launch_context_key(SESSION_ID) == WRITER_KEY


def test_record_value_and_source_replace_unavailable(tr, tmp_path, capsys):
    state = tmp_path / "state"
    _write_record(state, _record())
    report = tr.analyze(incident_rows(), state_dir=state)
    assert report["max_context_tokens"] == {"value": 1048576, "source": "served"}
    assert report["launch_context"]["route"] == "direct"
    assert report["launch_context"]["auto_compact_window"]["source"] == "unset"
    path = _transcript(tmp_path)
    before = sorted(str(p) for p in tmp_path.rglob("*"))
    assert tr.main([str(path), "--state-dir", str(state)]) == 0
    out = capsys.readouterr().out
    assert "max context: 1048576 (source: served)" in out
    assert "route: direct" in out
    assert SESSION_ID not in out
    assert WRITER_KEY not in out
    assert sorted(str(p) for p in tmp_path.rglob("*")) == before, "the tool must write no files"


def test_default_state_dir_comes_from_the_env(tr, tmp_path, capsys):
    _write_record(tmp_path / "state", _record())
    assert tr.main([str(_transcript(tmp_path))]) == 0
    assert "max context: 1048576 (source: served)" in capsys.readouterr().out


def test_missing_record_still_unavailable(tr, tmp_path):
    report = tr.analyze(incident_rows(), state_dir=tmp_path / "state")
    assert report["max_context_tokens"]["value"] is None
    assert "unavailable" in report["max_context_tokens"]["source"]
    assert report["launch_context"] is None


def test_malformed_record_is_ignored(tr, tmp_path):
    state = tmp_path / "state"
    d = state / "launch-context"
    d.mkdir(parents=True)
    (d / f"{WRITER_KEY}.json").write_text("{not json", encoding="utf-8")
    report = tr.analyze(incident_rows(), state_dir=state)
    assert "unavailable" in report["max_context_tokens"]["source"]
    _write_record(state, _record(max_context_tokens={"value": "x", "source": "evil text"}))
    report = tr.analyze(incident_rows(), state_dir=state)
    assert "unavailable" in report["max_context_tokens"]["source"]


def test_record_fields_are_allowlisted(tr, tmp_path):
    state = tmp_path / "state"
    _write_record(state, _record(extra="SECRET-EXTRA-FIELD"))
    report = tr.analyze(incident_rows(), state_dir=state)
    assert "SECRET-EXTRA-FIELD" not in json.dumps(report)


def test_token_fields_stay_transcript_derived(tr, tmp_path):
    """A record never changes per-response token or compaction numbers."""
    state = tmp_path / "state"
    without = tr.analyze(incident_rows(), state_dir=state, salt=b"s" * 16)
    _write_record(state, _record())
    with_record = tr.analyze(incident_rows(), state_dir=state, salt=b"s" * 16)
    for key in ("per_response", "compactions", "output_tokens_total", "context_jumps"):
        assert with_record[key] == without[key]
