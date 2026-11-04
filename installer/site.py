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
:root{color-scheme:dark;--bg:#0b0e14;--panel:#121722;--line:#232b3a;--text:#e6edf3;--muted:#9aa7b8;
--accent:#3ddc97;--accent-ink:#04120b;--focus:#7cc4ff;
--mono:ui-monospace,SFMono-Regular,Menlo,Consolas,"Liberation Mono",monospace}
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:radial-gradient(60rem 30rem at 50% -10rem,#14301f 0,var(--bg) 70%)
var(--bg);color:var(--text);font:16px/1.6 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
main{max-width:52rem;margin:0 auto;padding:clamp(1.25rem,4vw,3.5rem) clamp(1rem,4vw,2rem) 3rem}
a{color:var(--focus)}
code,.mono{font-family:var(--mono);font-size:.92em;overflow-wrap:anywhere}
.brand{display:flex;align-items:center;gap:.6rem;font-family:var(--mono);font-weight:700;
font-size:1.1rem;letter-spacing:.02em}
.brand .prompt{color:var(--accent)}
.version{margin-left:auto;font-size:.8rem;font-weight:500;color:var(--muted);
border:1px solid var(--line);border-radius:999px;padding:.15rem .7rem}
.hero h1{font-size:clamp(2rem,6vw,3.25rem);line-height:1.1;margin:2.5rem 0 .75rem;
letter-spacing:-.02em}
.hero p{color:var(--muted);font-size:1.1rem;max-width:38rem;margin:0}
.card{background:var(--panel);border:1px solid var(--line);border-radius:14px;
padding:clamp(1.1rem,3vw,1.75rem);margin:1.5rem 0}
.card h2{font-size:1.05rem;margin:0 0 1rem;letter-spacing:.01em}
.chip{display:inline-block;
font-size:.9rem;color:var(--muted);background:#0e131c;border:1px solid var(--line);
border-radius:12px;padding:.35rem .9rem;margin:0 0 1.25rem;line-height:1.45}
.chip::before{content:"";display:inline-block;width:.5rem;height:.5rem;border-radius:50%;
background:var(--accent);margin-right:.55rem;vertical-align:.1em}
.chip .linkish{margin-left:.4rem}
.linkish{font:inherit;color:var(--focus);background:none;border:0;padding:.25rem 0;
min-height:40px;text-decoration:underline;cursor:pointer}
.linkish[hidden]{display:none}
.button{display:flex;flex-direction:column;gap:.15rem;width:100%;background:var(--accent);
color:var(--accent-ink);text-decoration:none;padding:1rem 1.25rem;border-radius:10px;
font-weight:700;font-size:1.1rem;min-height:48px;transition:filter .15s ease}
.button:hover{filter:brightness(1.08)}
.button[hidden]{display:none}
.button .mono{font-weight:500;font-size:.85rem;opacity:.85}
.field{display:grid;gap:.4rem;margin:1.25rem 0 0}
.field label{font-size:.85rem;color:var(--muted)}
select{appearance:none;-webkit-appearance:none;width:100%;min-height:44px;font:inherit;
font-family:var(--mono);font-size:.88rem;color:var(--text);background-color:#0e131c;
background-image:linear-gradient(45deg,transparent 50%,var(--muted) 50%),
linear-gradient(135deg,var(--muted) 50%,transparent 50%);
background-position:calc(100% - 1.3rem) 50%,calc(100% - .95rem) 50%;
background-size:.35rem .35rem;background-repeat:no-repeat;text-overflow:ellipsis;
border:1px solid var(--line);border-radius:10px;padding:.6rem 2.4rem .6rem .9rem}
select:hover{border-color:#34405a}
.cmd{display:flex;gap:.5rem;align-items:stretch;flex-wrap:wrap}
.cmd code{flex:1 1 18rem;min-width:0;background:#06080c;color:var(--text);border:1px solid
var(--line);border-radius:10px;padding:.8rem 1rem;white-space:pre-wrap;word-break:break-all}
.cmd code::before{content:"$ ";color:var(--accent)}
.copy{font:inherit;font-weight:600;min-height:44px;min-width:6.5rem;color:var(--text);
background:#1a2232;border:1px solid var(--line);border-radius:10px;padding:.5rem 1rem;
cursor:pointer;transition:background .15s ease}
.copy:hover{background:#222c40}
.copy[data-copied]{color:var(--accent-ink);background:var(--accent);border-color:var(--accent)}
.muted{color:var(--muted);font-size:.95rem}
details{margin-top:.75rem}
summary{cursor:pointer;min-height:40px;display:flex;align-items:center;gap:.5rem;
color:var(--muted);list-style:none}
summary::-webkit-details-marker{display:none}
summary::before{content:"\\25B8";color:var(--accent);transition:transform .15s ease}
details[open] summary::before{transform:rotate(90deg)}
ul{padding-left:1.1rem}
li{margin:.45rem 0;overflow-wrap:anywhere}
li a{font-family:var(--mono);font-size:.88rem}
footer{color:var(--muted);font-size:.85rem;margin-top:2rem}
:focus-visible{outline:3px solid var(--focus);outline-offset:3px;border-radius:6px}
@media (max-width:480px){body{font-size:15px}.cmd .copy{flex:1 1 100%}}
@media (prefers-reduced-motion:reduce){*{transition:none!important;animation:none!important}}
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
            '<div class="field"><label for="platform-select">Other platforms</label>'
            '<select id="platform-select"><option value="">Select your system\u2026</option>'
            + "".join(options)
            + "</select></div>"
            "<details><summary>All downloads</summary><ul>" + "".join(rows) + "</ul></details>"
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
<meta name="color-scheme" content="dark">
<title>Install clud</title><style>{STYLE}</style></head>
<body><main>
<header class="brand"><span class="prompt">&gt;_</span> clud
<span class="version">v{latest}</span></header>
<section class="hero"><h1>Install clud</h1>
<p>One binary that runs Claude Code and Codex in YOLO mode, with a daemon, loops and
bundled skills. Pick your download, or use the one-liner.</p></section>
<section class="card" aria-labelledby="dl-title"><h2 id="dl-title">Recommended download</h2>
<p id="detected" class="chip" role="status"><span id="detected-text">Detecting your
system\u2026 If nothing appears, choose your download below.</span>
<button id="arch-override" class="linkish" type="button" hidden></button></p>
<a id="recommended" class="button" hidden>Download for your system
<span id="recommended-name" class="mono"></span></a>
{download_section}
</section>
<section class="card" aria-labelledby="sh-title"><h2 id="sh-title">macOS and Linux: one-line
install</h2>
<div class="cmd"><code id="one-liner">{one_liner}</code>
<button id="copy-one-liner" class="copy" type="button">Copy</button></div>
<p class="muted">Downloads the latest release for your system, verifies it against the
release's <code>SHA256SUMS</code>, and installs it to <code>~/.local/bin</code>
(override with <code>CLUD_INSTALL_DIR</code>). Add <code>sh -s -- --dry-run</code> to see
what it would do, or <code>--version &lt;tag&gt;</code> to pin a release. Then run
<code>clud setup</code>. Windows: use the <code>.exe</code> download above.</p>
</section>
<section class="card" aria-labelledby="run-title"><h2 id="run-title">Run the download</h2>
{launch_section}
{mac_help}
<p class="muted">Run the downloaded program with <code>--installer</code> to choose a release,
or <code>--installer --install-current</code> to install these same bytes. It shows the
destination and asks before changing files. A bare interactive first run offers
installation only when no <code>clud</code> executable is found on PATH.</p>
</section>
<footer><a href="{CATALOG_HREF}">View the release catalog</a></footer>
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
