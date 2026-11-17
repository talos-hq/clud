"""Developer ID signing + notarization of the macOS release wheels (ci/sign_macos_release.py)."""

from __future__ import annotations

import base64
import json
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
}
NOTARY = {
    "MACOS_SIGNING_IDENTITY": IDENTITY,
    "APPLE_NOTARY_USER": "release@example.com",
    "APPLE_NOTARY_PASSWORD": "abcd-efgh-ijkl-mnop",
    "APPLE_TEAM_ID": "ABCDE12345",
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
    for name in sign.CERT_SECRETS[1:]:
        assert name in message
    assert "MACOS_SIGNING_CERT," not in message
    assert "release-signing" in message
    # Signing needs only the certificate; notarization only the Apple ID.
    assert sign.config_from_env(SECRETS).identity == IDENTITY
    with pytest.raises(
        sign.SigningError, match="APPLE_NOTARY_USER, APPLE_NOTARY_PASSWORD, APPLE_TEAM_ID"
    ):
        sign.notary_config_from_env({"MACOS_SIGNING_IDENTITY": IDENTITY})
    with pytest.raises(sign.SigningError, match="MACOS_SIGNING_IDENTITY"):
        sign.notary_config_from_env({k: v for k, v in NOTARY.items() if k != "MACOS_SIGNING_IDENTITY"})


def test_config_validates_identity_and_certificate_encoding() -> None:
    config = sign.config_from_env(SECRETS)
    assert config.cert_p12 == b"p12-bytes"
    with pytest.raises(sign.SigningError, match="Developer ID Application"):
        sign.config_from_env({**SECRETS, "MACOS_SIGNING_IDENTITY": "Apple Development: x"})
    with pytest.raises(sign.SigningError, match="base64"):
        sign.config_from_env({**SECRETS, "MACOS_SIGNING_CERT": "not base64!"})
    with pytest.raises(sign.SigningError, match="MACOS_SIGNING_IDENTITY"):
        sign.identity_from_env({})


def test_notary_team_must_be_the_signing_team() -> None:
    config = sign.notary_config_from_env(NOTARY)
    assert (config.apple_id, config.team_id) == ("release@example.com", "ABCDE12345")
    with pytest.raises(sign.SigningError, match="APPLE_TEAM_ID does not match"):
        sign.notary_config_from_env({**NOTARY, "APPLE_TEAM_ID": "ZZZZZ99999"})


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


def test_sign_command_keeps_secrets_off_argv() -> None:
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


def test_notarytool_commands() -> None:
    config = sign.notary_config_from_env(NOTARY)
    submit = sign.notary_submit_command(config, Path("x.zip"))
    assert submit[:4] == ["xcrun", "notarytool", "submit", "x.zip"]
    for flag, value in (
        ("--apple-id", "release@example.com"),
        ("--team-id", "ABCDE12345"),
        ("--timeout", sign.NOTARY_TIMEOUT),
        ("--output-format", "json"),
    ):
        assert submit[submit.index(flag) + 1] == value
    assert "--wait" in submit
    log = sign.notary_log_command(config, "sub-1")
    assert log[:4] == ["xcrun", "notarytool", "log", "sub-1"]
    assert log[log.index("--team-id") + 1] == "ABCDE12345"
    assert sign.ditto_command(Path("d"), Path("u.zip")) == [
        "ditto", "-c", "-k", "--keepParent", "d", "u.zip",
    ]


def test_notary_result_parses_the_final_json_record() -> None:
    record = {"id": "sub-1", "status": "Accepted", "message": "Processing complete"}
    assert sign.notary_result(json.dumps(record))["status"] == "Accepted"
    noisy = "Conducting pre-submission checks...\n" + json.dumps(record) + "\n"
    assert sign.notary_result(noisy)["id"] == "sub-1"
    with pytest.raises(sign.SigningError, match="no JSON"):
        sign.notary_result("Error: HTTP status code: 401")


def _signed_wheel(tmp_path: Path) -> Path:
    good = _macho(_superblob(sign.CS_RUNTIME, _good_cms()))
    return _wheel(
        tmp_path,
        {"clud-2.8.66.data/scripts/clud": good, "clud-2.8.66.data/scripts/clud-webterm": good},
    )


def _fake_notary(status: str, code: int = 0):
    calls: list[list[str]] = []

    def run(command: list[str]) -> tuple[int, str, str]:
        calls.append(command)
        if command[0] == "ditto":
            staged = Path(command[-2])
            assert sorted(p.name for p in staged.rglob("*") if p.is_file()) == [
                "clud",
                "clud-webterm",
            ]
            assert all(p.stat().st_mode & 0o111 for p in staged.rglob("*") if p.is_file())
            Path(command[-1]).write_bytes(b"zip")
            return 0, "", ""
        if command[2] == "submit":
            record = {"id": "sub-1", "status": status, "message": "done"}
            return code, json.dumps(record), "Conducting pre-submission checks..."
        return 0, '{"issues": [{"message": "The signature does not include a secure timestamp."}]}', ""

    return run, calls


def test_notarize_accepts_and_leaves_the_wheel_untouched(tmp_path: Path) -> None:
    wheel = _signed_wheel(tmp_path)
    before = wheel.read_bytes()
    run, calls = _fake_notary("Accepted")
    accepted = sign.notarize_dist(tmp_path, sign.notary_config_from_env(NOTARY), run)
    assert accepted == {wheel.name: "sub-1"}
    assert [c[0] if c[0] == "ditto" else c[2] for c in calls] == ["ditto", "submit"]
    assert wheel.read_bytes() == before


@pytest.mark.parametrize(("status", "code"), [("Invalid", 0), ("Rejected", 0), ("Accepted", 1)])
def test_notarize_fails_on_anything_but_accepted_and_prints_the_log(
    tmp_path: Path, status: str, code: int, capsys: pytest.CaptureFixture[str]
) -> None:
    _signed_wheel(tmp_path)
    run, calls = _fake_notary(status, code)
    with pytest.raises(sign.SigningError, match=f"not accepted: submission sub-1 status {status}"):
        sign.notarize_dist(tmp_path, sign.notary_config_from_env(NOTARY), run)
    assert calls[-1][:4] == ["xcrun", "notarytool", "log", "sub-1"]
    out = capsys.readouterr().out
    assert "secure timestamp" in out
    assert NOTARY["APPLE_NOTARY_PASSWORD"] not in out


def test_notarize_refuses_an_unsigned_wheel_before_submitting(tmp_path: Path) -> None:
    _wheel(tmp_path, {"clud-2.8.66.data/scripts/clud": _macho()})
    run, calls = _fake_notary("Accepted")
    with pytest.raises(sign.SigningError, match="unsigned macOS release binaries"):
        sign.notarize_dist(tmp_path, sign.notary_config_from_env(NOTARY), run)
    assert calls == []


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
    assert "runs-on: ubuntu-24.04" in signer
    assert "needs: [preflight, build]" in signer
    assert f"artifact: [{', '.join(darwin)}]" in signer
    assert "python -m ci.sign_macos_release sign --dist-dir dist" in signer
    assert "overwrite: true" in signer
    for name in sign.CERT_SECRETS:
        assert f"{name}: ${{{{ secrets.{name} }}}}" in signer
    notary = _job(release, "notarize-macos")
    assert "environment: release-signing" in notary
    assert "runs-on: macos-15" in notary
    assert "needs: [preflight, sign-macos]" in notary
    assert f"artifact: [{', '.join(darwin)}]" in notary
    assert "python -m ci.sign_macos_release notarize --dist-dir dist" in notary
    # It reads the signed artifact and never replaces it: published bytes are
    # exactly what sign-macos produced.
    assert "upload-artifact" not in notary
    for name in ("MACOS_SIGNING_IDENTITY", *sign.NOTARY_SECRETS):
        assert f"{name}: ${{{{ secrets.{name} }}}}" in notary
    assert "MACOS_SIGNING_CERT" not in notary
    for name in sign.NOTARY_SECRETS:
        assert name not in signer
    smoke = _job(release, "macos-signature-smoke")
    assert "needs: [preflight, notarize-macos]" in smoke
    assert sorted(re.findall(r"- artifact: (\S+)", smoke)) == darwin
    assert "python -m ci.sign_macos_release smoke" in smoke
    for name in (*sign.NOTARY_SECRETS, "MACOS_SIGNING_CERT"):
        assert name not in smoke
    assert "APPLE_API" not in release
    assert "needs: [preflight, release-matrix, build, sign-macos]" in release
    for job in ("publish-pypi", "publish-release"):
        body = release[release.index(f"  {job}:") :]
        needs = body.split("needs:", 1)[1].split("\n", 1)[0]
        condition = body.split("    if: >-", 1)[1].split("    runs-on:", 1)[0]
        for proof in ("sign-macos", "notarize-macos", "macos-signature-smoke"):
            assert proof in needs, job
            assert f"needs.{proof}.result == 'success'" in condition, job
    # Only the signing chain declares the secrets environment.
    holders = [
        name
        for name in re.findall(r"^  ([a-z0-9-]+):$", release.split("\njobs:\n", 1)[1], re.MULTILINE)
        if "environment: release-signing" in _job(release, name)
    ]
    assert holders == ["sign-macos", "notarize-macos", "macos-signature-smoke"]
    # The Rust build job runs third-party build scripts; it never sees Apple secrets.
    build = (WORKFLOWS / "_build-target.yml").read_text(encoding="utf-8")
    for name in (*sign.CERT_SECRETS, *sign.NOTARY_SECRETS):
        assert name not in build


# Jobs a notarize_only proof run executes. Every other job must be skipped.
PROOF_JOBS = {
    "preflight",
    "release-matrix",
    "build",
    "sign-macos",
    "notarize-macos",
    "macos-signature-smoke",
    "wheel-smoke",
}


def test_notarize_only_proof_run_publishes_nothing() -> None:
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    assert "notarize_only:\n" in release
    assert "type: boolean" in release.split("notarize_only:\n", 1)[1].split("\n\n", 1)[0] + "\n"
    jobs = re.findall(r"^  ([a-z0-9-]+):$", release.split("\njobs:\n", 1)[1], re.MULTILINE)
    assert set(PROOF_JOBS) <= set(jobs)
    for name in jobs:
        body = _job(release, name)
        if name in PROOF_JOBS:
            continue
        # A job runs in the proof run only when it opts out of the implicit
        # success() check (always()); every such job names the input. Jobs
        # without always() are skipped because the gate they need is skipped.
        if "always()" in body.split("    steps:", 1)[0]:
            assert "!inputs.notarize_only" in body, name
        else:
            assert re.search(r"needs: .*(full-ci-gate|build-static-musl)", body) or (
                "if: ${{ !inputs.notarize_only }}" in body
            ), name
    gate = _job(release, "full-ci-gate")
    assert "if: ${{ !inputs.notarize_only }}" in gate
    # The build may follow a skipped gate only in notarize_only mode.
    for name in ("release-matrix", "build"):
        assert (
            "needs.full-ci-gate.result == 'success' || "
            "(inputs.notarize_only && needs.full-ci-gate.result == 'skipped')"
        ) in _job(release, name)
    assert "--release ${{ inputs.notarize_only && '--macos-only' || '' }}" in release
    assert "clud-notarize-proof-" in release


def test_resume_requires_the_original_macos_signing_proof() -> None:
    assert release_resume.REQUIRED["Sign macOS ("] == 2
    assert release_resume.REQUIRED["Notarize macOS ("] == 2
    assert release_resume.REQUIRED["Verify macOS signature ("] == 2
