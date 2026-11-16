"""Developer ID signing + notarization of the macOS release wheels (ci/sign_macos_release.py)."""

from __future__ import annotations

import base64
import re
import stat
import struct
import zipfile
from pathlib import Path

import pytest

from ci import ci_matrix, release_resume
from ci import sign_macos_release as sign
from ci.wheel_rewrite import record_line

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
IDENTITY = "Developer ID Application: Example Corp (ABCDE12345)"
SECRETS = {
    "MACOS_SIGNING_CERT": base64.b64encode(b"p12-bytes").decode(),
    "MACOS_SIGNING_CERT_PASSWORD": "pw",
    "MACOS_SIGNING_IDENTITY": IDENTITY,
    "APPLE_API_KEY_P8": "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----",
    "APPLE_API_KEY_ID": "KEYID12345",
    "APPLE_API_ISSUER": "00000000-0000-0000-0000-000000000000",
}


def _blob(magic: int, body: bytes) -> bytes:
    return struct.pack(">II", magic, 8 + len(body)) + body


def _superblob(flags: int, cms: bytes | None) -> bytes:
    directory = _blob(sign.CSMAGIC_CODEDIRECTORY, struct.pack(">II", 0x20500, flags) + bytes(28))
    blobs = [(sign.CSSLOT_CODEDIRECTORY, directory)]
    if cms is not None:
        blobs.append((sign.CSSLOT_SIGNATURESLOT, _blob(0xFADE0B01, cms)))
    offset = 12 + 8 * len(blobs)
    index, payload = b"", b""
    for slot, blob in blobs:
        index += struct.pack(">II", slot, offset + len(payload))
        payload += blob
    body = index + payload
    return struct.pack(">III", sign.CSMAGIC_EMBEDDED_SIGNATURE, 12 + len(body), len(blobs)) + body


def _macho(signature: bytes | None = None) -> bytes:
    """A thin arm64 Mach-O with at most one load command, LC_CODE_SIGNATURE."""
    ncmds = 1 if signature is not None else 0
    header = sign.MH_MAGIC_64 + struct.pack("<IIIIIII", 0x0100000C, 0, 2, ncmds, 16 * ncmds, 0, 0)
    if signature is None:
        return header + b"\0" * 64
    dataoff = len(header) + 16
    command = struct.pack("<IIII", sign.LC_CODE_SIGNATURE, 16, dataoff, len(signature))
    return header + command + signature


def _good_cms(identity: str = IDENTITY, timestamp: bool = True) -> bytes:
    return b"cms" + identity.encode() + (sign.TIMESTAMP_TOKEN_OID if timestamp else b"")


def _wheel(path: Path, members: dict[str, bytes]) -> Path:
    wheel = path / "clud-2.8.66-py3-none-macosx_11_0_arm64.whl"
    with zipfile.ZipFile(wheel, "w") as archive:  # wheel-write-lint: allow (test fixture)
        members = {**members, "clud-2.8.66.dist-info/WHEEL": b"Wheel-Version: 1.0\n"}
        for name, data in members.items():
            info = zipfile.ZipInfo(name)
            info.external_attr = (stat.S_IFREG | 0o755) << 16
            archive.writestr(info, data)
        archive.writestr("clud-2.8.66.dist-info/RECORD", "stale")
    return wheel


def test_missing_secrets_fail_closed_naming_every_one() -> None:
    with pytest.raises(sign.SigningError) as error:
        sign.config_from_env({"MACOS_SIGNING_CERT": SECRETS["MACOS_SIGNING_CERT"]})
    message = str(error.value)
    for name in (*sign.CERT_SECRETS[1:], *sign.NOTARY_SECRETS):
        assert name in message
    assert "MACOS_SIGNING_CERT," not in message
    assert "release-signing" in message
    partial = {k: v for k, v in SECRETS.items() if not k.startswith("APPLE_API")}
    with pytest.raises(
        sign.SigningError, match="APPLE_API_KEY_P8, APPLE_API_KEY_ID, APPLE_API_ISSUER"
    ):
        sign.config_from_env(partial)


