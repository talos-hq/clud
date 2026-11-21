"""Extract the Windows clud executables from the release wheels for direct download.

macOS direct downloads are the universal binary and installer disk image
``ci.macos_package`` builds (#2863); Linux ships the static musl builds.
"""

from __future__ import annotations

import argparse
from pathlib import Path
from zipfile import ZipFile

from installer.catalog import DIRECT_TARGETS, TARGETS, executable_member, wheel_target


def extract(
    wheels: Path,
    output: Path,
    version: str,
) -> list[Path]:
    output.mkdir(parents=True, exist_ok=True)
    results = []
    found = set()
    for wheel_path in sorted(wheels.glob("*.whl")):
        target = wheel_target(wheel_path.name, version)
        if target is None:
            continue
        os_name, arch, executable = target
        if os_name != "windows":
            continue
        candidates = [suffix for suffix, mapped in DIRECT_TARGETS.items() if mapped == target]
        if len(candidates) != 1 or (os_name, arch) in found:
            raise ValueError(f"ambiguous target in {wheel_path.name}")
        found.add((os_name, arch))
        with ZipFile(wheel_path) as archive:
            member = executable_member(wheel_path.read_bytes(), executable)
            payload = archive.read(member)
        destination = output / f"clud-{version}-{candidates[0]}"
        destination.write_bytes(payload)
        destination.chmod(0o755)
        results.append(destination)
    expected = sum(os_name == "windows" for os_name, _, _ in TARGETS.values())
    if len(results) != expected:
        raise ValueError(f"expected {expected} Windows wheel targets, found {len(results)}")
    return results


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("version")
    parser.add_argument("wheels", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    for path in extract(args.wheels, args.output, args.version):
        print(path)


if __name__ == "__main__":
    main()
