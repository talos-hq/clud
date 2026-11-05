"""Acceptance checks for the release-backed installer catalog."""

from __future__ import annotations

import struct
from io import BytesIO
from zipfile import ZipFile

import pytest

from installer.catalog import (
    TARGETS,
    candidate_catalog_from_release,
    catalog_from_releases,
    executable_member,
    verify_static_musl_elf,
    version_key,
)
from installer.site import build_site, published_paths, render_page
from installer.verify_site import verify


def wheel(executable: str, data: bytes) -> bytes:
    output = BytesIO()
    with ZipFile(output, "w") as archive:
        archive.writestr(f"clud-2.9.0.data/scripts/{executable}", data)
        if executable == "clud.exe":
            archive.writestr("clud-2.9.0.data/scripts/clud", b"\x7fELFportable-fixture")
    return output.getvalue()


def pe_x64() -> bytes:
    payload = bytearray(160)
    payload[:2] = b"MZ"
    struct.pack_into("<I", payload, 0x3C, 128)
    payload[128:132] = b"PE\0\0"
    struct.pack_into("<H", payload, 132, 0x8664)
    return bytes(payload)


def release(version: str, url: str, payload: bytes) -> dict:
    import hashlib

    return {
        "tag_name": version,
        "published_at": "2026-09-01T00:00:00Z",
        "assets": [
            {
                "name": f"clud-{version}-py3-none-{platform}.whl",
                "browser_download_url": url,
                "digest": "sha256:" + hashlib.sha256(payload).hexdigest(),
                "size": len(payload),
            }
            for platform in TARGETS
        ],
    }


def test_semantic_version_sort_and_prerelease_not_latest() -> None:
    data = wheel("clud.exe", b"MZpayload")
    releases = [
        release("2.9.0", "https://example.com/29", data),
        release("2.10.0-rc.1", "https://example.com/210rc", data),
        release("2.10.0", "https://example.com/210", data),
    ]
    catalog = catalog_from_releases(releases, lambda _: data)
    assert [entry["version"] for entry in catalog["releases"]] == ["2.10.0", "2.9.0"]
    assert catalog["channels"]["latest-stable"] == "2.10.0"
    assert version_key("2.10.0") > version_key("2.9.0")
    assert version_key("2.10.0-rc.10") > version_key("2.10.0-rc.9")
    assert version_key("2.10.0") > version_key("2.10.0-rc.10")


def test_reject_changed_published_bytes() -> None:
    data = wheel("clud.exe", b"MZpayload")
    with pytest.raises(ValueError, match="digest/size mismatch"):
        catalog_from_releases(
            [release("2.9.0", "https://example.com", data)], lambda _: data + b"changed"
        )


def test_cached_catalog_reuses_only_unchanged_verified_release_assets() -> None:
    old_wheel = wheel("clud.exe", b"MZprevious")
    new_wheel = wheel("clud.exe", b"MZnew-release")
    old_release = release("2.9.0", "https://example.com/29", old_wheel)
    cached = catalog_from_releases([old_release], lambda _: old_wheel)
    new_release = release("2.10.0", "https://example.com/210", new_wheel)

    catalog = catalog_from_releases(
        [old_release, new_release],
        lambda url: new_wheel if url.endswith("210") else pytest.fail("unchanged asset fetched"),
        verified_catalog=cached,
    )

    assert catalog["channels"]["latest-stable"] == "2.10.0"
    assert [item["version"] for item in catalog["releases"]] == ["2.10.0", "2.9.0"]


def test_cached_catalog_from_old_pages_host_is_still_reused() -> None:
    # #2126: the live catalog predates the move to talos-hq and still names
    # the old host; the next publish must reuse it and rewrite the field.
    old_wheel = wheel("clud.exe", b"MZprevious")
    old_release = release("2.9.0", "https://example.com/29", old_wheel)
    cached = catalog_from_releases([old_release], lambda _: old_wheel)
    cached["online_url"] = "https://zackees.github.io/clud/install/manifest.json"

    catalog = catalog_from_releases(
        [old_release], lambda _: pytest.fail("unchanged asset fetched"), verified_catalog=cached
    )

    assert catalog["online_url"] == "https://talos-hq.github.io/clud/install/manifest.json"


