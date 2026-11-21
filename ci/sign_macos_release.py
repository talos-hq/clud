"""Developer ID sign the macOS release wheels with rcodesign; notarize with notarytool.

Both ``*-apple-darwin`` release lanes cross-build on Linux runners, so the
signer is rcodesign (apple-codesign, https://github.com/indygreg/apple-platform-rs),
a pure-Rust implementation of Apple code signing. Its Linux release tarball is
pinned by version and SHA-256 and refused on mismatch.

``sign --dist-dir dist`` runs in Auto Release's ``sign-macos`` job on Linux. It
re-signs every Mach-O inside each macOS wheel (``clud``, ``clud-webterm``; the
linker's ad-hoc signature is replaced) with the Developer ID Application
certificate, the hardened runtime and a secure timestamp. The wheel is
rewritten through ``ci.wheel_rewrite`` so RECORD matches the signed bytes, and
the job re-uploads it under the same artifact name. PyPI, the standalone macOS
assets ``installer.release_assets`` extracts from these wheels, SHA256SUMS,
the Pages catalog and the Homebrew formula therefore all carry signed bytes.
A structural post-check fails the step when any Mach-O lacks the identity,
the hardened-runtime flag or the timestamp.

``notarize --dist-dir dist`` runs in the ``notarize-macos`` job on a macOS
runner, because notarization authenticates with an Apple ID and app-specific
password, which only Apple's ``xcrun notarytool`` accepts (rcodesign's
``notary-submit`` takes App Store Connect API keys only). It re-checks the
signature structure, zips the signed Mach-O files with ``ditto`` (Apple
notarizes zips, never a bare Mach-O), submits with ``notarytool submit
--wait`` and fails unless the status is ``Accepted``, printing ``notarytool
log`` otherwise. It never modifies the wheel: notarization registers the
signed files' code-directory hashes with Apple, so the bytes signed on Linux
are the bytes that ship. A bare executable cannot carry a stapled ticket, so
Gatekeeper looks its ticket up online; ``ci.macos_package`` builds the
stapled universal ``Install clud.app`` and disk image from these bytes.

Missing secrets fail closed, naming every absent secret at once.

``smoke --dist-dir wheels`` runs on native macOS and requires
``codesign --verify --strict``, a Developer ID authority with the hardened
runtime and a timestamp, and ``spctl`` reporting ``Notarized Developer ID`` for
every Mach-O (an online ticket lookup), then executes ``clud --version``.

Usage (CI):
    python -m ci.sign_macos_release sign --dist-dir dist/                  # Linux
    python -m ci.sign_macos_release notarize --dist-dir dist/              # macOS runner
    python -m ci.sign_macos_release verify --dist-dir dist/
    python -m ci.sign_macos_release smoke --dist-dir wheels/ --version X   # macOS runner
"""

from __future__ import annotations

import argparse
import base64
import binascii
import json
import os
import re
import shutil
import struct
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path

from ci import process
from ci.sign_windows_release import sha256_file
from ci.wheel_rewrite import rewrite_wheel

RCODESIGN_VERSION = "0.29.0"
RCODESIGN_ASSET = f"apple-codesign-{RCODESIGN_VERSION}-x86_64-unknown-linux-musl"
RCODESIGN_URL = (
    "https://github.com/indygreg/apple-platform-rs/releases/download/"
    f"apple-codesign/{RCODESIGN_VERSION}/{RCODESIGN_ASSET}.tar.gz"
)
RCODESIGN_SHA256 = "dbe85cedd8ee4217b64e9a0e4c2aef92ab8bcaaa41f20bde99781ff02e600002"

TIMESTAMP_URL = "http://timestamp.apple.com/ts01"
NOTARY_TIMEOUT = "30m"
NOTARY_ACCEPTED = "Accepted"

