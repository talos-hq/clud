"""Authenticode-sign the Windows release wheels through Azure Artifact Signing.

Both ``*-pc-windows-msvc`` release lanes cross-build on Linux runners, so the
signer is jsign (https://github.com/ebourg/jsign), a pure-Java Authenticode
implementation with a ``TRUSTEDSIGNING`` keystore for Azure Artifact Signing.
The jar is pinned by version and SHA-256 and refused on mismatch.

``_build-target.yml`` runs ``azure/login`` (OIDC, no client secret) first;
this script then asks the Azure CLI for a short-lived code-signing token and
hands it to jsign through the environment, never on argv.

``sign --dist-dir dist`` signs every Windows PE inside each Windows wheel
(``clud.exe``, ``clud-webterm.exe`` and the bundled Kitty terminal binaries)
that does not already carry an Authenticode signature. PEs that ship already
signed by their vendor (Microsoft's ``conpty.dll`` / ``OpenConsole.exe``) keep
that signature. The wheel is rewritten through ``ci.wheel_rewrite`` so RECORD
matches the signed bytes. This runs right after ``Build wheel``, before the
size/RECORD checks and the upload, so the PyPI wheels, the standalone ``.exe``
assets extracted from them by ``installer.release_assets`` and SHA256SUMS all
carry signed bytes. A structural post-check fails the step when any PE is left
unsigned, or a PE we signed lacks the publisher name or RFC 3161 timestamp.

``smoke --dist-dir wheels`` runs on native Windows and requires
``Get-AuthenticodeSignature`` to report ``Valid`` for every PE in every wheel,
then executes ``clud.exe --version``.

Usage (CI):
    python -m ci.sign_windows_release sign --dist-dir dist/
    python -m ci.sign_windows_release verify --dist-dir dist/
    python -m ci.sign_windows_release smoke --dist-dir wheels/   # Windows runner
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import struct
import sys
import tempfile
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path

from ci import process
from ci.wheel_rewrite import rewrite_wheel

JSIGN_VERSION = "7.6"
JSIGN_URL = (
    f"https://github.com/ebourg/jsign/releases/download/{JSIGN_VERSION}/jsign-{JSIGN_VERSION}.jar"
)
JSIGN_SHA256 = "6cf58aa080e7ff82656f320cff8d4a3bff30408df8766806dcba3740dd839fa5"

TIMESTAMP_URL = "http://timestamp.acs.microsoft.com"
CODESIGNING_RESOURCE = "https://codesigning.azure.net"
TOKEN_ENV = "CLUD_AZURE_SIGNING_TOKEN"

PE_SUFFIXES = (".exe", ".dll", ".pyd")

# WIN_CERTIFICATE: wRevision 0x0200, wCertificateType WIN_CERT_TYPE_PKCS_SIGNED_DATA.
WIN_CERT_REVISION_2 = 0x0200
WIN_CERT_TYPE_PKCS_SIGNED_DATA = 0x0002
# DER of OID 1.3.6.1.4.1.311.3.3.1 (Microsoft RFC 3161 timestamp countersignature).
RFC3161_TIMESTAMP_OID = bytes.fromhex("060a2b060104018237030301")
SECURITY_DIRECTORY_INDEX = 4


class SigningError(RuntimeError):
    """A signing precondition or post-signing check failed."""


@dataclass(frozen=True)
class SigningConfig:
    """Azure Artifact Signing coordinates, read from the job environment."""

    endpoint_host: str
    account: str
    profile: str
    publisher: str


def endpoint_host(endpoint: str) -> str:
    """``https://eus.codesigning.azure.net/`` -> ``eus.codesigning.azure.net``."""
    host = endpoint.strip().removeprefix("https://").removeprefix("http://")
    host = host.split("/", 1)[0]
    if not host.endswith(".codesigning.azure.net"):
        raise SigningError(f"unexpected Artifact Signing endpoint: {endpoint!r}")
    return host