def test_cached_catalog_refetches_release_asset_when_digest_changes() -> None:
    old_wheel = wheel("clud.exe", b"MZprevious")
    changed_wheel = wheel("clud.exe", b"MZchanged")
    old_release = release("2.9.0", "https://example.com/29", old_wheel)
    changed_release = release("2.9.0", "https://example.com/29", changed_wheel)
    cached = catalog_from_releases([old_release], lambda _: old_wheel)

    catalog = catalog_from_releases(
        [changed_release], lambda _: changed_wheel, verified_catalog=cached
    )

    asset = next(
        entry["asset"] for entry in catalog["releases"][0]["platforms"]
        if entry["platform"]["os"] == "windows" and entry["platform"]["arch"] == "x86_64"
    )
    assert asset["sha256"] != cached["releases"][0]["platforms"][0]["asset"]["sha256"]


def test_reject_wheel_without_real_clud() -> None:
    with pytest.raises(ValueError, match="exactly one"):
        executable_member(wheel("other.exe", b"MZpayload"), "clud.exe")
    with pytest.raises(ValueError, match="not a PE"):
        executable_member(wheel("clud.exe", b"invalid"), "clud.exe")


def test_pages_paths_root_serves_page_and_only_manifest_link(tmp_path) -> None:
    from html.parser import HTMLParser

    data = wheel("clud.exe", b"MZpayload")
    catalog = build_site(
        tmp_path, [release("2.9.0", "https://example.com/29", data)], lambda _: data
    )
    root = (tmp_path / "index.html").read_text(encoding="utf-8")
    page = (tmp_path / "install" / "index.html").read_text(encoding="utf-8")
    public = (tmp_path / "install" / "manifest.json").read_text(encoding="utf-8")

    class Links(HTMLParser):
        def __init__(self):
            super().__init__()
            self.hrefs = []

        def handle_starttag(self, tag, attrs):
            if tag == "a":
                self.hrefs.extend(value for key, value in attrs if key == "href")

    links = Links()
    links.feed(root)
    assert links.hrefs == ["/clud/install/manifest.json"]
    assert root == page
    assert "2.9.0" in page
    assert "No native downloads are available for this stable release" in page
    assert "releases/latest/download/clud-installer.exe" not in page
    assert '"latest-stable": "2.9.0"' in public
    assert catalog["online_url"] == "https://talos-hq.github.io/clud/install/manifest.json"
    rendered = {
        path.relative_to(tmp_path).as_posix()
        for path in tmp_path.rglob("*")
        if path.is_file() and path.name != ".nojekyll"
    }
    assert rendered == set(published_paths())


def test_page_links_only_verified_native_asset_from_selected_stable(tmp_path) -> None:
    import hashlib

    data = wheel("clud.exe", b"MZpayload")
    direct = pe_x64()
    item = release("2.9.0", "https://example.com/wheel", data)
    direct_url = "https://example.com/verified-native.exe"
    item["assets"].append(
        {
            "name": "clud-2.9.0-x86_64-pc-windows-msvc.exe",
            "browser_download_url": direct_url,
            "digest": "sha256:" + hashlib.sha256(direct).hexdigest(),
            "size": len(direct),
        }
    )
    build_site(
        tmp_path,
        [item],
        lambda url: direct if url == direct_url else data,
    )
    page = (tmp_path / "install" / "index.html").read_text(encoding="utf-8")
    assert f'href="{direct_url}"' in page
    assert "https://example.com/wheel" not in page
    assert "releases/latest/download" not in page
    assert "--installer" in page


def test_page_prefers_static_musl_and_escapes_catalog_urls() -> None:
    version = "2.10.0"
    base = {
        "platform": {"os": "linux", "arch": "x86_64"},
        "asset": {
            "filename": "clud-2.10.0-x86_64-unknown-linux-musl",
            "media_type": "application/octet-stream",
            "urls": ["https://example.com/static?a=1&b=2"],
        },
        "variant": {"flavor": "static-musl"},
    }
    gnu = {
        "platform": {"os": "linux", "arch": "x86_64", "libc": "glibc"},
        "asset": {
            "filename": "clud-2.10.0-x86_64-unknown-linux-gnu",
            "media_type": "application/octet-stream",
            "urls": ["https://example.com/gnu"],
        },
        "variant": {"flavor": "gnu"},
    }
    catalog = {
        "channels": {"latest-stable": version},
        "releases": [
            {
                "version": version,
                "platforms": [
                    gnu,
                    base,
                    {
                        "platform": {"os": "darwin", "arch": "aarch64"},
                        "asset": {
                            "filename": "clud-2.10.0-aarch64-apple-darwin",
                            "media_type": "application/octet-stream",
                            "urls": ["https://example.com/macos"],
                        },
                    },
                ],
            }
        ],
    }
    page = render_page(catalog)
    assert 'href="https://example.com/static?a=1&amp;b=2"' in page
    assert "https://example.com/gnu" not in page
    assert "chmod +x" in page
    assert "Open Anyway" in page


