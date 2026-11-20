"""Check the direct release downloads before hashing and publication.

macOS ships one universal binary and the stapled installer disk image
(#2863) instead of two thin binaries; the Pages catalog keeps resolving macOS
to the per-architecture wheels, which every installed clud already accepts.
"""

from __future__ import annotations

import hashlib
import struct
import sys
from pathlib import Path

from installer.catalog import verify_direct_executable

TARGETS = {
    "x86_64-pc-windows-msvc.exe": ("pe", "x86_64"),
    "aarch64-pc-windows-msvc.exe": ("pe", "aarch64"),
    "universal-apple-darwin": ("macho-universal", "universal"),
    "macos.dmg": ("udif", "universal"),
    "x86_64-unknown-linux-musl": ("elf", "x86_64"),
    "aarch64-unknown-linux-musl": ("elf", "aarch64"),
}
FAT_MAGIC = b"\xca\xfe\xba\xbe"
MACHO_CPUS = {0x01000007: "x86_64", 0x0100000C: "aarch64"}
UDIF_TRAILER = 512


def verify_universal_macho(data: bytes) -> None:
    """A fat Mach-O with exactly one x86_64 and one arm64 slice, in bounds."""
    if len(data) < 8 or data[:4] != FAT_MAGIC:
        raise ValueError("universal asset is not a fat Mach-O")
    count = struct.unpack_from(">I", data, 4)[0]
    if count != 2 or len(data) < 8 + 20 * count:
        raise ValueError("universal Mach-O must hold exactly two slices")
    archs = []
    for index in range(count):
        cpu, _sub, offset, size, _align = struct.unpack_from(">iiIII", data, 8 + 20 * index)
        if offset + size > len(data) or data[offset : offset + 4] != b"\xcf\xfa\xed\xfe":
            raise ValueError("universal Mach-O slice is out of bounds or not Mach-O 64")
        archs.append(MACHO_CPUS.get(cpu & 0xFFFFFFFF, hex(cpu)))
    if sorted(archs) != ["aarch64", "x86_64"]:
        raise ValueError(f"universal Mach-O architecture mismatch: {archs}")


def verify_udif(data: bytes) -> None:
    """An Apple disk image ends in a 512-byte ``koly`` trailer."""
    if len(data) < UDIF_TRAILER or data[-UDIF_TRAILER:][:4] != b"koly":
        raise ValueError("macOS disk image lacks the UDIF koly trailer")


def verify_format(data: bytes, kind: str, arch: str) -> None:
    if kind == "macho-universal":
        verify_universal_macho(data)
        return
    if kind == "udif":
        verify_udif(data)
        return
    os_name = {"elf": "linux", "pe": "windows", "macho": "darwin"}.get(kind)
    if os_name is None:
        raise ValueError(f"unknown native format: {kind}")
    verify_direct_executable(data, os_name, arch, "static-musl" if kind == "elf" else None)


def verify(directory: Path, version: str) -> dict[str, tuple[int, str]]:
    expected = {
        f"clud-{version}-{suffix}": (kind, arch)
        for suffix, (kind, arch) in TARGETS.items()
    }
    actual = {path.name for path in directory.iterdir()}
    if actual != set(expected):
        missing = set(expected) - actual
        extra = actual - set(expected)
        raise ValueError(f"native asset set mismatch: missing={missing}, extra={extra}")
    results = {}
    for name, (kind, arch) in expected.items():
        path = directory / name
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"native asset is not a regular file: {name}")
        data = path.read_bytes()
        verify_format(data, kind, arch)
        results[name] = len(data), hashlib.sha256(data).hexdigest()
        print(f"{name}: {results[name][0]} bytes sha256={results[name][1]} arch={arch}")
    return results


def verify_one(directory: Path, version: str, target: str) -> tuple[int, str]:
    """Verify the exact uploaded musl artifact before native ARM execution."""
    if target not in ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"):
        raise ValueError(f"unsupported standalone target: {target}")
    path = directory / f"clud-{version}-{target}"
    if not path.is_file() or path.is_symlink():
        raise ValueError(f"missing standalone asset: {path.name}")
    data = path.read_bytes()
    verify_format(data, "elf", TARGETS[target][1])
    digest = hashlib.sha256(data).hexdigest()
    print(f"{path.name}: {len(data)} bytes sha256={digest}")
    return len(data), digest


if __name__ == "__main__":
    if len(sys.argv) == 4:
        verify_one(Path(sys.argv[2]), sys.argv[1], sys.argv[3])
    else:
        verify(Path(sys.argv[2]), sys.argv[1])
