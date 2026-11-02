"""Build the public clud catalog from published GitHub release assets.

The generator deliberately does not infer an installable target from a wheel
name alone.  It opens each wheel and checks that the one clud executable that
the installer would extract is present and has executable bytes.
"""

from __future__ import annotations

import hashlib
import json
import re
import struct
from pathlib import Path
from urllib.request import Request, urlopen
from zipfile import ZipFile

SCHEMA = "https://zackees.github.io/manifest.json/v1/manifest.schema.json"
ONLINE_URL = "https://talos-hq.github.io/clud/install/manifest.json"
# Catalogs published before the move to talos-hq (#2126) still name the old
# Pages host; accept it until the next publish rewrites the field.
# The canonical custom domain; github.io 301-redirects to it (#2449).
DOMAIN_ONLINE_URL = "https://talosbot.ai/clud/install/manifest.json"
ACCEPTED_ONLINE_URLS = (
    ONLINE_URL,
    "https://zackees.github.io/clud/install/manifest.json",
    DOMAIN_ONLINE_URL,
)
# Fetch order: the domain, then the newest release's catalog on GitHub, so a
# lost domain degrades instead of breaking. Mirrors published_urls.rs.
INSTALL_CATALOG_URLS = (
    "https://talosbot.ai/clud/install/manifest.json",
    "https://github.com/talos-hq/clud/releases/latest/download/installer-candidate-manifest.json",
)
REPO_API = "https://api.github.com/repos/talos-hq/clud/releases"
TARGETS = {
    "win_amd64": ("windows", "x86_64", "clud.exe"),
    "win_arm64": ("windows", "aarch64", "clud.exe"),
    "macosx_10_15_x86_64": ("darwin", "x86_64", "clud"),
    "macosx_11_0_arm64": ("darwin", "aarch64", "clud"),
    "manylinux_2_17_x86_64.manylinux2014_x86_64": ("linux", "x86_64", "clud"),
    "manylinux_2_17_aarch64.manylinux2014_aarch64": ("linux", "aarch64", "clud"),
}
DIRECT_TARGETS = {
    "x86_64-pc-windows-msvc.exe": ("windows", "x86_64", "clud.exe"),
    "aarch64-pc-windows-msvc.exe": ("windows", "aarch64", "clud.exe"),
    "x86_64-apple-darwin": ("darwin", "x86_64", "clud"),
    "aarch64-apple-darwin": ("darwin", "aarch64", "clud"),
    "x86_64-unknown-linux-gnu": ("linux", "x86_64", "clud"),
    "aarch64-unknown-linux-gnu": ("linux", "aarch64", "clud"),
    "x86_64-unknown-linux-musl": ("linux", "x86_64", "clud"),
    "aarch64-unknown-linux-musl": ("linux", "aarch64", "clud"),
}
VERSION = re.compile(r"^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.+-]+)?$")


def version_key(version: str) -> tuple:
    match = re.match(r"^(\d+)\.(\d+)\.(\d+)(.*)$", version)
    if not match:
        raise ValueError(f"invalid version: {version}")
    major, minor, patch, suffix = match.groups()
    prerelease = suffix.split("+", 1)[0].removeprefix("-") if suffix.startswith("-") else ""
    identifiers = tuple(
        (0, int(part)) if part.isdecimal() else (1, part) for part in prerelease.split(".") if part
    )
    return int(major), int(minor), int(patch), int(not prerelease), identifiers


def wheel_target(name: str, version: str) -> tuple[str, str, str] | None:
    prefix = f"clud-{version}-py3-none-"
    if not name.startswith(prefix) or not name.endswith(".whl"):
        return None
    return TARGETS.get(name[len(prefix) : -4])


def direct_target(name: str, version: str) -> tuple[str, str, str] | None:
    prefix = f"clud-{version}-"
    return DIRECT_TARGETS.get(name[len(prefix) :]) if name.startswith(prefix) else None


def linux_variant(filename: str, os_name: str) -> tuple[dict, dict]:
    """Describe the host requirement separately from the executable flavor."""
    if os_name != "linux":
        return {}, {}
    if filename.endswith("-unknown-linux-musl"):
        return {}, {"flavor": "static-musl"}
    if filename.endswith("-unknown-linux-gnu") or filename.endswith(".whl"):
        return {"libc": "glibc"}, {"flavor": "gnu"}
    raise ValueError(f"unclassified Linux release asset: {filename}")


