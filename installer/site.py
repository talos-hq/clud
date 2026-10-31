"""Render the GitHub Pages installer landing page from a validated catalog."""

from __future__ import annotations

import html
import json
from pathlib import Path

from installer.catalog import direct_target, linux_variant, write_catalog

INSTALLER_SITE_FILES = ("index.html", "install/index.html", "install/manifest.json")
STATIC_ASSETS: dict[str, bytes] = {}


def published_paths() -> tuple[str, ...]:
    """Every installer-owned Pages file that a model-only deploy must retain."""
    return (*INSTALLER_SITE_FILES, *STATIC_ASSETS)

ROOT = """<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta http-equiv="refresh" content="0;url=/clud/install/index.html">
<title>Install clud</title></head><body>
<a href="/clud/install/manifest.json">Installer manifest</a>
<script>location.replace('/clud/install/index.html')</script>
</body></html>
"""


def select_native_downloads(catalog: dict) -> list[dict[str, str]]:  # noqa: C901
    """Choose verified native assets from the catalog's selected stable release."""
    version = catalog["channels"]["latest-stable"]
    release = next(
        (item for item in catalog["releases"] if item["version"] == version), None
    )
    if release is None:
        raise ValueError("latest-stable release is absent from the catalog")
    chosen: dict[tuple[str, str], dict[str, str]] = {}
    for item in release["platforms"]:
        asset = item.get("asset")
        if not isinstance(asset, dict) or asset.get("media_type") != "application/octet-stream":
            continue
        filename = asset.get("filename")
        if not isinstance(filename, str):
            continue
        target = direct_target(filename, version)
        if target is None:
            continue
        os_name, arch, _executable = target
        extra, variant = linux_variant(filename, os_name)
        if item["platform"] != {"os": os_name, "arch": arch, **extra}:
            raise ValueError(f"native asset platform disagrees with filename: {filename}")
        if item.get("variant", {}) != variant:
            raise ValueError(f"native asset variant disagrees with filename: {filename}")
        urls = asset.get("urls")
        if not isinstance(urls, list) or len(urls) != 1 or not isinstance(urls[0], str):
            raise ValueError(f"native asset lacks one verified URL: {filename}")
        row = {
            "os": os_name,
            "arch": arch,
            "flavor": variant.get("flavor", "native"),
            "filename": filename,
            "url": urls[0],
        }
        key = (os_name, arch)
        previous = chosen.get(key)
        if previous is None:
            chosen[key] = row
        elif previous["flavor"] == row["flavor"]:
            raise ValueError(f"duplicate native asset for {os_name}/{arch}")
        elif row["flavor"] == "static-musl":
            chosen[key] = row
    order = {"windows": 0, "darwin": 1, "linux": 2}
    return sorted(chosen.values(), key=lambda row: (order[row["os"]], row["arch"]))


def render_page(catalog: dict) -> str:
    latest = html.escape(catalog["channels"]["latest-stable"])
    downloads = select_native_downloads(catalog)
    rows = []
    instructions = []
    labels = {"windows": "Windows", "darwin": "macOS", "linux": "Linux"}
    for row in downloads:
        platform = f"{labels[row['os']]} {row['arch']}"
        if row["os"] == "linux":
            platform += " (static musl)" if row["flavor"] == "static-musl" else " (glibc)"
        url = html.escape(row["url"], quote=True)
        filename = html.escape(row["filename"])
        rows.append(
            f'<li>{html.escape(platform)}: <a class="native-download" '
            f'data-os="{row["os"]}" data-arch="{row["arch"]}" '
            f'data-flavor="{row["flavor"]}" href="{url}">{filename}</a></li>'
        )
        if row["os"] == "windows":
            command = f'& "$HOME\\Downloads\\{row["filename"]}" --installer'
            instructions.append(
                f"<li>{html.escape(platform)}: open PowerShell and run "
                f"<code>{html.escape(command)}</code>.</li>"
            )
        else:
            path = f'"$HOME/Downloads/{row["filename"]}"'
            command = f"chmod +x {path} && {path} --installer"
            instructions.append(
                f"<li>{html.escape(platform)}: open a terminal and run "
                f"<code>{html.escape(command)}</code>.</li>"
            )
    if rows:
        download_section = "<ul>" + "".join(rows) + "</ul>"
        launch_section = "<ul>" + "".join(instructions) + "</ul>"
    else:
        download_section = "<p>No native downloads are available for this stable release.</p>"
        launch_section = ""
    mac_help = ""
    if any(row["os"] == "darwin" for row in downloads):
        mac_help = (
            "<p>If macOS blocks the downloaded executable, try the terminal command first. "
            "If you trust the download, open System Settings &gt; Privacy &amp; Security, "
            "choose Open Anyway, then confirm Open. "
            '<a href="https://support.apple.com/en-us/102445">Apple’s instructions</a>. '
            "This page does not promise Finder double-click launch.</p>"
        )
    return f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Install clud</title></head>
<body><main><h1>Install clud</h1>
<p>Latest stable release: {latest}</p>
<h2>Native downloads</h2>
{download_section}
<h2>Run the download</h2>
{launch_section}
{mac_help}
<p>Run the downloaded program with <code>--installer</code> to choose a release, or
<code>--installer --install-current</code> to install these same bytes. It shows the
destination and asks before changing files. A bare interactive first run offers
installation only when no <code>clud</code> executable is found on PATH.</p>
<p><a href="manifest.json">View the release catalog</a></p>
</main></body></html>
"""


def build_site(
    destination: Path,
    releases: list[dict],
    fetch_bytes,
    previous_catalog: dict | None = None,
) -> dict:
    catalog = write_catalog(
        destination / "install" / "manifest.json",
        releases,
        fetch_bytes,
        verified_catalog=previous_catalog,
    )
    (destination / "index.html").write_text(ROOT, encoding="utf-8")
    (destination / ".nojekyll").write_text("", encoding="utf-8")
    (destination / "install" / "index.html").write_text(
        render_page(catalog), encoding="utf-8"
    )
    for relative, body in STATIC_ASSETS.items():
        asset = destination / relative
        asset.parent.mkdir(parents=True, exist_ok=True)
        asset.write_bytes(body)
    return catalog


def main() -> None:
    import argparse

    from installer.catalog import INSTALL_CATALOG_URLS, fetch, fetch_first, published_releases

    parser = argparse.ArgumentParser()
    parser.add_argument("destination", type=Path)
    parser.add_argument(
        "--latest-only",
        action="store_true",
        help="PR smoke check using the currently published latest release",
    )
    args = parser.parse_args()
    releases = published_releases()
    if args.latest_only:
        releases = [
            next(item for item in releases if not item.get("draft") and not item.get("prerelease"))
        ]
    previous_catalog = None
    try:
        previous_catalog = json.loads(fetch_first(INSTALL_CATALOG_URLS))
        print("Found the previous published catalog; unchanged assets will reuse its verification.")
    except (OSError, ValueError):
        print("No previous published catalog is available; verifying release assets from scratch.")
    catalog = build_site(args.destination, releases, fetch, previous_catalog)
    print(
        json.dumps(
            {
                "latest-stable": catalog["channels"]["latest-stable"],
                "versions": len(catalog["releases"]),
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
