"""Exercise the actual archive formats that cross CI runner boundaries."""

import shutil
import stat
from pathlib import Path

import pytest

from ci import bundle


@pytest.mark.parametrize("mode", ["gzip", "zstd", "fallback"])
def test_bundle_archive_round_trip(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, mode: str
) -> None:
    if mode == "zstd" and not shutil.which("zstd"):
        pytest.skip("zstd is not installed")
    if mode == "fallback":
        monkeypatch.setattr(bundle.shutil, "which", lambda _cmd: None)
    staging = tmp_path / "staging"
    staging.mkdir()
    (staging / "manifest.json").write_text(
        '{"target":"x86_64-unknown-linux-gnu","harnesses":[]}', encoding="utf-8"
    )
    executable = staging / "clud"
    executable.write_bytes(b"some executable bytes")
    executable.chmod(0o755)
    archives = tmp_path / "archives"
    archives.mkdir()
    archive = bundle._write_archive(staging, archives, prefer_zstd=mode != "gzip")
    expected_name = bundle.ZSTD_ARCHIVE_NAME if mode == "zstd" else bundle.ARCHIVE_NAME
    assert archive.name == expected_name

    extracted = tmp_path / "extracted"
    assert bundle.unpack(archives, extracted) == 0
    result = extracted / executable.name
    assert result.read_bytes() == executable.read_bytes()
    assert stat.S_IMODE(result.stat().st_mode) == stat.S_IMODE(executable.stat().st_mode)
