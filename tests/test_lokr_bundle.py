"""Pinned lokr distribution never consults crates.io or executes a prompt."""

import hashlib
import json
import os
from pathlib import Path

import pytest

from ci import lokr_bundle


def test_published_pin_covers_every_clud_release_target() -> None:
    from ci.ci_matrix import TARGETS

    manifest = json.loads(lokr_bundle.SPEC.read_text(encoding="utf-8"))
    targets = {target.triple for target in TARGETS}
    targets.update(
        target.removesuffix("-gnu") + "-musl"
        for target in tuple(targets)
        if target.endswith("-linux-gnu")
    )
    assert set(manifest["artifacts"]) == targets
    for target in targets:
        lokr_bundle.artifact(manifest, target)


def test_build_entry_prepares_lokr_before_compiling(monkeypatch) -> None:
    from ci import xbuild

    calls = []
    monkeypatch.setattr(xbuild, "cross_toolchain_preflight", lambda _: None)
    monkeypatch.setattr(
        lokr_bundle, "prepare_build", lambda target, *, embed: calls.append((target, embed))
    )
    monkeypatch.setattr(xbuild, "cmd_compile", lambda _: calls.append("compile") or 0)
    assert xbuild.main(["compile", "--target", "x86_64-unknown-linux-gnu"]) == 0
    assert calls == [("x86_64-unknown-linux-gnu", True), "compile"]


@pytest.mark.parametrize(
    ("command", "embed"),
    [("compile", False), ("wheel", False), ("standalone", True), ("candidate", True)],
)
def test_release_distribution_policy(command: str, embed: bool, monkeypatch) -> None:
    from ci import xbuild

    calls = []
    monkeypatch.setattr(xbuild, "cross_toolchain_preflight", lambda _: None)
    monkeypatch.setattr(xbuild, f"cmd_{command}", lambda _: 0)
    monkeypatch.setattr(
        lokr_bundle, "prepare_build", lambda target, *, embed: calls.append((target, embed))
    )
    assert (
        xbuild.main([command, "--target", "x86_64-unknown-linux-gnu", "--profile", "release"]) == 0
    )
    assert calls == [("x86_64-unknown-linux-gnu", embed)]


def spec(payload: bytes) -> dict:
    return {
        "version": "0.1.0",
        "tag": "v0.1.0",
        "artifacts": {
            "x86_64-unknown-linux-gnu": {
                "filename": "lokr-x86_64-unknown-linux-gnu",
                "url": f"{lokr_bundle.ORIGIN}/v0.1.0/lokr-x86_64-unknown-linux-gnu",
                "sha256": hashlib.sha256(payload).hexdigest(),
                "size_bytes": len(payload),
            }
        },
    }


def test_pinned_binary_is_verified_and_reused_without_network(tmp_path: Path) -> None:
    payload = b"fake native lokr fixture; never execute"
    calls = []

    def download(url: str) -> bytes:
        calls.append(url)
        return payload

    manifest = spec(payload)
    env = lokr_bundle.prepare({}, "x86_64-unknown-linux-gnu", manifest, tmp_path, download=download)
    assert Path(env["CLUD_LOKR_BINARY"]).read_bytes() == payload
    assert env["CLUD_LOKR_SHA256"] == hashlib.sha256(payload).hexdigest()
    assert env["CLUD_LOKR_VERSION"] == "0.1.0"
    assert calls == [
        "https://github.com/forge-ci/lokr/releases/download/v0.1.0/lokr-x86_64-unknown-linux-gnu"
    ]
    lokr_bundle.prepare({}, "x86_64-unknown-linux-gnu", manifest, tmp_path, download=download)
    assert len(calls) == 1


