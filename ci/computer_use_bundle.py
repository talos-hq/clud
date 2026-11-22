"""Stage the checksum-pinned Computer Use archive for clud release builds."""

import hashlib
import os
from pathlib import Path

from ci.lokr_bundle import atomic_write, download_release

VERSION = "0.57.1"
ASSETS = {
    "darwin-universal.tar.gz": "508f1f103dceb9a62caa88a5ee9bbeeb72fc5825a149b80028ee5acc4451ce26",
    "linux-aarch64.tar.gz": "60895da81cfc421a2ae3f3ea5ad669cf580a52759751d846d4be0d007fd949a8",
    "linux-x86_64.tar.gz": "2bacb5aad2e058197070296967c7c713865085d2a13f3125b53a9f252b2b1af4",
    "windows-aarch64.zip": "ad52d3dbe5aeacdee092f809201988151bc75d24c275c434f1d98314622d31f3",
    "windows-x86_64.zip": "b9028e8d716c05cf54c6a731a4a89285ab1e7ea91afa51f014059cfd66e380fb",
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