def test_config_validates_identity_and_certificate_encoding() -> None:
    config = sign.config_from_env(SECRETS)
    assert config.cert_p12 == b"p12-bytes"
    assert config.api_key_p8.endswith("-----END PRIVATE KEY-----\n")
    with pytest.raises(sign.SigningError, match="Developer ID Application"):
        sign.config_from_env({**SECRETS, "MACOS_SIGNING_IDENTITY": "Apple Development: x"})
    with pytest.raises(sign.SigningError, match="base64"):
        sign.config_from_env({**SECRETS, "MACOS_SIGNING_CERT": "not base64!"})
    with pytest.raises(sign.SigningError, match="MACOS_SIGNING_IDENTITY"):
        sign.identity_from_env({})


@pytest.mark.parametrize(
    ("binary", "problem"),
    [
        (_macho(), "no code signature"),
        (_macho(_superblob(sign.CS_RUNTIME, None)), "ad-hoc"),
        (_macho(_superblob(sign.CS_RUNTIME, b"")), "ad-hoc"),
        (_macho(_superblob(0x20002, _good_cms())), "hardened runtime"),
        (_macho(_superblob(sign.CS_RUNTIME, _good_cms("Someone Else"))), "does not name"),
        (_macho(_superblob(sign.CS_RUNTIME, _good_cms(timestamp=False))), "timestamp"),
        (_macho(_superblob(sign.CS_RUNTIME, _good_cms())), None),
    ],
)
def test_structural_signature_check(binary: bytes, problem: str | None) -> None:
    found = sign.signature_problem(binary, IDENTITY)
    if problem is None:
        assert found is None
    else:
        assert found is not None
        assert problem in found


def test_fat_binaries_are_refused() -> None:
    with pytest.raises(sign.SigningError, match="thin 64-bit"):
        sign.signature_problem(b"\xca\xfe\xba\xbe" + bytes(60), IDENTITY)


def test_signs_every_macho_and_rewrites_record(tmp_path: Path) -> None:
    wheel = _wheel(
        tmp_path,
        {
            "clud-2.8.66.data/scripts/clud": _macho(_superblob(0x20002, None)),
            "clud-2.8.66.data/scripts/clud-webterm": _macho(),
            "clud/__init__.py": b"",
        },
    )
    calls = []

    def fake_sign(label: str, files: list[Path]) -> None:
        calls.append((label, [path.name for path in files]))
        for path in files:
            path.write_bytes(_macho(_superblob(sign.CS_RUNTIME, _good_cms())))

    signed = sign.sign_wheel(wheel, fake_sign)
    assert signed == ["clud-2.8.66.data/scripts/clud", "clud-2.8.66.data/scripts/clud-webterm"]
    assert calls == [(wheel.stem, ["clud", "clud-webterm"])]
    assert sign.verify_wheel(wheel, IDENTITY) == []
    sign.verify_dist(tmp_path, IDENTITY)
    with zipfile.ZipFile(wheel) as archive:
        record = archive.read("clud-2.8.66.dist-info/RECORD").decode()
        clud = archive.read("clud-2.8.66.data/scripts/clud")
        mode = archive.getinfo("clud-2.8.66.data/scripts/clud").external_attr >> 16
        assert archive.read("clud/__init__.py") == b""
    assert record_line("clud-2.8.66.data/scripts/clud", clud) in record
    assert mode & 0o777 == 0o755


def test_verify_fails_closed_on_adhoc_wheel(tmp_path: Path) -> None:
    _wheel(tmp_path, {"clud-2.8.66.data/scripts/clud": _macho(_superblob(0x20002, None))})
    with pytest.raises(sign.SigningError, match="unsigned macOS release binaries"):
        sign.verify_dist(tmp_path, IDENTITY)
    with pytest.raises(sign.SigningError, match="no macOS wheels"):
        sign.verify_dist(tmp_path / "empty", IDENTITY)


