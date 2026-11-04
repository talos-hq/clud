"""Render the GitHub Pages installer landing page from a validated catalog."""

from __future__ import annotations

import html
import json
from pathlib import Path

from installer.catalog import direct_target, linux_variant, write_catalog

INSTALLER_SITE_FILES = ("index.html", "install/index.html", "install/manifest.json")
_HERE = Path(__file__).resolve().parent
DETECT_SCRIPT = (_HERE / "download.js").read_text(encoding="utf-8").strip()
STATIC_ASSETS: dict[str, bytes] = {"install.sh": (_HERE / "install.sh").read_bytes()}
ONE_LINER = "curl -fsSL https://talosbot.ai/clud/install.sh | sh"
CATALOG_HREF = "/clud/install/manifest.json"
STYLE = """
body{font:16px/1.5 system-ui,-apple-system,sans-serif;margin:0;color:#1b1f24;background:#f6f8fa}
main{max-width:46rem;margin:0 auto;padding:1.25rem}
.card{background:#fff;border:1px solid #d0d7de;border-radius:10px;
padding:1rem 1.25rem;margin:1rem 0}
.button{display:inline-block;background:#1f6feb;color:#fff;text-decoration:none;padding:.75rem 1rem;
border-radius:8px;font-weight:600;word-break:break-all;max-width:100%;box-sizing:border-box}
.button[hidden]{display:none}
select,button{font:inherit;padding:.5rem;max-width:100%;border-radius:6px;border:1px solid #8c959f}
.cmd{display:flex;gap:.5rem;align-items:stretch;flex-wrap:wrap}
.cmd code{flex:1 1 16rem;background:#0d1117;color:#e6edf3;padding:.6rem .75rem;border-radius:6px;
overflow-x:auto;white-space:nowrap}
code{word-break:break-all}
li{margin:.35rem 0;overflow-wrap:anywhere}
"""


def published_paths() -> tuple[str, ...]:
    """Every installer-owned Pages file that a model-only deploy must retain."""
    return (*INSTALLER_SITE_FILES, *STATIC_ASSETS)


def select_native_downloads(catalog: dict) -> list[dict[str, str]]:  # noqa: C901
    """Choose verified native assets from the catalog's selected stable release."""
    version = catalog["channels"]["latest-stable"]
    release = next((item for item in catalog["releases"] if item["version"] == version), None)
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
    options = []
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
        options.append(
            f'<option value="{url}" data-os="{row["os"]}" data-arch="{row["arch"]}" '
            f'data-filename="{filename}">{html.escape(platform)}: {filename}</option>'
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
        download_section = (
            '<p><label for="platform-select">Choose a platform:</label><br>'
            '<select id="platform-select"><option value="">Select your system\u2026</option>'
            + "".join(options)
            + "</select></p>"
            "<h3>All downloads</h3><ul>" + "".join(rows) + "</ul>"
        )
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
    one_liner = html.escape(ONE_LINER)
    return f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Install clud</title><style>{STYLE}</style></head>
<body><main><h1>Install clud</h1>
<p>Latest stable release: {latest}</p>
<section class="card"><h2>Recommended download</h2>
<p id="detected">Detecting your system\u2026 If nothing appears, choose your download below.</p>
<p><a id="recommended" class="button" hidden>Download</a></p>
{download_section}
</section>
<section class="card"><h2>macOS and Linux: one-line install</h2>
<div class="cmd"><code id="one-liner">{one_liner}</code>
<button id="copy-one-liner" type="button">Copy</button></div>
<p>Downloads the latest release for your system, verifies it against the release's
<code>SHA256SUMS</code>, and installs it to <code>~/.local/bin</code>
(override with <code>CLUD_INSTALL_DIR</code>). Add <code>sh -s -- --dry-run</code> to see
what it would do, or <code>--version &lt;tag&gt;</code> to pin a release. Then run
<code>clud setup</code>. Windows: use the <code>.exe</code> download above.</p>
</section>
<section class="card"><h2>Run the download</h2>
{launch_section}
{mac_help}
<p>Run the downloaded program with <code>--installer</code> to choose a release, or
<code>--installer --install-current</code> to install these same bytes. It shows the
destination and asks before changing files. A bare interactive first run offers
installation only when no <code>clud</code> executable is found on PATH.</p>
</section>
<p><a href="{CATALOG_HREF}">View the release catalog</a></p>
</main>
<script>
{DETECT_SCRIPT}
</script>
</body></html>
"""


def root_page(catalog: dict) -> str:
    """The site root serves the same download page as ``install/index.html``."""
    return render_page(catalog)


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
    (destination / "index.html").write_text(root_page(catalog), encoding="utf-8")
    (destination / ".nojekyll").write_text("", encoding="utf-8")
    (destination / "install" / "index.html").write_text(render_page(catalog), encoding="utf-8")
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
