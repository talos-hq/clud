#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "running-process==4.10.1",
# ]
# ///
# managed-by: clud
"""transcribe.py — transcribe a 16 kHz mono WAV with talos-vox / voxtype.

Usage:
  clud tool run voice/transcribe.py -- <file.wav> [--language LANG]

Finds the binary the way F3 voice mode does: CLUD_VOICE_TRANSCRIBER, then
talos-vox, then voxtype on PATH. --language falls back to CLUD_VOICE_LANGUAGE.
Input is not resampled: convert it first, for example with
`ffmpeg -i in.mp3 -ar 16000 -ac 1 out.wav`.

Prints only the transcript on stdout. As in voice/transcriber.rs (a deliberate
copy: a bundled script cannot call into the Rust crate), a run that exits
nonzero yields no transcript, even if it printed one before crashing.

Exit codes:
  0  transcript printed
  1  usage error or unsupported input
  2  transcriber not installed
  3  transcriber failed, crashed or timed out
  4  transcriber succeeded but found no speech
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import sys
import wave
from pathlib import Path

from running_process import PIPE, RunningProcess, TimeoutExpired

EXIT_OK = 0
EXIT_INPUT = 1
EXIT_NOT_INSTALLED = 2
EXIT_FAILED = 3
EXIT_NO_SPEECH = 4

TIMEOUT = 300.0
TAIL_LINES = 5
REQUIRED_RATE = 16000

_ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
_TRACING = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:")
_STATUS = (
    "Loading audio file:",
    "Audio format:",
    "Resampling from ",
    "Processing ",
    "VAD: ",
    "No speech detected",
)


def input_problem(path: Path) -> str | None:
    """Why `path` is not a 16 kHz mono WAV, or None if it is."""
    if not path.is_file():
        return f"{path}: no such file"
    try:
        with wave.open(str(path), "rb") as reader:
            rate, channels = reader.getframerate(), reader.getnchannels()
    except (wave.Error, EOFError) as error:
        return f"{path}: not a readable PCM WAV ({error}); convert with ffmpeg -ar 16000 -ac 1"
    if rate != REQUIRED_RATE or channels != 1:
        return (
            f"{path}: is {rate} Hz with {channels} channel(s); need 16000 Hz mono. "
            "Convert with ffmpeg -ar 16000 -ac 1"
        )
    return None


def locate() -> tuple[str | None, str]:
    """Return (binary, error message)."""
    override = os.environ.get("CLUD_VOICE_TRANSCRIBER")
    if override:
        found = shutil.which(override)
        return found, f"CLUD_VOICE_TRANSCRIBER is set to {override} but no executable is there"
    found = shutil.which("talos-vox") or shutil.which("voxtype")
    return found, (
        "neither talos-vox nor voxtype is on PATH. talos-vox has no releases yet: build it from "
        "https://github.com/talos-hq/talos-vox, install voxtype from your package manager, or "
        "set CLUD_VOICE_TRANSCRIBER"
    )


def parse_transcript(stdout: str) -> str | None:
    """The last stdout line that is not a log line.

    Unlike the F3 path, this cannot require voxtype's blank separator line:
    RunningProcess.run drops empty lines from captured text. The result goes
    to the calling agent, not into a live prompt, so that is acceptable.
    """
    lines = [line.strip() for line in _ANSI.sub("", stdout).splitlines()]
    kept = [ln for ln in lines if ln and not (ln.startswith(_STATUS) or _TRACING.match(ln))]
    return kept[-1] if kept else None


def describe_failure(code: int, stdout: str, stderr: str) -> str:
    if code < 0 or 129 <= code <= 192:
        head = f"transcriber crashed (exit code {code})"
    else:
        head = f"transcriber failed with exit code {code}"
    if parse_transcript(stdout):
        head += "; it printed a transcript first, which was discarded"
    text = _ANSI.sub("", stderr if stderr.strip() else stdout)
    tail = [line for line in text.splitlines() if line.strip()][-TAIL_LINES:]
    return head + ("\n" + "\n".join(tail) if tail else "")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Transcribe a 16 kHz mono WAV.")
    parser.add_argument("file", type=Path)
    parser.add_argument("--language", default=os.environ.get("CLUD_VOICE_LANGUAGE"))
    args = parser.parse_args(argv)

    problem = input_problem(args.file)
    if problem:
        print(problem, file=sys.stderr)
        return EXIT_INPUT
    binary, not_found = locate()
    if not binary:
        print(not_found, file=sys.stderr)
        return EXIT_NOT_INSTALLED

    # --language is a top-level option; voxtype refuses it after `transcribe`.
    language = ["--language", args.language] if args.language else []
    try:
        res = RunningProcess.run(
            [binary, *language, "transcribe", str(args.file)],
            capture_output=True,
            text=True,
            stdout=PIPE,
            stderr=PIPE,
            timeout=TIMEOUT,
        )
    except (TimeoutError, TimeoutExpired):
        print(f"{binary} did not finish within {TIMEOUT:.0f}s", file=sys.stderr)
        return EXIT_FAILED
    except OSError as error:
        print(f"could not start {binary}: {error}", file=sys.stderr)
        return EXIT_FAILED

    stdout, stderr = res.stdout or "", res.stderr or ""
    if res.returncode != 0:
        print(describe_failure(res.returncode, stdout, stderr), file=sys.stderr)
        return EXIT_FAILED
    transcript = parse_transcript(stdout)
    if not transcript:
        print("no transcript: no speech was detected in the recording", file=sys.stderr)
        return EXIT_NO_SPEECH
    print(transcript)
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
