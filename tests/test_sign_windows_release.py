"""Authenticode signing of the Windows release wheels (ci/sign_windows_release.py)."""

from __future__ import annotations

import stat
import struct
import zipfile
from pathlib import Path

import pytest

from ci import sign_windows_release as sign
from ci.wheel_rewrite import record_line

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
PUBLISHER = "Talos HQ"


def _pe(cert: bytes = b"") -> bytes:
    """A minimal PE32+ image; ``cert`` (if any) is the WIN_CERTIFICATE table."""
    pe_offset = 0x40
    optional = pe_offset + 24
    size = optional + 112 + 16 * 8
    image = bytearray(size)
    image[:2] = b"MZ"
    struct.pack_into("<I", image, 0x3C, pe_offset)
    image[pe_offset : pe_offset + 4] = b"PE\0\0"
    struct.pack_into("<H", image, optional, 0x20B)
    if cert:
        struct.pack_into("<II", image, optional + 112 + 4 * 8, size, len(cert))
        image += cert
    return bytes(image)


def _cert(publisher: str = PUBLISHER, timestamp: bool = True) -> bytes:
    body = publisher.encode() + (sign.RFC3161_TIMESTAMP_OID if timestamp else b"")
    return struct.pack("<IHH", 8 + len(body), 0x0200, 0x0002) + body


def _wheel(path: Path, members: dict[str, bytes]) -> Path:
    wheel = path / "clud-2.8.50-py3-none-win_amd64.whl"
    with zipfile.ZipFile(wheel, "w") as archive:  # wheel-write-lint: allow (test fixture)
        members = {**members, "clud-2.8.50.dist-info/WHEEL": b"Wheel-Version: 1.0\n"}
        for name, data in members.items():
            info = zipfile.ZipInfo(name)
            info.external_attr = (stat.S_IFREG | 0o755) << 16
            archive.writestr(info, data)
        archive.writestr("clud-2.8.50.dist-info/RECORD", "stale")
    return wheel


def _fake_signer(files: list[Path]) -> None:
    for path in files:
        path.write_bytes(_pe(_cert()))


def test_signs_only_unsigned_pes_and_rewrites_record(tmp_path: Path) -> None:
    vendor = _pe(_cert("Microsoft Corporation"))
    wheel = _wheel(
        tmp_path,
        {
            "clud-2.8.50.data/scripts/clud.exe": _pe(),
            "clud-2.8.50.data/data/clud-kittyterm/wezterm.exe": _pe(),
            "clud-2.8.50.data/data/clud-kittyterm/conpty.dll": vendor,
        },
    )
    signed = sign.sign_wheel(wheel, _fake_signer)
    assert signed == [
        "clud-2.8.50.data/data/clud-kittyterm/wezterm.exe",
        "clud-2.8.50.data/scripts/clud.exe",
    ]
    assert sign.verify_wheel(wheel, PUBLISHER, signed) == []
    with zipfile.ZipFile(wheel) as archive:
        assert archive.read("clud-2.8.50.data/data/clud-kittyterm/conpty.dll") == vendor
        record = archive.read("clud-2.8.50.dist-info/RECORD").decode()
        exe = archive.read("clud-2.8.50.data/scripts/clud.exe")
        mode = archive.getinfo("clud-2.8.50.data/scripts/clud.exe").external_attr >> 16
    assert record_line("clud-2.8.50.data/scripts/clud.exe", exe) in record
    assert mode & 0o777 == 0o755


def test_verify_fails_closed_on_unsigned_or_untimestamped(tmp_path: Path) -> None:
    wheel = _wheel(
        tmp_path,
        {
            "clud-2.8.50.data/scripts/clud.exe": _pe(_cert(timestamp=False)),
            "clud-2.8.50.data/scripts/clud-webterm.exe": _pe(),
        },
    )
    failures = sign.verify_wheel(wheel, PUBLISHER)
    assert len(failures) == 2
    assert any("RFC 3161" in f for f in failures)
    assert any("no Authenticode" in f for f in failures)
    with pytest.raises(sign.SigningError):
        sign.verify_dist(tmp_path, PUBLISHER)


def test_verify_requires_publisher_on_clud_binaries(tmp_path: Path) -> None:
    _wheel(tmp_path, {"clud-2.8.50.data/scripts/clud.exe": _pe(_cert("Someone Else"))})
    with pytest.raises(sign.SigningError, match="publisher"):
        sign.verify_dist(tmp_path, PUBLISHER)


def test_trusted_ref_and_config() -> None:
    sign.require_trusted_ref({"GITHUB_REF": "refs/tags/2.8.51"})
    sign.require_trusted_ref({"GITHUB_REF": "refs/heads/main"})
    with pytest.raises(sign.SigningError):
        sign.require_trusted_ref({"GITHUB_REF": "refs/heads/release-candidate"})
    config = sign.config_from_env(
        {
            "AZURE_SIGNING_ENDPOINT": "https://eus.codesigning.azure.net/",
            "AZURE_SIGNING_ACCOUNT": "acct",
            "AZURE_SIGNING_PROFILE": "prof",
            "AZURE_SIGN_PUBLISHER": "CN=Talos HQ, O=Talos HQ",
        }
    )
    assert config.endpoint_host == "eus.codesigning.azure.net"
    assert config.publisher == PUBLISHER
    with pytest.raises(sign.SigningError):
        sign.config_from_env({})


def test_jsign_command_keeps_token_off_argv() -> None:
    config = sign.SigningConfig("eus.codesigning.azure.net", "acct", "prof", PUBLISHER)
    command = sign.jsign_command(Path("jsign.jar"), config, [Path("clud.exe")])
    assert f"env:{sign.TOKEN_ENV}" in command
    for flag, value in (
        ("--storetype", "TRUSTEDSIGNING"),
        ("--alias", "acct/prof"),
        ("--alg", "SHA-256"),
        ("--tsaurl", "http://timestamp.acs.microsoft.com"),
        ("--tsmode", "RFC3161"),
    ):
        assert command[command.index(flag) + 1] == value


def test_release_workflow_signs_windows_wheels_before_publish() -> None:
    build = (WORKFLOWS / "_build-target.yml").read_text(encoding="utf-8")
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    assert "azure/login@a457da9ea143d694b1b9c7c869ebb04ebe844ef5" in build
    assert "client-secret" not in build
    sign_step = build.index("ci.sign_windows_release sign")
    assert build.index("name: Build wheel") < sign_step < build.index("name: Upload dist artifacts")
    assert "github.event_name != 'pull_request'" in build
    assert "sign-windows: ${{ contains(matrix.target, 'windows') }}" in release
    assert "id-token: write" in release
    for job in ("publish-pypi", "publish-release"):
        needs = release[release.index(f"  {job}:") :].split("needs:", 1)[1].split("\n", 1)[0]
        assert "windows-signature-smoke" in needs, job
