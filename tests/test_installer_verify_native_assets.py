"""The release can publish only its correctly typed native downloads."""

from __future__ import annotations

import struct

import pytest

from installer.verify_native_assets import TARGETS, verify, verify_one


def universal_payload(cpus: tuple[int, ...] = (0x01000007, 0x0100000C)) -> bytes:
    header = bytearray(b"\xca\xfe\xba\xbe" + struct.pack(">I", len(cpus)))
    slices = b""
    offset = 4096
    for cpu in cpus:
        thin = bytearray(64)
        thin[:4] = b"\xcf\xfa\xed\xfe"
        struct.pack_into("<I", thin, 4, cpu)
        header += struct.pack(">iiIII", cpu, 3, offset + len(slices), len(thin), 12)
        slices += bytes(thin)
    return bytes(header.ljust(offset, b"\0")) + slices


def native_payload(kind: str, arch: str) -> bytes:
    if kind == "macho-universal":
        return universal_payload()
    if kind == "udif":
        return b"\0" * 1024 + b"koly" + b"\0" * 508
    payload = bytearray(256)
    if kind == "elf":
        payload[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<HH", payload, 16, 2, {"x86_64": 62, "aarch64": 183}[arch])
        struct.pack_into("<Q", payload, 32, 64)
        struct.pack_into("<HH", payload, 54, 56, 1)
        struct.pack_into("<I", payload, 64, 1)
    elif kind == "pe":
        payload[:2] = b"MZ"
        struct.pack_into("<I", payload, 0x3C, 128)
        payload[128:132] = b"PE\0\0"
        struct.pack_into("<H", payload, 132, {"x86_64": 0x8664, "aarch64": 0xAA64}[arch])
    else:
        payload[:4] = b"\xcf\xfa\xed\xfe"
        struct.pack_into("<I", payload, 4, {"x86_64": 0x01000007, "aarch64": 0x0100000C}[arch])
    return bytes(payload)


def test_native_downloads_have_exact_names_and_architectures(tmp_path) -> None:
    for suffix, (kind, arch) in TARGETS.items():
        (tmp_path / f"clud-2.9.0-{suffix}").write_bytes(native_payload(kind, arch))
    assert sorted(verify(tmp_path, "2.9.0")) == sorted(f"clud-2.9.0-{s}" for s in TARGETS)
    # macOS ships one universal binary and the disk image, never thin binaries.
    assert "universal-apple-darwin" in TARGETS
    assert "macos.dmg" in TARGETS
    assert not any(s.endswith(("x86_64-apple-darwin", "aarch64-apple-darwin")) for s in TARGETS)
    (tmp_path / "clud-2.9.0-aarch64-unknown-linux-musl").write_bytes(
        native_payload("elf", "x86_64")
    )
    with pytest.raises(ValueError, match="architecture"):
        verify(tmp_path, "2.9.0")
    (tmp_path / "clud-2.9.0-aarch64-unknown-linux-musl").rename(tmp_path / "unexpected-asset")
    with pytest.raises(ValueError, match="missing"):
        verify(tmp_path, "2.9.0")


def test_uploaded_arm_binary_is_verified_before_execution(tmp_path) -> None:
    path = tmp_path / "clud-2.9.0-aarch64-unknown-linux-musl"
    path.write_bytes(native_payload("elf", "aarch64"))
    assert verify_one(tmp_path, "2.9.0", "aarch64-unknown-linux-musl")[0] == 256
    path.write_bytes(native_payload("elf", "x86_64"))
    with pytest.raises(ValueError, match="architecture"):
        verify_one(tmp_path, "2.9.0", "aarch64-unknown-linux-musl")


@pytest.mark.parametrize(
    ("payload", "message"),
    [
        (universal_payload((0x01000007,)), "exactly two"),
        (universal_payload((0x01000007, 0x01000007)), "architecture"),
        (native_payload("macho", "aarch64"), "not a fat"),
        (universal_payload()[:4100], "out of bounds"),
    ],
)
def test_universal_macos_download_must_hold_both_slices(tmp_path, payload, message) -> None:
    for suffix, (kind, arch) in TARGETS.items():
        (tmp_path / f"clud-2.9.0-{suffix}").write_bytes(native_payload(kind, arch))
    (tmp_path / "clud-2.9.0-universal-apple-darwin").write_bytes(payload)
    with pytest.raises(ValueError, match=message):
        verify(tmp_path, "2.9.0")


def test_disk_image_needs_the_udif_trailer(tmp_path) -> None:
    for suffix, (kind, arch) in TARGETS.items():
        (tmp_path / f"clud-2.9.0-{suffix}").write_bytes(native_payload(kind, arch))
    (tmp_path / "clud-2.9.0-macos.dmg").write_bytes(b"\0" * 2048)
    with pytest.raises(ValueError, match="koly"):
        verify(tmp_path, "2.9.0")