def test_issue_1480_site_inventory_covers_added_asset(tmp_path, monkeypatch) -> None:
    from installer import site

    monkeypatch.setattr(site, "STATIC_ASSETS", {"install/site.css": b"body {}"})
    data = wheel("clud.exe", b"MZpayload")
    build_site(tmp_path, [release("2.9.0", "https://example.com/29", data)], lambda _: data)
    rendered = {
        path.relative_to(tmp_path).as_posix()
        for path in tmp_path.rglob("*")
        if path.is_file() and path.name != ".nojekyll"
    }
    assert rendered == set(published_paths())
    assert (tmp_path / "install" / "site.css").read_bytes() == b"body {}"


def test_direct_binary_wins_over_wheel_for_same_target() -> None:
    import hashlib

    data = wheel("clud.exe", b"MZwheel")
    direct = pe_x64()
    item = release("2.9.0", "https://example.com/wheel", data)
    item["assets"].append(
        {
            "name": "clud-2.9.0-x86_64-pc-windows-msvc.exe",
            "browser_download_url": "https://example.com/direct",
            "digest": "sha256:" + hashlib.sha256(direct).hexdigest(),
            "size": len(direct),
        }
    )
    catalog = catalog_from_releases([item], lambda url: direct if url.endswith("direct") else data)
    asset = next(
        entry["asset"]
        for entry in catalog["releases"][0]["platforms"]
        if entry["platform"]["os"] == "windows" and entry["platform"]["arch"] == "x86_64"
    )
    assert asset["filename"].endswith("msvc.exe")
    assert asset["sha256"] == hashlib.sha256(direct).hexdigest()


def test_direct_binary_rejects_wrong_architecture() -> None:
    import hashlib

    payload = bytearray(pe_x64())
    struct.pack_into("<H", payload, 132, 0xAA64)
    direct = bytes(payload)
    data = wheel("clud.exe", b"MZwheel")
    item = release("2.9.0", "https://example.com/wheel", data)
    item["assets"].append({
        "name": "clud-2.9.0-x86_64-pc-windows-msvc.exe",
        "browser_download_url": "https://example.com/direct",
        "digest": "sha256:" + hashlib.sha256(direct).hexdigest(),
        "size": len(direct),
    })
    with pytest.raises(ValueError, match="PE architecture mismatch"):
        catalog_from_releases([item], lambda url: direct if url.endswith("direct") else data)


def test_published_prerelease_flag_cannot_displace_stable() -> None:
    data = wheel("clud.exe", b"MZpayload")
    stable = release("2.9.0", "https://example.com/stable", data)
    candidate = release("2.10.0", "https://example.com/candidate", data)
    candidate["prerelease"] = True
    catalog = catalog_from_releases([candidate, stable], lambda _: data)
    assert catalog["channels"]["latest-stable"] == "2.9.0"
    assert [entry["version"] for entry in catalog["releases"]] == ["2.9.0"]


def test_versioned_candidate_catalog_preserves_prior_stable() -> None:
    data = wheel("clud.exe", b"MZpayload")
    stable = catalog_from_releases(
        [release("2.9.0", "https://example.com/stable", data)], lambda _: data
    )
    candidate = release("2.10.0", "https://example.com/candidate", data)
    candidate["prerelease"] = True
    result = candidate_catalog_from_release(candidate, stable, lambda _: data)
    assert result["channels"] == {"latest-stable": "2.9.0", "candidate": "2.10.0"}
    assert [row["version"] for row in result["releases"]] == ["2.10.0", "2.9.0"]
    with pytest.raises(ValueError, match="complete"):
        candidate_catalog_from_release(
            {**candidate, "assets": candidate["assets"][:-1]}, stable, lambda _: data
        )
    with pytest.raises(ValueError, match="prerelease"):
        candidate_catalog_from_release({**candidate, "prerelease": False}, stable, lambda _: data)


def test_incomplete_newer_release_cannot_displace_complete_stable() -> None:
    data = wheel("clud.exe", b"MZpayload")
    stable = release("2.9.0", "https://example.com/stable", data)
    incomplete = release("2.10.0", "https://example.com/incomplete", data)
    incomplete["assets"].pop()
    catalog = catalog_from_releases([incomplete, stable], lambda _: data)
    assert catalog["channels"]["latest-stable"] == "2.9.0"
    assert catalog["releases"][0]["version"] == "2.10.0"


