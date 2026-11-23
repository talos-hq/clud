"""Stage the checksum-pinned Computer Use archive for clud release builds."""

import hashlib
import os
from pathlib import Path

from ci.lokr_bundle import atomic_write, download_release

VERSION = "0.57.2"
ASSETS = {
    "darwin-universal.tar.gz": "9ecfbba8c1b13100bf1f2a869c1d2a0ecbd5a326555fcc394c9a3581747d6a7d",
    "linux-aarch64.tar.gz": "cffe922c0d9ca6e281cbd358cc21677f1fbb7beb80c488234521d5a50fbd060e",
    "linux-x86_64.tar.gz": "529857b9ebcb1a22e4b7450f2f7e935e96374df2c60385a9d84115e5e15002e8",
    "windows-aarch64.zip": "cd199f5d2fb89b3a157b8b304f5db05c08ed9517af560f6d027092fea1185135",
    "windows-x86_64.zip": "919309cf98213c341c24810ff65e7441b5cb02aaa83f7587aec429620a48a997",
}


def asset_for_target(target: str) -> str:
    arch = target.split("-")[0]
    if target.endswith("-apple-darwin"):
        return "darwin-universal.tar.gz"
    if "windows" in target:
        return f"windows-{arch}.zip"
    if "linux" in target:
        return f"linux-{arch}.tar.gz"
    raise ValueError(f"No Computer Use archive for {target}")


def prepare_build(target: str, cache: Path) -> None:
    asset = asset_for_target(target)
    digest = ASSETS[asset]
    filename = f"computer-use-{VERSION}-{asset}"
    url = f"https://github.com/talos-hq/computer-use/releases/download/computer-use-v{VERSION}/{filename}"
    destination = cache / "computer-use" / filename
    if not destination.is_file() or hashlib.sha256(destination.read_bytes()).hexdigest() != digest:
        payload = download_release(url)
        if hashlib.sha256(payload).hexdigest() != digest:
            raise ValueError(f"Invalid SHA-256 for {filename}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        atomic_write(destination, payload)
    os.environ["CLUD_COMPUTER_USE_ARCHIVE"] = str(destination.resolve())
