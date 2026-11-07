"""voice/transcribe.py parses real voxtype output and refuses bad input."""

import importlib.util
import os
import sys
import wave
from pathlib import Path

import pytest

TOOL = Path(__file__).resolve().parents[1] / "crates/clud-bin/assets/tools/voice/transcribe.py"

REAL_STDOUT = (
    'Loading audio file: "speech_hello.wav"\n'
    "Audio format: 16000 Hz, 1 channel(s), Int\n"
    "Processing 16454 samples (1.03s)...\n"
    "\x1b[2m2026-11-07T01:53:29.793888Z\x1b[0m \x1b[32m INFO\x1b[0m Using Energy VAD backend\n"
    "VAD: 0.74s speech (71.2% of audio)\n"
    '\x1b[2m2026-11-07T01:53:34.517530Z\x1b[0m INFO completed in 0.51s: "Hello world."\n'
    "\n"
    "Hello world.\n"
)


@pytest.fixture
def tool():
    spec = importlib.util.spec_from_file_location("voice_transcribe", TOOL)
    assert spec
    assert spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _wav(path: Path, rate: int = 16000, channels: int = 1) -> Path:
    with wave.open(str(path), "wb") as writer:
        writer.setnchannels(channels)
        writer.setsampwidth(2)
        writer.setframerate(rate)
        writer.writeframes(b"\0\0" * channels * 160)
    return path


def _fake(tmp_path: Path, monkeypatch, body: str) -> None:
    script = tmp_path / "fake-transcriber"
    script.write_text(f"#!/bin/sh\n{body}\n")
    script.chmod(0o755)
    monkeypatch.setenv("CLUD_VOICE_TRANSCRIBER", str(script))


def test_parse_real_format(tool):
    assert tool.parse_transcript(REAL_STDOUT) == "Hello world."
    assert tool.parse_transcript("VAD: 0s\nNo speech detected, skipping.\n") is None
    assert tool.parse_transcript("\x1b[2m2026-11-07T01:53:29.1Z\x1b[0m INFO x\n") is None


def test_input_validation(tool, tmp_path):
    assert tool.input_problem(_wav(tmp_path / "ok.wav")) is None
    assert "44100" in tool.input_problem(_wav(tmp_path / "a.wav", rate=44100))
    assert "2 channel" in tool.input_problem(_wav(tmp_path / "b.wav", channels=2))
    assert "no such file" in tool.input_problem(tmp_path / "missing.wav")
    junk = tmp_path / "junk.wav"
    junk.write_bytes(b"not audio")
    assert "not a readable" in tool.input_problem(junk)


def test_missing_binary(tool, tmp_path, monkeypatch, capsys):
    monkeypatch.setenv("CLUD_VOICE_TRANSCRIBER", str(tmp_path / "nope"))
    assert tool.main([str(_wav(tmp_path / "ok.wav"))]) == tool.EXIT_NOT_INSTALLED
    assert "CLUD_VOICE_TRANSCRIBER" in capsys.readouterr().err


@pytest.mark.skipif(sys.platform == "win32" or os.name == "nt", reason="fake is a shell script")
class TestFakeTranscriber:
    def test_success(self, tool, tmp_path, monkeypatch, capsys):
        _fake(tmp_path, monkeypatch, "echo 'VAD: 1s'; echo; echo 'hello there'")
        assert tool.main([str(_wav(tmp_path / "ok.wav"))]) == tool.EXIT_OK
        assert capsys.readouterr().out == "hello there\n"

    def test_nonzero(self, tool, tmp_path, monkeypatch, capsys):
        _fake(tmp_path, monkeypatch, "echo 'model missing' >&2; exit 3")
        assert tool.main([str(_wav(tmp_path / "ok.wav"))]) == tool.EXIT_FAILED
        captured = capsys.readouterr()
        assert captured.out == ""
        assert "exit code 3" in captured.err
        assert "model missing" in captured.err

    def test_crash_discards_transcript(self, tool, tmp_path, monkeypatch, capsys):
        _fake(tmp_path, monkeypatch, "echo; echo 'hello there'; kill -ABRT $$")
        assert tool.main([str(_wav(tmp_path / "ok.wav"))]) == tool.EXIT_FAILED
        captured = capsys.readouterr()
        assert captured.out == ""
        assert "crashed" in captured.err
        assert "discarded" in captured.err

    def test_empty(self, tool, tmp_path, monkeypatch):
        _fake(tmp_path, monkeypatch, "echo 'VAD: 0s'")
        assert tool.main([str(_wav(tmp_path / "ok.wav"))]) == tool.EXIT_NO_SPEECH