ENVIRONMENT = "release-signing"
CERT_SECRETS = ("MACOS_SIGNING_CERT", "MACOS_SIGNING_CERT_PASSWORD", "MACOS_SIGNING_IDENTITY")
# Apple ID + app-specific password (appleid.apple.com), and the Developer team.
NOTARY_SECRETS = ("APPLE_NOTARY_USER", "APPLE_NOTARY_PASSWORD", "APPLE_TEAM_ID")
IDENTITY_RE = re.compile(r"^Developer ID Application: .+ \(([A-Z0-9]{10})\)$")

# Mach-O headers (little-endian thin images, either-endian fat images).
MACHO_MAGICS = frozenset(
    {b"\xce\xfa\xed\xfe", b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca"}
)
MH_MAGIC_64 = b"\xcf\xfa\xed\xfe"
MACH_HEADER_64_SIZE = 32
LC_CODE_SIGNATURE = 0x1D
# Code signing blobs are big-endian (xnu osfmk/kern/cs_blobs.h).
CSMAGIC_EMBEDDED_SIGNATURE = 0xFADE0CC0
CSMAGIC_CODEDIRECTORY = 0xFADE0C02
CSSLOT_CODEDIRECTORY = 0
CSSLOT_SIGNATURESLOT = 0x10000
CS_RUNTIME = 0x10000
EMPTY_BLOB_WRAPPER = 8
# DER of OID 1.2.840.113549.1.9.16.2.14 (id-smime-aa-timeStampToken).
TIMESTAMP_TOKEN_OID = bytes.fromhex("060b2a864886f70d010910020e")
NOTARIZED_SOURCE = "source=Notarized Developer ID"


class SigningError(RuntimeError):
    """A signing precondition or post-signing check failed."""


@dataclass(frozen=True)
class SigningConfig:
    """Developer ID certificate credentials, read from the job environment."""

    cert_p12: bytes
    cert_password: str
    identity: str


@dataclass(frozen=True)
class NotaryConfig:
    """Apple ID notarization credentials, read from the job environment."""

    apple_id: str
    password: str
    team_id: str
    identity: str


def missing_secrets(env: dict[str, str], names: tuple[str, ...]) -> list[str]:
    return [name for name in names if not env.get(name, "").strip()]


def require_secrets(env: dict[str, str], names: tuple[str, ...]) -> None:
    missing = missing_secrets(env, names)
    if missing:
        raise SigningError(
            f"missing macOS signing secret(s): {', '.join(missing)}. Set them in the "
            f"talos-hq/clud `{ENVIRONMENT}` environment (gh secret set NAME --env "
            f"{ENVIRONMENT} -R talos-hq/clud < file); see docs/architecture/ci.md "
            "#developer-id-signing-and-notarization-of-macos-release-wheels"
        )


def validated_identity(identity: str) -> str:
    identity = identity.strip()
    if not IDENTITY_RE.match(identity):
        raise SigningError(
            "MACOS_SIGNING_IDENTITY must be the certificate common name "
            "'Developer ID Application: <name> (<TEAMID>)'"
        )
    return identity


def config_from_env(env: dict[str, str]) -> SigningConfig:
    require_secrets(env, CERT_SECRETS)
    try:
        cert = base64.b64decode(env["MACOS_SIGNING_CERT"].strip(), validate=True)
    except (binascii.Error, ValueError) as exc:
        raise SigningError("MACOS_SIGNING_CERT is not valid base64 of a .p12") from exc
    return SigningConfig(
        cert_p12=cert,
        cert_password=env["MACOS_SIGNING_CERT_PASSWORD"],
        identity=validated_identity(env["MACOS_SIGNING_IDENTITY"]),
    )


def notary_config_from_env(env: dict[str, str]) -> NotaryConfig:
    require_secrets(env, ("MACOS_SIGNING_IDENTITY", *NOTARY_SECRETS))
    identity = validated_identity(env["MACOS_SIGNING_IDENTITY"])
    team_id = env["APPLE_TEAM_ID"].strip()
    match = IDENTITY_RE.match(identity)
    if match is None or match.group(1) != team_id:
        raise SigningError(
            "APPLE_TEAM_ID does not match the team in MACOS_SIGNING_IDENTITY; "
            "notarization must use the team that signed the binaries"
        )
    return NotaryConfig(
        apple_id=env["APPLE_NOTARY_USER"].strip(),
        password=env["APPLE_NOTARY_PASSWORD"].strip(),
        team_id=team_id,
        identity=identity,
    )


def identity_from_env(env: dict[str, str]) -> str:
    require_secrets(env, ("MACOS_SIGNING_IDENTITY",))
    return validated_identity(env["MACOS_SIGNING_IDENTITY"])


def fetch_rcodesign(dest_dir: Path) -> Path:
    """Download the pinned rcodesign tarball, verify its SHA-256, return the binary."""
    binary = dest_dir / "rcodesign"
    dest_dir.mkdir(parents=True, exist_ok=True)
    archive = dest_dir / f"{RCODESIGN_ASSET}.tar.gz"
    if not (archive.is_file() and sha256_file(archive) == RCODESIGN_SHA256):
        partial = archive.with_suffix(".partial")
        with (
            urllib.request.urlopen(RCODESIGN_URL, timeout=120) as response,
            partial.open("wb") as out,
        ):
            shutil.copyfileobj(response, out)
        actual = sha256_file(partial)
        if actual != RCODESIGN_SHA256:
            partial.unlink()
            raise SigningError(
                f"rcodesign {RCODESIGN_VERSION} sha256 mismatch: got {actual}, "
                f"pinned {RCODESIGN_SHA256}"
            )
        partial.replace(archive)
        print(f"rcodesign {RCODESIGN_VERSION}: sha256 verified ({RCODESIGN_SHA256})")
    with tarfile.open(archive, "r:gz") as tar:
        member = tar.extractfile(f"{RCODESIGN_ASSET}/rcodesign")
        if member is None:
            raise SigningError(f"rcodesign binary missing from {archive.name}")
        binary.write_bytes(member.read())
    binary.chmod(0o755)
    return binary


def sign_command(rcodesign: Path, p12: Path, password: Path, target: Path) -> list[str]:
    return [
        str(rcodesign),
        "sign",
        "--p12-file",
        str(p12),
        "--p12-password-file",
        str(password),
        "--code-signature-flags",
        "runtime",
        "--for-notarization",
        "--timestamp-url",
        TIMESTAMP_URL,
        str(target),
    ]


def notary_credentials(config: NotaryConfig) -> list[str]:
    """notarytool auth flags. The password is on argv (notarytool has no file or
    environment form) of a single-tenant ephemeral runner; Actions masks the
    value, and callers never print or raise with the command line."""
    return [
        "--apple-id",
        config.apple_id,
        "--password",
        config.password,
        "--team-id",
        config.team_id,
    ]


def notary_submit_command(config: NotaryConfig, upload: Path) -> list[str]:
    return [
        "xcrun",
        "notarytool",
        "submit",
        str(upload),
        *notary_credentials(config),
        "--wait",
        "--timeout",
        NOTARY_TIMEOUT,
        "--output-format",
        "json",
    ]


def notary_log_command(config: NotaryConfig, submission_id: str) -> list[str]:
    return ["xcrun", "notarytool", "log", submission_id, *notary_credentials(config)]


def ditto_command(source_dir: Path, upload: Path) -> list[str]:
    return ["ditto", "-c", "-k", "--keepParent", str(source_dir), str(upload)]


def notary_result(stdout: str) -> dict[str, str]:
    """The final submission record notarytool prints with ``--output-format json``."""
    for line in reversed([stdout, *stdout.splitlines()]):
        text = line.strip()
        if not text.startswith("{"):
            continue
        try:
            record = json.loads(text)
        except ValueError:
            continue
        if isinstance(record, dict) and "status" in record:
            return {key: str(value) for key, value in record.items()}
    raise SigningError("notarytool printed no JSON submission status")


def is_macho(data: bytes) -> bool:
    return data[:4] in MACHO_MAGICS


def macos_wheels(dist_dir: Path) -> list[Path]:
    wheels = sorted(dist_dir.glob("*-macosx_*.whl"))
    if not wheels:
        raise SigningError(f"no macOS wheels under {dist_dir}")
    return wheels


def wheel_machos(wheel: Path) -> dict[str, bytes]:
    with zipfile.ZipFile(wheel) as archive:
        members = {n: archive.read(n) for n in archive.namelist() if not n.endswith("/")}
    return {name: data for name, data in members.items() if is_macho(data)}


def code_signature(data: bytes) -> bytes | None:
    """The LC_CODE_SIGNATURE SuperBlob of a thin 64-bit Mach-O, or None."""
    if data[:4] != MH_MAGIC_64:
        raise SigningError("expected a thin 64-bit Mach-O (clud ships one triple per wheel)")
    (ncmds,) = struct.unpack_from("<I", data, 16)
    offset = MACH_HEADER_64_SIZE
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from("<II", data, offset)
        if cmd == LC_CODE_SIGNATURE:
            dataoff, datasize = struct.unpack_from("<II", data, offset + 8)
            return data[dataoff : dataoff + datasize]
        if cmdsize <= 0:
            raise SigningError("malformed Mach-O load command")
        offset += cmdsize
    return None


def signature_blobs(superblob: bytes) -> dict[int, bytes]:
    magic, _length, count = struct.unpack_from(">III", superblob, 0)
    if magic != CSMAGIC_EMBEDDED_SIGNATURE:
        raise SigningError(f"unexpected code signature magic {magic:#x}")
    blobs = {}
    for index in range(count):
        slot, offset = struct.unpack_from(">II", superblob, 12 + 8 * index)
        _magic, length = struct.unpack_from(">II", superblob, offset)
        blobs[slot] = superblob[offset : offset + length]
    return blobs


def signature_problem(data: bytes, identity: str) -> str | None:
    """Why ``data`` is not Developer ID signed with runtime + timestamp, or None."""
    superblob = code_signature(data)
    if superblob is None:
        return "no code signature (no LC_CODE_SIGNATURE)"
    blobs = signature_blobs(superblob)
    directory = blobs.get(CSSLOT_CODEDIRECTORY)
    if directory is None or struct.unpack_from(">I", directory, 0)[0] != CSMAGIC_CODEDIRECTORY:
        return "code signature has no CodeDirectory"
    (flags,) = struct.unpack_from(">I", directory, 12)
    if not flags & CS_RUNTIME:
        return "hardened runtime flag is not set"
    cms = blobs.get(CSSLOT_SIGNATURESLOT, b"")
    if len(cms) <= EMPTY_BLOB_WRAPPER:
        return "ad-hoc signature (no CMS signature)"
    if identity.encode("utf-8") not in cms:
        return f"signature does not name {identity!r}"
    if TIMESTAMP_TOKEN_OID not in cms:
        return "signature carries no secure timestamp"
    return None


def verify_wheel(wheel: Path, identity: str) -> list[str]:
    machos = wheel_machos(wheel)
    if not machos:
        return [f"  {wheel.name}: no Mach-O files"]
    failures = []
    for name, data in sorted(machos.items()):
        problem = signature_problem(data, identity)
        if problem:
            failures.append(f"  {wheel.name}:{name}: {problem}")
        else:
            print(f"signed: {wheel.name}:{name}")
    return failures


def sign_wheel(wheel: Path, sign_files) -> list[str]:
    """Sign every Mach-O in ``wheel`` via ``sign_files``; rewrite it."""
    machos = wheel_machos(wheel)
    if not machos:
        raise SigningError(f"no Mach-O files inside {wheel.name}")
    targets = sorted(machos)
    with tempfile.TemporaryDirectory(prefix="clud-macsign-") as scratch:
        paths: dict[str, Path] = {}
        for index, name in enumerate(targets):
            path = Path(scratch) / str(index) / Path(name).name
            path.parent.mkdir()
            path.write_bytes(machos[name])
            path.chmod(0o755)
            paths[name] = path
        sign_files(wheel.stem, list(paths.values()))
        signed = {name: path.read_bytes() for name, path in paths.items()}

    def transform(info: zipfile.ZipInfo, data: bytes) -> bytes:
        return signed.get(info.filename, data)

    rewrite_wheel(wheel, transform)
    return targets


def _private_file(path: Path, data: bytes) -> Path:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(data)
    return path


def sign_dist(dist_dir: Path, config: SigningConfig, tool_dir: Path) -> None:
    rcodesign = fetch_rcodesign(tool_dir)
    with tempfile.TemporaryDirectory(prefix="clud-macsign-secrets-") as secrets_dir:
        secrets = Path(secrets_dir)
        p12 = _private_file(secrets / "developer-id.p12", config.cert_p12)
        password = _private_file(secrets / "p12-password", config.cert_password.encode())

        def sign_files(label: str, files: list[Path]) -> None:
            for path in files:
                print(f"rcodesign: signing {label}:{path.name} as {config.identity}")
                process.run(sign_command(rcodesign, p12, password, path), check=True)

        failures = []
        for wheel in macos_wheels(dist_dir):
            ours = sign_wheel(wheel, sign_files)
            print(f"{wheel.name}: signed {', '.join(ours)}")
            failures += verify_wheel(wheel, config.identity)
    if failures:
        raise SigningError("unsigned macOS release binaries:\n" + "\n".join(failures))


def _quiet(command: list[str]) -> tuple[int, str, str]:
    """Run without ever echoing ``command``: notarytool's argv holds the password."""
    completed = process.run(command, capture_output=True, text=True, check=False)
    return completed.returncode, completed.stdout or "", completed.stderr or ""


def stage_machos(wheel: Path, dest: Path) -> list[str]:
    """Write ``wheel``'s Mach-O files, executable, under ``dest``; return their names."""
    names = sorted(wheel_machos(wheel))
    if not names:
        raise SigningError(f"no Mach-O files inside {wheel.name}")
    with zipfile.ZipFile(wheel) as archive:
        for index, name in enumerate(names):
            path = dest / str(index) / Path(name).name
            path.parent.mkdir(parents=True)
            path.write_bytes(archive.read(name))
            path.chmod(0o755)
    return names


def notarize_wheel(wheel: Path, config: NotaryConfig, run=_quiet) -> str:
    """Submit ``wheel``'s signed Mach-O files to Apple; return the Accepted submission ID."""
    with tempfile.TemporaryDirectory(prefix="clud-notary-") as scratch:
        staged = Path(scratch) / wheel.stem
        names = stage_machos(wheel, staged)
        upload = Path(scratch) / f"{wheel.stem}-notary.zip"
        code, out, err = run(ditto_command(staged, upload))
        if code != 0:
            raise SigningError(f"ditto failed for {wheel.name}: {(out + err).strip()}")
        print(f"notarytool: submitting {upload.name} ({len(names)} Mach-O file(s)); waiting")
        return submit_for_notarization(upload, config, wheel.name, run)


def submit_for_notarization(upload: Path, config: NotaryConfig, label: str, run=_quiet) -> str:
    """``notarytool submit --wait`` ``upload`` (a zip, .dmg or .pkg); return the
    Accepted submission ID, or print ``notarytool log`` and raise."""
    code, out, err = run(notary_submit_command(config, upload))
    print(err.strip())
    try:
        record = notary_result(out)
    except SigningError as exc:
        raise SigningError(f"{exc} for {label} (exit {code}): {out.strip()}") from exc
    submission = record.get("id", "")
    status = record.get("status", "")
    print(f"notarytool: {label}: submission {submission} status {status}")
    if code != 0 or status != NOTARY_ACCEPTED:
        if submission:
            _code, log, log_err = run(notary_log_command(config, submission))
            print(log.strip() or log_err.strip())
        raise SigningError(
            f"notarization of {label} was not accepted: submission "
            f"{submission or '<none>'} status {status or '<none>'} "
            f"({record.get('message', '')}) exit {code}"
        )
    return submission


def notarize_dist(dist_dir: Path, config: NotaryConfig, run=_quiet) -> dict[str, str]:
    verify_dist(dist_dir, config.identity)
    accepted = {wheel.name: notarize_wheel(wheel, config, run) for wheel in macos_wheels(dist_dir)}
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            for name, submission in accepted.items():
                handle.write(f"- `{name}`: notarization `{submission}` {NOTARY_ACCEPTED}\n")
    return accepted


def verify_dist(dist_dir: Path, identity: str) -> None:
    failures = []
    for wheel in macos_wheels(dist_dir):
        failures += verify_wheel(wheel, identity)
    if failures:
        raise SigningError("unsigned macOS release binaries:\n" + "\n".join(failures))


def _output(command: list[str]) -> tuple[int, str]:
    completed = process.run(command, capture_output=True, text=True, check=False)
    return completed.returncode, f"{completed.stdout or ''}{completed.stderr or ''}"


def native_problems(path: Path, identity: str) -> list[str]:
    """macOS's own verdict: codesign chain/runtime/timestamp and Gatekeeper notarization."""
    problems = []
    code, out = _output(["codesign", "--verify", "--strict", "--verbose=2", str(path)])
    if code != 0:
        problems.append(f"codesign --verify --strict failed: {out.strip()}")
    _code, details = _output(["codesign", "--display", "--verbose=2", str(path)])
    for needle in (f"Authority={identity}", "(runtime)", "Timestamp="):
        if needle not in details:
            problems.append(f"codesign --display lacks {needle!r}")
    code, verdict = _output(["spctl", "--assess", "--type", "install", "-vv", str(path)])
    print(verdict.strip())
    if code != 0 or NOTARIZED_SOURCE not in verdict:
        problems.append(f"spctl did not report {NOTARIZED_SOURCE!r}: {verdict.strip()}")
    return problems


def smoke_dist(dist_dir: Path, identity: str, version: str | None) -> None:
    """Require a notarized Developer ID verdict for every Mach-O, then run clud."""
    bad = []
    for wheel in macos_wheels(dist_dir):
        with tempfile.TemporaryDirectory(prefix="clud-macsmoke-") as scratch:
            clud = None
            for index, (name, data) in enumerate(sorted(wheel_machos(wheel).items())):
                path = Path(scratch) / str(index) / Path(name).name
                path.parent.mkdir()
                path.write_bytes(data)
                path.chmod(0o755)
                problems = native_problems(path, identity)
                print(f"{'FAIL' if problems else 'ok'}: {wheel.name}:{name}")
                bad += [f"  {wheel.name}:{name}: {problem}" for problem in problems]
                if name.endswith(".data/scripts/clud"):
                    clud = path
            if clud is None:
                bad.append(f"  {wheel.name}: no .data/scripts/clud")
                continue
            out = process.run(
                [str(clud), "--version"], capture_output=True, text=True, check=True
            ).stdout.strip()
            print(out)
            if version and out != f"clud {version}":
                bad.append(f"  {wheel.name}: clud --version printed {out!r}")
    if bad:
        raise SigningError("macOS release signature smoke failed:\n" + "\n".join(bad))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("mode", choices=("sign", "notarize", "verify", "smoke"))
    parser.add_argument("--dist-dir", type=Path, required=True)
    parser.add_argument("--tool-dir", type=Path, default=None)
    parser.add_argument("--version", default=None, help="smoke: expected clud version")
    args = parser.parse_args(argv)
    env = dict(os.environ)

    try:
        if args.mode == "sign":
            config = config_from_env(env)
            runner_temp = Path(env.get("RUNNER_TEMP", tempfile.gettempdir()))
            sign_dist(args.dist_dir, config, args.tool_dir or runner_temp / "rcodesign")
        elif args.mode == "notarize":
            notarize_dist(args.dist_dir, notary_config_from_env(env))
        elif args.mode == "verify":
            verify_dist(args.dist_dir, identity_from_env(env))
        else:
            smoke_dist(args.dist_dir, identity_from_env(env), args.version)
    except SigningError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