def is_executable(data: bytes, executable: str) -> bool:
    return (
        data.startswith(b"MZ")
        if executable.endswith(".exe")
        else (data.startswith(b"\x7fELF") or data.startswith(b"\xcf\xfa\xed\xfe"))
    )


def verify_static_musl_elf(data: bytes, arch: str) -> None:  # noqa: C901
    """Reject truncated, wrong-architecture, or dynamically linked Linux assets."""
    if len(data) < 64 or data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("static musl asset must be a little-endian ELF64 executable")
    if struct.unpack_from("<H", data, 16)[0] not in (2, 3):
        raise ValueError("static musl asset is not an ELF executable")
    machine = struct.unpack_from("<H", data, 18)[0]
    if machine != {"x86_64": 62, "aarch64": 183}[arch]:
        raise ValueError("static musl asset architecture mismatch")
    phoff = struct.unpack_from("<Q", data, 32)[0]
    phentsize, phnum = struct.unpack_from("<HH", data, 54)
    if not phnum or phentsize < 56 or phoff + phentsize * phnum > len(data):
        raise ValueError("static musl asset has an invalid program header table")
    for index in range(phnum):
        header = phoff + index * phentsize
        kind = struct.unpack_from("<I", data, header)[0]
        if kind == 3:  # PT_INTERP
            raise ValueError("static musl asset has a PT_INTERP loader")
        if kind != 2:  # PT_DYNAMIC
            continue
        offset = struct.unpack_from("<Q", data, header + 8)[0]
        size = struct.unpack_from("<Q", data, header + 32)[0]
        if offset + size > len(data) or size % 16:
            raise ValueError("static musl asset has an invalid dynamic table")
        for position in range(offset, offset + size, 16):
            if struct.unpack_from("<q", data, position)[0] == 1:  # DT_NEEDED
                raise ValueError("static musl asset has a DT_NEEDED dependency")


def verify_direct_executable(data: bytes, os_name: str, arch: str, flavor: str | None) -> None:  # noqa: C901
    """Check the advertised native format and machine before cataloging it."""
    if os_name == "linux":
        if flavor == "static-musl":
            verify_static_musl_elf(data, arch)
            return
        if len(data) < 20 or data[:6] != b"\x7fELF\x02\x01":
            raise ValueError("GNU asset is not a little-endian ELF64 executable")
        if struct.unpack_from("<H", data, 18)[0] != {"x86_64": 62, "aarch64": 183}[arch]:
            raise ValueError("GNU asset architecture mismatch")
        return
    if os_name == "windows":
        if len(data) < 64 or data[:2] != b"MZ":
            raise ValueError("invalid PE executable")
        offset = struct.unpack_from("<I", data, 0x3C)[0]
        if offset + 6 > len(data) or data[offset : offset + 4] != b"PE\0\0":
            raise ValueError("invalid PE header")
        if struct.unpack_from("<H", data, offset + 4)[0] != {
            "x86_64": 0x8664,
            "aarch64": 0xAA64,
        }[arch]:
            raise ValueError("PE architecture mismatch")
        return
    if os_name == "darwin":
        if len(data) < 32 or data[:4] != b"\xcf\xfa\xed\xfe":
            raise ValueError("invalid Mach-O executable")
        if struct.unpack_from("<I", data, 4)[0] != {
            "x86_64": 0x01000007,
            "aarch64": 0x0100000C,
        }[arch]:
            raise ValueError("Mach-O architecture mismatch")
        return
    raise ValueError(f"unsupported release OS: {os_name}")


def executable_member(wheel_bytes: bytes, executable: str) -> str:
    from io import BytesIO

    with ZipFile(BytesIO(wheel_bytes)) as wheel:
        members = [
            name for name in wheel.namelist() if name.endswith(f".data/scripts/{executable}")
        ]
        if len(members) != 1:
            raise ValueError(f"wheel must contain exactly one {executable}: {members}")
        data = wheel.read(members[0])
        if not is_executable(data, executable):
            raise ValueError("clud is not a PE, ELF or Mach-O executable")
        return members[0]


