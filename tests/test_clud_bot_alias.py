"""The fixed-host remote-connect alias owns its help at the process boundary."""

import os
from pathlib import Path

from tests import process


def test_clud_bot_help() -> None:
    binary = os.environ.get("CLUD_TEST_BINARY") or str(
        Path(__file__).resolve().parents[1] / "target" / "debug" / "clud"
    )
    result = process.run(
        [binary, "clud.bot", "--help"], capture_output=True, text=True, timeout=30
    )
    assert result.returncode == 0, result.stderr
    assert "clud.bot" in result.stdout
    assert "consent" in result.stdout
    assert "Usage:" in result.stdout