def _required_env(env: dict[str, str], name: str) -> str:
    value = env.get(name, "").strip()
    if not value:
        raise SigningError(f"{name} is not set (expected a talos-hq org Actions secret)")
    return value


def publisher_common_name(publisher: str) -> str:
    """Accept either a bare name or a full subject DN (``CN=..., O=...``)."""
    for part in publisher.split(","):
        key, _, value = part.strip().partition("=")
        if key.strip().upper() == "CN" and value.strip():
            return value.strip().strip('"')
    return publisher.strip()


def config_from_env(env: dict[str, str]) -> SigningConfig:
    return SigningConfig(
        endpoint_host=endpoint_host(_required_env(env, "AZURE_SIGNING_ENDPOINT")),
        account=_required_env(env, "AZURE_SIGNING_ACCOUNT"),
        profile=_required_env(env, "AZURE_SIGNING_PROFILE"),
        publisher=publisher_common_name(_required_env(env, "AZURE_SIGN_PUBLISHER")),
    )


def require_trusted_ref(env: dict[str, str]) -> None:
    """Azure trusts this repo's OIDC subject only for main and tags.

    Releases run on tag pushes (or a dispatch from main). Fail with a clear
    message instead of an opaque azure/login AADSTS error elsewhere.
    """
    ref = env.get("GITHUB_REF", "")
    if ref == "refs/heads/main" or ref.startswith("refs/tags/"):
        return
    raise SigningError(
        f"signing needs a trusted OIDC subject (refs/heads/main or a tag); got {ref!r}. "
        "Push the release tag or dispatch Auto Release from main."
    )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fetch_jsign(dest_dir: Path) -> Path:
    """Download the pinned jsign jar once and verify its SHA-256."""
    jar = dest_dir / f"jsign-{JSIGN_VERSION}.jar"
    if jar.is_file() and sha256_file(jar) == JSIGN_SHA256:
        return jar
    dest_dir.mkdir(parents=True, exist_ok=True)
    partial = jar.with_suffix(".partial")
    with urllib.request.urlopen(JSIGN_URL, timeout=120) as response, partial.open("wb") as out:
        shutil.copyfileobj(response, out)
    actual = sha256_file(partial)
    if actual != JSIGN_SHA256:
        partial.unlink()
        raise SigningError(
            f"jsign {JSIGN_VERSION} sha256 mismatch: got {actual}, pinned {JSIGN_SHA256}"
        )
    partial.replace(jar)
    print(f"jsign {JSIGN_VERSION}: sha256 verified ({JSIGN_SHA256})")
    return jar


