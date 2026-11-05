"""Reclaim unused SDKs on disposable GitHub-hosted Linux build runners only."""

from __future__ import annotations

import os
import shutil
from collections.abc import Mapping
from pathlib import Path

# clud builds desktop Rust/Python targets, never Android or .NET. Keep this
# allowlist fixed; never accept caller-supplied cleanup paths.
UNUSED_SDKS = (Path("/usr/local/lib/android"), Path("/usr/share/dotnet"))


def reclaim(environ: Mapping[str, str]) -> list[Path]:
    """Leave developer machines and self-hosted runners untouched."""
    expected = {
        "GITHUB_ACTIONS": "true",
        "RUNNER_ENVIRONMENT": "github-hosted",
        "RUNNER_OS": "Linux",
    }
    if any(environ.get(key) != value for key, value in expected.items()):
        print("SDK reclamation skipped: not a disposable GitHub-hosted Linux runner")
        return []
    before = shutil.disk_usage("/").free
    removed = []
    for sdk in UNUSED_SDKS:
        if sdk.is_symlink():
            print(f"Leaving SDK symlink untouched: {sdk}")
        elif sdk.is_dir():
            shutil.rmtree(sdk)
            removed.append(sdk)
            print(f"Removed unused hosted SDK: {sdk}")
    after = shutil.disk_usage("/").free
    print(f"Hosted build disk: {before:,} -> {after:,} free bytes")
    return removed


if __name__ == "__main__":
    reclaim(os.environ)