def test_release_wheels_prepare_only_verified_download_metadata(
    tmp_path: Path, monkeypatch
) -> None:
    manifest = spec(b"fixture")
    pin = tmp_path / "pin.json"
    pin.write_text(json.dumps(manifest))
    monkeypatch.setattr(lokr_bundle, "SPEC", pin)
    monkeypatch.setattr(lokr_bundle, "ROOT", tmp_path)
    monkeypatch.setattr(
        lokr_bundle, "download_release", lambda _: pytest.fail("release wheels must stay small")
    )
    for prefix in ("", "FALLBACK_"):
        for field in ("BINARY", "VERSION", "SHA256", "URL", "SIZE", "FETCH_ONLY"):
            monkeypatch.setenv(f"CLUD_LOKR_{prefix}{field}", "stale native bundle")
    lokr_bundle.prepare_build("x86_64-unknown-linux-gnu", embed=False)
    assert "CLUD_LOKR_BINARY" not in os.environ
    assert os.environ["CLUD_LOKR_FETCH_ONLY"] == "1"
    assert os.environ["CLUD_LOKR_VERSION"] == "0.1.0"
    assert os.environ["CLUD_LOKR_SIZE"] == str(len(b"fixture"))
    assert os.environ["CLUD_LOKR_URL"] == manifest["artifacts"]["x86_64-unknown-linux-gnu"]["url"]
    assert not (tmp_path / "target/lokr-bundle").exists()


def test_digest_failure_never_publishes_binary(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="digest"):
        lokr_bundle.prepare(
            {},
            "x86_64-unknown-linux-gnu",
            spec(b"expected"),
            tmp_path,
            download=lambda _: b"tampered",
        )
    assert not list(tmp_path.rglob("lokr"))


def test_nonfunctional_crates_release_and_unknown_target_fail_closed(tmp_path: Path) -> None:
    manifest = spec(b"fixture")
    manifest["version"] = "0.0.1"
    with pytest.raises(ValueError, match="functional"):
        lokr_bundle.prepare({}, "x86_64-unknown-linux-gnu", manifest, tmp_path)
    with pytest.raises(ValueError, match="target"):
        lokr_bundle.prepare({}, "missing-target", spec(b"fixture"), tmp_path)


def test_untrusted_asset_name_cannot_change_release_origin(tmp_path: Path) -> None:
    manifest = spec(b"fixture")
    manifest["artifacts"]["x86_64-unknown-linux-gnu"]["filename"] = "../evil"
    with pytest.raises(ValueError, match="filename"):
        lokr_bundle.prepare({}, "x86_64-unknown-linux-gnu", manifest, tmp_path)


def test_untrusted_release_url_is_rejected_before_download(tmp_path: Path) -> None:
    manifest = spec(b"fixture")
    manifest["artifacts"]["x86_64-unknown-linux-gnu"]["url"] = "https://example.invalid/lokr"
    with pytest.raises(ValueError, match="release URL"):
        lokr_bundle.prepare(
            {}, "x86_64-unknown-linux-gnu", manifest, tmp_path, download=lambda _: b"fixture"
        )


def test_static_clud_contains_desktop_gnu_and_musl_fallback(tmp_path: Path, monkeypatch) -> None:
    gnu, musl = b"fake GNU fixture", b"fake musl fixture"
    manifest = spec(gnu)
    item = spec(musl)["artifacts"]["x86_64-unknown-linux-gnu"]
    item["filename"] = "lokr-x86_64-unknown-linux-musl"
    item["url"] = f"{lokr_bundle.ORIGIN}/v0.1.0/{item['filename']}"
    manifest["artifacts"]["x86_64-unknown-linux-musl"] = item
    pin = tmp_path / "pin.json"
    pin.write_text(json.dumps(manifest))
    monkeypatch.setattr(lokr_bundle, "SPEC", pin)
    monkeypatch.setattr(lokr_bundle, "ROOT", tmp_path)
    monkeypatch.setattr(
        lokr_bundle, "download_release", lambda url: musl if url.endswith("musl") else gnu
    )
    for field in ("BINARY", "VERSION", "SHA256", "URL", "SIZE", "FETCH_ONLY"):
        monkeypatch.setenv(f"CLUD_LOKR_{field}", "")
        monkeypatch.setenv(f"CLUD_LOKR_FALLBACK_{field}", "")
    lokr_bundle.prepare_build("x86_64-unknown-linux-musl")
    assert Path(os.environ["CLUD_LOKR_BINARY"]).read_bytes() == gnu
    assert Path(os.environ["CLUD_LOKR_FALLBACK_BINARY"]).read_bytes() == musl
    lokr_bundle.prepare_build("x86_64-unknown-linux-gnu")
    assert "CLUD_LOKR_FALLBACK_BINARY" not in os.environ
