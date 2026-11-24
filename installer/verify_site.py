"""Verify that generated Pages match the catalog's selected stable release."""

from __future__ import annotations

import json
import sys
from html.parser import HTMLParser
from pathlib import Path

from installer.catalog import TARGETS
from installer.site import CATALOG_HREF, DETECT_SCRIPT, download_path, select_native_downloads


class Links(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.hrefs: list[str] = []
        self.native: list[dict[str, str]] = []
        self.options: list[tuple[str, str]] = []
        self._current: dict[str, str] | None = None

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag == "option":
            values = dict(attrs)
            if value := values.get("value"):
                self.options.append((value, values.get("data-download-url") or ""))
        if tag == "a":
            values = dict(attrs)
            href = values.get("href")
            if href:
                self.hrefs.append(href)
            if "native-download" in (values.get("class") or "").split():
                self._current = {
                    "url": href or "",
                    "os": values.get("data-os") or "",
                    "arch": values.get("data-arch") or "",
                    "flavor": values.get("data-flavor") or "",
                    "filename": "",
                }

    def handle_data(self, data: str) -> None:
        if self._current is not None:
            self._current["filename"] += data

    def handle_endtag(self, tag: str) -> None:
        if tag == "a" and self._current is not None:
            self.native.append(self._current)
            self._current = None


def verify_download_links(page_links: Links, catalog: dict, version: str) -> None:
    downloads = select_native_downloads(catalog)
    local = [{**row, "url": download_path(row)} for row in downloads]
    # The optional universal disk image is verified by site.macos_installer.
    # It is served by GitHub Releases and is absent from the v1 catalog.
    if page_links.native and page_links.native[0]["flavor"] == "installer":
        installer = page_links.native[0]
        filename = f"clud-{version}-macos.dmg"
        base = "https://github.com/talos-hq/clud/releases/download"
        urls = {f"{base}/{tag}/{filename}" for tag in (version, f"v{version}")}
        if installer["url"] not in urls or installer != {
            "os": "darwin",
            "arch": "universal",
            "flavor": "installer",
            "filename": filename,
            "url": installer["url"],
        }:
            raise ValueError(f"invalid macOS installer download link: {installer}")
        local.insert(0, installer)
        downloads.insert(0, installer)
    if page_links.native != local:
        raise ValueError(f"native download links differ from catalog: {page_links.native}")
    options = [(row["url"], link["url"]) for row, link in zip(downloads, local, strict=True)]
    if page_links.options != options:
        raise ValueError(f"platform dropdown differs from catalog: {page_links.options}")
    allowed_links = {row["url"] for row in local}
    allowed_links.update((CATALOG_HREF, "https://support.apple.com/en-us/102445"))
    for href in page_links.hrefs:
        if href not in allowed_links:
            raise ValueError(f"page link is absent from catalog or site guidance: {href}")


def verify(site: Path) -> str:
    root = (site / "index.html").read_text(encoding="utf-8")
    page = (site / "install" / "index.html").read_text(encoding="utf-8")
    catalog = json.loads((site / "install" / "manifest.json").read_text(encoding="utf-8"))
    if root != page:
        raise ValueError("root must serve the same download page as install/index.html")
    expected = catalog["channels"]["latest-stable"]
    entry = next((item for item in catalog["releases"] if item["version"] == expected), None)
    if entry is None:
        raise ValueError("latest stable release is absent from the catalog")
    actual_targets = {
        (item["platform"]["os"], item["platform"]["arch"]) for item in entry["platforms"]
    }
    required_targets = {(os_name, arch) for os_name, arch, _ in TARGETS.values()}
    if actual_targets != required_targets:
        raise ValueError(f"latest stable release is incomplete: {actual_targets}")
    variants = [
        (
            item["platform"]["os"],
            item["platform"]["arch"],
            item["platform"].get("libc"),
            item.get("variant", {}).get("flavor"),
        )
        for item in entry["platforms"]
    ]
    if len(variants) != len(set(variants)):
        raise ValueError("latest stable release has duplicate platform variants")
    static_arches = {
        arch
        for os_name, arch, _libc, flavor in variants
        if os_name == "linux" and flavor == "static-musl"
    }
    if static_arches and static_arches != {"x86_64", "aarch64"}:
        raise ValueError(
            f"latest stable release has incomplete static musl assets: {static_arches}"
        )
    if expected not in page:
        raise ValueError("landing page does not show catalog latest")
    page_links = Links()
    page_links.feed(page)
    verify_download_links(page_links, catalog, expected)
    if page_links.native and DETECT_SCRIPT not in page:
        raise ValueError("download page lacks the platform detection script")
    return expected


if __name__ == "__main__":
    print(verify(Path(sys.argv[1])))