def test_commands_keep_secrets_off_argv(tmp_path: Path) -> None:
    command = sign.sign_command(Path("rc"), Path("id.p12"), Path("pw"), Path("clud"))
    for flag, value in (
        ("--p12-file", "id.p12"),
        ("--p12-password-file", "pw"),
        ("--code-signature-flags", "runtime"),
        ("--timestamp-url", sign.TIMESTAMP_URL),
    ):
        assert command[command.index(flag) + 1] == value
    assert "--for-notarization" in command
    assert "--p12-password" not in command
    notary = sign.notary_command(Path("rc"), Path("key.json"), Path("x.zip"))
    assert notary[1] == "notary-submit"
    assert notary[notary.index("--api-key-file") + 1] == "key.json"
    assert "--wait" in notary


def test_notary_upload_is_a_zip_of_executables(tmp_path: Path) -> None:
    binary = tmp_path / "clud"
    binary.write_bytes(_macho())
    upload = sign.notary_zip([binary], tmp_path / "up.zip")
    with zipfile.ZipFile(upload) as archive:
        (info,) = archive.infolist()
        assert info.filename == "0/clud"
        assert (info.external_attr >> 16) & 0o777 == 0o755


def test_rcodesign_pin_is_a_linux_release_with_a_sha256() -> None:
    assert re.fullmatch(r"[0-9a-f]{64}", sign.RCODESIGN_SHA256)
    assert sign.RCODESIGN_ASSET.endswith("-x86_64-unknown-linux-musl")
    assert sign.RCODESIGN_URL.endswith(
        f"apple-codesign/{sign.RCODESIGN_VERSION}/{sign.RCODESIGN_ASSET}.tar.gz"
    )


def _job(text: str, name: str) -> str:
    body = text.split(f"\n  {name}:\n", 1)[1]
    return re.split(r"\n\n  (?=[a-z#])", body, maxsplit=1)[0]


def test_release_workflow_signs_and_notarizes_macos_wheels_before_publish() -> None:
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    darwin = sorted(t.artifact for t in ci_matrix.TARGETS if t.triple.endswith("-apple-darwin"))
    signer = _job(release, "sign-macos")
    assert "environment: release-signing" in signer
    assert "needs: [preflight, build]" in signer
    assert f"artifact: [{', '.join(darwin)}]" in signer
    assert "python -m ci.sign_macos_release sign --dist-dir dist" in signer
    assert "overwrite: true" in signer
    for name in (*sign.CERT_SECRETS, *sign.NOTARY_SECRETS):
        assert f"{name}: ${{{{ secrets.{name} }}}}" in signer
    smoke = _job(release, "macos-signature-smoke")
    assert "needs: [preflight, sign-macos]" in smoke
    assert sorted(re.findall(r"- artifact: (\S+)", smoke)) == darwin
    assert "python -m ci.sign_macos_release smoke" in smoke
    assert "APPLE_API_KEY_P8" not in smoke
    assert "needs: [preflight, release-matrix, build, sign-macos]" in release
    for job in ("publish-pypi", "publish-release"):
        body = release[release.index(f"  {job}:") :]
        needs = body.split("needs:", 1)[1].split("\n", 1)[0]
        assert "sign-macos" in needs, job
        assert "macos-signature-smoke" in needs, job
        condition = body.split("    if: >-", 1)[1].split("    runs-on:", 1)[0]
        assert "needs.sign-macos.result == 'success'" in condition, job
        assert "needs.macos-signature-smoke.result == 'success'" in condition, job
    # The Rust build job runs third-party build scripts; it never sees Apple secrets.
    build = (WORKFLOWS / "_build-target.yml").read_text(encoding="utf-8")
    for name in (*sign.CERT_SECRETS, *sign.NOTARY_SECRETS):
        assert name not in build


def test_resume_requires_the_original_macos_signing_proof() -> None:
    assert release_resume.REQUIRED["Sign and notarize macOS ("] == 2
    assert release_resume.REQUIRED["Verify macOS signature ("] == 2