def access_token() -> str:
    """Mint an Artifact Signing token from the ``azure/login`` session."""
    az = shutil.which("az") or "az"
    completed = process.run(
        [
            az,
            "account",
            "get-access-token",
            "--resource",
            CODESIGNING_RESOURCE,
            "--query",
            "accessToken",
            "-o",
            "tsv",
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    token = completed.stdout.strip()
    if not token:
        raise SigningError("az returned an empty code-signing access token")
    if os.environ.get("GITHUB_ACTIONS") == "true":
        print(f"::add-mask::{token}", flush=True)
    return token


def jsign_command(jar: Path, config: SigningConfig, files: list[Path]) -> list[str]:
    return [
        "java",
        "-jar",
        str(jar),
        "--storetype",
        "TRUSTEDSIGNING",
        "--keystore",
        config.endpoint_host,
        "--storepass",
        f"env:{TOKEN_ENV}",
        "--alias",
        f"{config.account}/{config.profile}",
        "--alg",
        "SHA-256",
        "--tsaurl",
        TIMESTAMP_URL,
        "--tsmode",
        "RFC3161",
        *[str(path) for path in files],
    ]


def is_pe_name(name: str) -> bool:
    return name.lower().endswith(PE_SUFFIXES)


def windows_wheels(dist_dir: Path) -> list[Path]:
    wheels = sorted(dist_dir.glob("*-win_*.whl"))
    if not wheels:
        raise SigningError(f"no Windows wheels under {dist_dir}")
    return wheels


def authenticode_blob(data: bytes) -> bytes | None:
    """Return the PE's WIN_CERTIFICATE table, or None when it has none."""
    if data[:2] != b"MZ" or len(data) < 0x40:
        raise SigningError("not a PE image (missing MZ header)")
    (pe_offset,) = struct.unpack_from("<I", data, 0x3C)
    if data[pe_offset : pe_offset + 4] != b"PE\0\0":
        raise SigningError("not a PE image (missing PE signature)")
    optional = pe_offset + 24
    (magic,) = struct.unpack_from("<H", data, optional)
    directories = {0x10B: optional + 96, 0x20B: optional + 112}.get(magic)
    if directories is None:
        raise SigningError(f"unknown PE optional-header magic {magic:#x}")
    offset, size = struct.unpack_from("<II", data, directories + SECURITY_DIRECTORY_INDEX * 8)
    if offset == 0 or size == 0:
        return None
    return data[offset : offset + size]


def signature_problem(data: bytes, publisher: str | None) -> str | None:
    """Why ``data`` lacks a valid-shaped signature (by ``publisher``, when given)."""
    blob = authenticode_blob(data)
    if blob is None or len(blob) < 8:
        return "no Authenticode signature (empty PE security directory)"
    _length, revision, cert_type = struct.unpack_from("<IHH", blob, 0)
    if revision != WIN_CERT_REVISION_2 or cert_type != WIN_CERT_TYPE_PKCS_SIGNED_DATA:
        return f"unexpected WIN_CERTIFICATE revision/type {revision:#x}/{cert_type:#x}"
    if publisher is None:
        return None
    if publisher.encode("utf-8") not in blob:
        return f"signature does not name publisher {publisher!r}"
    if RFC3161_TIMESTAMP_OID not in blob:
        return "signature carries no RFC 3161 timestamp"
    return None


def wheel_pes(wheel: Path) -> dict[str, bytes]:
    with zipfile.ZipFile(wheel) as archive:
        return {n: archive.read(n) for n in archive.namelist() if is_pe_name(n)}


def unsigned_members(pes: dict[str, bytes]) -> list[str]:
    """Members with no Authenticode signature: ours to sign. Vendor-signed PEs are kept."""
    return sorted(name for name, data in pes.items() if authenticode_blob(data) is None)


def sign_wheel(wheel: Path, sign_files) -> list[str]:
    """Sign every unsigned PE in ``wheel`` via ``sign_files`` and rewrite it in place."""
    pes = wheel_pes(wheel)
    if not pes:
        raise SigningError(f"no Windows PE files inside {wheel.name}")
    targets = unsigned_members(pes)
    if not targets:
        return []
    with tempfile.TemporaryDirectory(prefix="clud-sign-") as scratch:
        paths: dict[str, Path] = {}
        for index, name in enumerate(targets):
            path = Path(scratch) / str(index) / Path(name).name
            path.parent.mkdir()
            path.write_bytes(pes[name])
            paths[name] = path
        sign_files(list(paths.values()))
        signed = {name: path.read_bytes() for name, path in paths.items()}

    def transform(info: zipfile.ZipInfo, data: bytes) -> bytes:
        return signed.get(info.filename, data)

    rewrite_wheel(wheel, transform)
    return targets


def verify_wheel(wheel: Path, publisher: str, ours: list[str] | None = None) -> list[str]:
    """Fail-closed structural check: every PE signed; ours by ``publisher`` + timestamp."""
    failures = []
    for name, data in sorted(wheel_pes(wheel).items()):
        must_be_ours = ours is None or name in ours
        problem = signature_problem(data, publisher)
        if problem and not must_be_ours:
            # A vendor-signed PE we left untouched: only demand a signature.
            problem = signature_problem(data, None)
        if problem:
            failures.append(f"  {wheel.name}:{name}: {problem}")
        else:
            print(f"signed: {wheel.name}:{name}")
    return failures


def sign_dist(dist_dir: Path, config: SigningConfig, jsign_dir: Path) -> None:
    jar = fetch_jsign(jsign_dir)
    env = dict(os.environ)
    env[TOKEN_ENV] = access_token()

    def sign_files(files: list[Path]) -> None:
        print(f"jsign: signing {len(files)} file(s) with {config.account}/{config.profile}")
        process.run(jsign_command(jar, config, files), env=env, check=True)

    failures = []
    for wheel in windows_wheels(dist_dir):
        ours = sign_wheel(wheel, sign_files)
        print(f"{wheel.name}: signed {len(ours)} PE(s): {', '.join(ours) or '(none)'}")
        failures += verify_wheel(wheel, config.publisher, ours)
    if failures:
        raise SigningError("unsigned Windows release binaries:\n" + "\n".join(failures))


def verify_dist(dist_dir: Path, publisher: str) -> None:
    failures = []
    for wheel in windows_wheels(dist_dir):
        # Without the signing log, clud's own binaries must name the publisher;
        # any other PE (wezterm, vendor DLLs) only needs a signature.
        ours = [n for n in wheel_pes(wheel) if Path(n).name.lower().startswith("clud")]
        failures += verify_wheel(wheel, publisher, ours)
    if failures:
        raise SigningError("unsigned Windows release binaries:\n" + "\n".join(failures))


AUTHENTICODE_PS = (
    "$s = Get-AuthenticodeSignature -LiteralPath $env:CLUD_SIGNED_PE; "
    'Write-Output "$($s.Status)|$($s.SignerCertificate.Subject)"'
)


def authenticode_status(path: Path) -> tuple[str, str]:
    """Windows' own verdict (chain, timestamp, hash) via Get-AuthenticodeSignature."""
    completed = process.run(
        ["pwsh", "-NoProfile", "-NonInteractive", "-Command", AUTHENTICODE_PS],
        env={**os.environ, "CLUD_SIGNED_PE": str(path)},
        capture_output=True,
        text=True,
        check=True,
    )
    status, _, subject = completed.stdout.strip().partition("|")
    return status, subject


def smoke_dist(dist_dir: Path, version: str | None) -> None:
    """Require ``Valid`` for every PE in every wheel, then run ``clud.exe --version``."""
    bad = []
    for wheel in windows_wheels(dist_dir):
        with tempfile.TemporaryDirectory(prefix="clud-smoke-") as scratch:
            clud_exe = None
            for index, (name, data) in enumerate(sorted(wheel_pes(wheel).items())):
                path = Path(scratch) / str(index) / Path(name).name
                path.parent.mkdir()
                path.write_bytes(data)
                status, subject = authenticode_status(path)
                print(f"{status}: {wheel.name}:{name} [{subject}]")
                if status != "Valid":
                    bad.append(f"  {wheel.name}:{name}: {status}")
                if name.endswith(".data/scripts/clud.exe"):
                    clud_exe = path
            if clud_exe is None:
                bad.append(f"  {wheel.name}: no .data/scripts/clud.exe")
                continue
            out = process.run(
                [str(clud_exe), "--version"], capture_output=True, text=True, check=True
            ).stdout.strip()
            print(out)
            if version and out != f"clud {version}":
                bad.append(f"  {wheel.name}: clud.exe --version printed {out!r}")
    if bad:
        raise SigningError("Windows release smoke failed:\n" + "\n".join(bad))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("mode", choices=("sign", "verify", "smoke"))
    parser.add_argument("--dist-dir", type=Path, required=True)
    parser.add_argument("--jsign-dir", type=Path, default=None)
    parser.add_argument("--version", default=None, help="smoke: expected clud version")
    args = parser.parse_args(argv)

    try:
        if args.mode == "smoke":
            smoke_dist(args.dist_dir, args.version)
            return 0
        config = config_from_env(dict(os.environ))
        if args.mode == "sign":
            require_trusted_ref(dict(os.environ))
            runner_temp = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir()))
            jsign_dir = args.jsign_dir or runner_temp / "jsign"
            sign_dist(args.dist_dir, config, jsign_dir)
        else:
            verify_dist(args.dist_dir, config.publisher)
    except SigningError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
