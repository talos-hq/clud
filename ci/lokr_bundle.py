"""Prepare checksum-pinned lokr bytes for an offline, single-file clud installer."""

import hashlib
import json
import os
import re
import tempfile
from collections.abc import Callable
from pathlib import Path
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, build_opener

ROOT = Path(__file__).resolve().parent.parent
SPEC = ROOT / "crates/clud-bin/assets/lokr-release.json"
MAX_BYTES = 256 * 1024 * 1024
ORIGIN = "https://github.com/forge-ci/lokr/releases/download"


class ReleaseRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        parsed = urlsplit(newurl)
        if parsed.scheme != "https" or parsed.hostname not in {
            "github.com",
            "release-assets.githubusercontent.com",
            "objects.githubusercontent.com",
        }:
            raise ValueError("untrusted lokr release redirect")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def download_release(url: str) -> bytes:
    with build_opener(ReleaseRedirect()).open(url, timeout=60) as response:
        payload = response.read(MAX_BYTES + 1)
    if len(payload) > MAX_BYTES:
        raise ValueError("lokr release exceeds size limit")
    return payload


def artifact(manifest: dict, target: str) -> tuple[str, dict]:
    version = manifest["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?", version):
        raise ValueError("invalid lokr version")
    if tuple(map(int, version.split("-")[0].split("."))) < (0, 1, 0):
        raise ValueError("lokr release must provide the functional request CLI (>=0.1.0)")
    if manifest["tag"] not in {version, f"v{version}"}:
        raise ValueError("lokr tag and version disagree")
    item = manifest["artifacts"].get(target)
    if item is None:
        raise ValueError(f"no pinned lokr artifact for target {target}")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", item["filename"]):
        raise ValueError("invalid lokr asset filename")
    if not re.fullmatch(r"[0-9a-f]{64}", item["sha256"]):
        raise ValueError("invalid lokr digest")
    if not 0 < item["size_bytes"] <= MAX_BYTES:
        raise ValueError("invalid lokr release size")
    return version, item


def prepare(
    env: dict[str, str],
    target: str,
    manifest: dict,
    cache: Path,
    *,
    download: Callable[[str], bytes] = download_release,
) -> dict[str, str]:
    version, item = artifact(manifest, target)
    cached = cache / f"{item['sha256']}-{item['filename']}"
    if cached.is_file():
        payload = cached.read_bytes()
    else:
        payload = download(f"{ORIGIN}/{manifest['tag']}/{item['filename']}")
    if hashlib.sha256(payload).hexdigest() != item["sha256"]:
        raise ValueError("lokr release digest mismatch")
    if len(payload) != item["size_bytes"]:
        raise ValueError("lokr release size mismatch")
    cache.mkdir(parents=True, exist_ok=True)
    if not cached.exists():
        atomic_write(cached, payload)
    destination = cache / item["sha256"] / ("lokr.exe" if "windows" in target else "lokr")
    destination.parent.mkdir(parents=True, exist_ok=True)
    if (
        not destination.is_file()
        or hashlib.sha256(destination.read_bytes()).hexdigest() != item["sha256"]
    ):
        atomic_write(destination, payload)
    return env | {
        "CLUD_LOKR_BINARY": str(destination.resolve()),
        "CLUD_LOKR_VERSION": version,
        "CLUD_LOKR_SHA256": item["sha256"],
    }


def atomic_write(destination: Path, payload: bytes) -> None:
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as stage:
        stage.write(payload)
        stage.flush()
        os.fsync(stage.fileno())
        temporary = stage.name
    os.replace(temporary, destination)


def prepare_build(target: str) -> None:
    manifest = json.loads(SPEC.read_text(encoding="utf-8"))
    cache = ROOT / "target/lokr-bundle"
    for field in ("BINARY", "VERSION", "SHA256"):
        os.environ.pop(f"CLUD_LOKR_FALLBACK_{field}", None)
    # Static clud supports both glibc/Nix-ld desktops and musl-only hosts.
    primary = target.removesuffix("-musl") + "-gnu" if target.endswith("-linux-musl") else target
    env = prepare(dict(os.environ), primary, manifest, cache, download=download_release)
    if primary != target:
        fallback = prepare({}, target, manifest, cache, download=download_release)
        env.update(
            {
                key.replace("CLUD_LOKR_", "CLUD_LOKR_FALLBACK_"): value
                for key, value in fallback.items()
            }
        )
    os.environ.update(env)