def _verified_catalog_assets(
    catalog: dict | None,
) -> dict[tuple[str, str], tuple[dict, dict, dict]]:
    if (
        not isinstance(catalog, dict)
        or catalog.get("kind") != "Catalog"
        or catalog.get("schema_version") != 1
        or catalog.get("tool") != "clud"
        or catalog.get("online_url") not in ACCEPTED_ONLINE_URLS
    ):
        return {}
    verified = {}
    entries = catalog.get("releases")
    if not isinstance(entries, list):
        return {}
    for release in entries:
        if not isinstance(release, dict) or not isinstance(release.get("version"), str):
            continue
        platforms = release.get("platforms")
        if not isinstance(platforms, list):
            continue
        for item in platforms:
            if not isinstance(item, dict):
                continue
            platform = item.get("platform")
            asset = item.get("asset")
            if (
                isinstance(platform, dict)
                and isinstance(platform.get("os"), str)
                and isinstance(platform.get("arch"), str)
                and isinstance(asset, dict)
                and isinstance(asset.get("filename"), str)
            ):
                verified[(release["version"], asset["filename"])] = (
                    platform, item.get("variant", {}), asset
                )
    return verified


def catalog_from_releases(  # noqa: C901
    releases: list[dict], fetch_bytes, verified_catalog: dict | None = None
) -> dict:
    cached_assets = _verified_catalog_assets(verified_catalog)
    entries = []
    for release in releases:
        version = release["tag_name"].removeprefix("v")
        if (
            release.get("draft")
            or release.get("prerelease")
            or not VERSION.fullmatch(version)
            or "-" in version
        ):
            continue
        platforms = []
        seen = set()
        assets = sorted(
            release.get("assets", []),
            key=lambda asset: direct_target(asset["name"], version) is None,
        )
        for asset in assets:
            direct = direct_target(asset["name"], version)
            target = direct or wheel_target(asset["name"], version)
            if target is None:
                continue
            os_name, arch, executable = target
            platform_extra, variant = linux_variant(asset["name"], os_name)
            key = (
                os_name,
                arch,
                tuple(sorted(platform_extra.items())),
                tuple(sorted(variant.items())),
            )
            if key in seen:
                if direct:
                    raise ValueError(f"duplicate platform variant in {version}: {key}")
                continue
            seen.add(key)
            filename = asset["name"]
            url = asset["browser_download_url"]
            api_digest = asset.get("digest")
            size = asset.get("size")
            cached = cached_assets.get((version, filename))
            cached_platform, cached_variant, cached_asset = cached if cached else (None, None, None)
            media_type = "application/octet-stream" if direct else "application/zip"
            reused = (
                cached_platform == {"os": os_name, "arch": arch, **platform_extra}
                and cached_variant == variant
                and cached_asset.get("filename") == filename
                and cached_asset.get("media_type") == media_type
                and cached_asset.get("size_bytes") == size
                and isinstance(cached_asset.get("sha256"), str)
                and len(cached_asset["sha256"]) == 64
                and all(c in "0123456789abcdef" for c in cached_asset["sha256"])
                and api_digest == f"sha256:{cached_asset.get('sha256')}"
                and cached_asset.get("urls") == [url]
                and cached_asset.get("provides") == ["clud"]
                and isinstance(size, int)
                and isinstance(api_digest, str)
            )
            if reused:
                digest = cached_asset["sha256"]
                print(f"reused verified release asset: {filename} ({size} bytes)", flush=True)
            else:
                data = fetch_bytes(url)
                digest = hashlib.sha256(data).hexdigest()
                if api_digest != f"sha256:{digest}" or size != len(data):
                    raise ValueError(f"published asset digest/size mismatch: {filename}")
                if direct:
                    verify_direct_executable(data, os_name, arch, variant.get("flavor"))
                else:
                    executable_member(data, executable)
                print(f"verified release asset: {filename} ({len(data)} bytes)", flush=True)
            entry = {
                "platform": {"os": os_name, "arch": arch, **platform_extra},
                "asset": {
                    "filename": filename,
                    "media_type": media_type,
                    "size_bytes": size,
                    "sha256": digest,
                    "urls": [url],
                    "provides": ["clud"],
                },
            }
            if variant:
                entry["variant"] = variant
            platforms.append(entry)
        if platforms:
            entries.append(
                {
                    "version": version,
                    "published_at": release["published_at"],
                    "platforms": sorted(
                        platforms,
                        key=lambda item: (
                            item["platform"]["os"],
                            item["platform"]["arch"],
                            item.get("variant", {}).get("flavor", ""),
                        ),
                    ),
                }
            )
    entries.sort(key=lambda entry: version_key(entry["version"]), reverse=True)
    prereleases = {
        release["tag_name"].removeprefix("v")
        for release in releases
        if release.get("prerelease")
    }
    required = {(os_name, arch) for os_name, arch, _ in TARGETS.values()}
    stable = next(
        (
            entry["version"]
            for entry in entries
            if "-" not in entry["version"]
            and entry["version"] not in prereleases
            and {
                (item["platform"]["os"], item["platform"]["arch"])
                for item in entry["platforms"]
            } == required
        ),
        None,
    )
    if not stable:
        raise ValueError("no complete installable stable release")
    return {
        "$schema": SCHEMA,
        "kind": "Catalog",
        "schema_version": 1,
        "tool": "clud",
        "online_url": ONLINE_URL,
        "channels": {"latest-stable": stable},
        "releases": entries,
    }


