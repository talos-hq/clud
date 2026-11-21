"""Resolve one exact native asset from an anonymous public release catalog."""

from __future__ import annotations

import json
from io import BytesIO
from urllib.request import Request, urlopen
from zipfile import ZipFile

from ci.public_release import REPO
from installer.catalog import ONLINE_URL, VERSION, executable_member

DIRECT = "application/octet-stream"
WHEEL = "application/zip"


def native_row(release: dict, os_name: str, arch: str) -> dict:
    """The one catalog row the native installer installs on this host.

    Linux takes the static musl binary. Since #2863 macOS ships one universal
    binary that the catalog cannot name (installed clud versions accept only
    per-architecture rows), so a macOS release resolves to its wheel, whose
    thin executable the installer extracts; older releases keep their direct
    per-architecture binary.
    """
    rows = [
        row
        for row in release["platforms"]
        if row["platform"]["os"] == os_name
        and row["platform"]["arch"] == arch
        and (os_name != "linux" or row.get("variant") == {"flavor": "static-musl"})
    ]
    direct = [row for row in rows if row["asset"]["media_type"] == DIRECT]
    if not direct and os_name == "darwin":
        direct = [row for row in rows if row["asset"]["media_type"] == WHEEL]
    if len(direct) != 1:
        raise ValueError("public catalog lacks one exact native asset")
    return direct[0]


def executable_bytes(row: dict, payload: bytes) -> bytes:
    """The executable a row installs: the asset itself, or a wheel's clud."""
    if row["asset"]["media_type"] == DIRECT:
        return payload
    member = executable_member(payload, "clud")
    with ZipFile(BytesIO(payload)) as wheel:
        return wheel.read(member)


def catalog_url(mode: str, tag: str) -> str:
    version = tag.removeprefix("v")
    if not VERSION.fullmatch(version) or "-" in version or "+" in version:
        raise ValueError("public release tag must be a stable-form version")
    if mode == "candidate":
        return f"https://github.com/{REPO}/releases/download/{tag}/installer-candidate-manifest.json"
    if mode == "released":
        return ONLINE_URL
    raise ValueError("invalid public installer mode")


def resolve(mode: str, tag: str, os_name: str, arch: str) -> dict:
    url = catalog_url(mode, tag)
    request = Request(url, headers={"User-Agent": "clud-public-installer-gate"})
    with urlopen(request, timeout=120) as response:
        if not response.geturl().startswith("https://"):
            raise ValueError("public catalog redirected away from HTTPS")
        data = response.read(8 * 1024 * 1024 + 1)
    if len(data) > 8 * 1024 * 1024:
        raise ValueError("public catalog exceeds size limit")
    catalog = json.loads(data)
    version = tag.removeprefix("v")
    channel = "candidate" if mode == "candidate" else "latest-stable"
    if catalog["channels"].get(channel) != version:
        raise ValueError("public catalog channel points to a different version")
    if mode == "candidate" and catalog["channels"].get("latest-stable") == version:
        raise ValueError("candidate incorrectly became latest stable")
    if mode == "released" and "candidate" in catalog["channels"]:
        raise ValueError("canonical catalog exposes a candidate channel")
    release = next(row for row in catalog["releases"] if row["version"] == version)
    asset = native_row(release, os_name, arch)["asset"]
    expected_url = (
        f"https://github.com/{REPO}/releases/download/{tag}/{asset['filename']}"
    )
    if asset["urls"] != [expected_url]:
        raise ValueError("public native asset URL is not fixed to the release tag")
    if len(asset["sha256"]) != 64 or asset["size_bytes"] <= 0:
        raise ValueError("public native asset has invalid digest or size")
    return {
        "version": version,
        "sha256": asset["sha256"],
        "size_bytes": asset["size_bytes"],
        "url": expected_url,
        "filename": asset["filename"],
    }