def test_linux_gnu_history_is_classified_as_host_specific() -> None:
    import hashlib

    payload = bytearray(68)
    payload[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", payload, 18, 62)
    payload = bytes(payload)
    windows_wheel = wheel("clud.exe", b"MZpayload")
    item = release("2.9.0", "https://example.com/windows", windows_wheel)
    item["assets"].append(
        {
            "name": "clud-2.9.0-x86_64-unknown-linux-gnu",
            "browser_download_url": "https://example.com/linux",
            "digest": "sha256:" + hashlib.sha256(payload).hexdigest(),
            "size": len(payload),
        }
    )
    catalog = catalog_from_releases(
        [item],
        lambda url: payload if url.endswith("linux") else windows_wheel,
    )
    linux = next(
        entry for entry in catalog["releases"][0]["platforms"]
        if entry["platform"]["os"] == "linux"
    )
    assert linux["platform"]["libc"] == "glibc"
    assert linux["variant"]["flavor"] == "gnu"


def test_linux_musl_and_gnu_variants_remain_distinct() -> None:
    import hashlib
    import struct

    payload = bytearray(128)
    payload[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", payload, 16, 2)
    struct.pack_into("<H", payload, 18, 62)
    struct.pack_into("<Q", payload, 32, 64)
    struct.pack_into("<HH", payload, 54, 56, 1)
    struct.pack_into("<I", payload, 64, 1)  # PT_LOAD, no loader or dependencies
    payload = bytes(payload)
    item = release("2.9.0", "https://example.com/windows", wheel("clud.exe", b"MZpayload"))
    for flavor in ("musl", "gnu"):
        item["assets"].append(
            {
                "name": f"clud-2.9.0-x86_64-unknown-linux-{flavor}",
                "browser_download_url": f"https://example.com/{flavor}",
                "digest": "sha256:" + hashlib.sha256(payload).hexdigest(),
                "size": len(payload),
            }
        )
    catalog = catalog_from_releases(
        [item],
        lambda url: payload if url.endswith(("musl", "gnu")) else wheel("clud.exe", b"MZpayload"),
    )
    linux = [
        entry for entry in catalog["releases"][0]["platforms"]
        if entry["platform"]["os"] == "linux" and entry["platform"]["arch"] == "x86_64"
    ]
    assert len(linux) == 2
    assert {entry["variant"]["flavor"] for entry in linux} == {"static-musl", "gnu"}
    musl = next(entry for entry in linux if entry["variant"]["flavor"] == "static-musl")
    assert musl["platform"] == {
        "os": "linux", "arch": "x86_64"
    }


def test_static_musl_rejects_loader_dependency_and_wrong_architecture() -> None:
    import struct

    payload = bytearray(128)
    payload[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", payload, 16, 2)
    struct.pack_into("<H", payload, 18, 62)
    struct.pack_into("<Q", payload, 32, 64)
    struct.pack_into("<HH", payload, 54, 56, 1)
    struct.pack_into("<I", payload, 64, 1)
    verify_static_musl_elf(bytes(payload), "x86_64")
    with pytest.raises(ValueError, match="architecture"):
        verify_static_musl_elf(bytes(payload), "aarch64")
    struct.pack_into("<I", payload, 64, 3)
    with pytest.raises(ValueError, match="PT_INTERP"):
        verify_static_musl_elf(bytes(payload), "x86_64")
    struct.pack_into("<I", payload, 64, 2)
    struct.pack_into("<Q", payload, 72, 120)
    struct.pack_into("<Q", payload, 96, 16)
    payload.extend(b"\x00" * 16)
    struct.pack_into("<q", payload, 120, 1)
    with pytest.raises(ValueError, match="DT_NEEDED"):
        verify_static_musl_elf(bytes(payload), "x86_64")


def test_public_site_rejects_incomplete_latest(tmp_path) -> None:
    import json

    data = wheel("clud.exe", b"MZpayload")
    build_site(tmp_path, [release("2.9.0", "https://example.com/29", data)], lambda _: data)
    manifest = tmp_path / "install" / "manifest.json"
    catalog = json.loads(manifest.read_text(encoding="utf-8"))
    catalog["releases"][0]["platforms"].pop()
    manifest.write_text(json.dumps(catalog), encoding="utf-8")
    with pytest.raises(ValueError, match="incomplete"):
        verify(tmp_path)


def test_page_does_not_link_unverified_installer_asset(tmp_path) -> None:
    data = wheel("clud.exe", b"MZpayload")
    item = release("2.9.0", "https://example.com/29", data)
    item["assets"].append({"name": "clud-installer.exe"})
    build_site(tmp_path, [item], lambda _: data)
    page = (tmp_path / "install" / "index.html").read_text(encoding="utf-8")
    assert "releases/latest/download/clud-installer.exe" not in page
    assert "No native downloads are available" in page