def candidate_catalog_from_release(
    candidate: dict, stable_catalog: dict, fetch_bytes=None
) -> dict:
    """Bind one public prerelease to the previously published stable catalog."""
    if fetch_bytes is None:
        fetch_bytes = fetch
    version = candidate.get("tag_name", "").removeprefix("v")
    if candidate.get("draft") or candidate.get("prerelease") is not True:
        raise ValueError("candidate must be a public prerelease")
    if not VERSION.fullmatch(version) or "-" in version:
        raise ValueError("candidate requires a stable-form version")
    if (
        stable_catalog.get("kind") != "Catalog"
        or stable_catalog.get("schema_version") != 1
        or stable_catalog.get("tool") != "clud"
        or stable_catalog.get("online_url") not in ACCEPTED_ONLINE_URLS
        or set(stable_catalog.get("channels", {})) != {"latest-stable"}
    ):
        raise ValueError("previous stable catalog is invalid")
    previous = stable_catalog["channels"]["latest-stable"]
    if version_key(version) <= version_key(previous):
        raise ValueError("candidate must be newer than previous stable")
    previous_rows = stable_catalog.get("releases", [])
    if not any(row.get("version") == previous for row in previous_rows):
        raise ValueError("previous stable release is missing")
    if any(row.get("version") == version for row in previous_rows):
        raise ValueError("candidate is already in the stable catalog")
    candidate_release = {**candidate, "prerelease": False}
    built = catalog_from_releases([candidate_release], fetch_bytes)
    if built["channels"]["latest-stable"] != version:
        raise ValueError("candidate release is incomplete")
    return {
        **built,
        "channels": {"latest-stable": previous, "candidate": version},
        "releases": built["releases"] + previous_rows,
    }


def fetch(url: str) -> bytes:
    request = Request(
        url,
        headers={"Accept": "application/vnd.github+json", "User-Agent": "clud-catalog"},
    )
    with urlopen(request, timeout=60) as response:
        return response.read()


def fetch_first(urls: tuple[str, ...], fetch_bytes=fetch) -> bytes:
    """The first of `urls` that answers; raises the last error otherwise."""
    error: Exception | None = None
    for url in urls:
        try:
            return fetch_bytes(url)
        except OSError as exc:
            error = exc
    raise OSError(f"every published URL failed: {urls}") from error


def published_releases(fetch_bytes=fetch) -> list[dict]:
    releases = []
    page = 1
    while True:
        batch = json.loads(fetch_bytes(f"{REPO_API}?per_page=100&page={page}"))
        releases.extend(batch)
        if len(batch) < 100:
            return releases
        page += 1


def write_catalog(
    destination: Path,
    releases: list[dict],
    fetch_bytes=fetch,
    verified_catalog: dict | None = None,
) -> dict:
    catalog = catalog_from_releases(releases, fetch_bytes, verified_catalog)
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(json.dumps(catalog, indent=2) + "\n", encoding="utf-8")
    return catalog
