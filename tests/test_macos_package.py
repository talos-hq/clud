"""ci/macos_package.py: the universal binary, Install clud.app and its disk image (#2863).

lipo, codesign, hdiutil and spctl run only on the macOS release runners
(``package-macos`` and ``macos-installer-gate``); these tests cover the
pieces that decide what those tools are given.
"""

from __future__ import annotations

import plistlib
import zipfile
from pathlib import Path

import pytest

from ci import macos_package as pkg
from ci.sign_macos_release import SigningError

ARM = b"\xcf\xfa\xed\xfe" + b"\x0c\x00\x00\x01" + b"arm-slice"
X86 = b"\xcf\xfa\xed\xfe" + b"\x07\x00\x00\x01" + b"x86-slice"


def _wheel(directory: Path, tag: str, clud: bytes, extra: dict[str, bytes] | None = None) -> Path:
    path = directory / f"clud-2.8.70-py3-none-{tag}.whl"
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("clud-2.8.70.data/scripts/clud", clud)
        for name, data in (extra or {}).items():
            archive.writestr(name, data)
    return path


def test_names_are_the_published_asset_names() -> None:
    assert pkg.universal_name("2.8.70") == "clud-2.8.70-universal-apple-darwin"
    assert pkg.dmg_name("2.8.70") == "clud-2.8.70-macos.dmg"


def test_info_plist_makes_the_universal_clud_the_bundle_executable() -> None:
    plist = plistlib.loads(pkg.info_plist("2.8.70"))
    assert plist["CFBundleExecutable"] == "clud"
    assert plist["CFBundleIdentifier"] == pkg.BUNDLE_ID
    assert plist["CFBundlePackageType"] == "APPL"
    assert plist["CFBundleShortVersionString"] == plist["CFBundleVersion"] == "2.8.70"
    assert plist["LSMinimumSystemVersion"] == "10.15"
    # No Dock icon: the installer only shows dialogs.
    assert plist["LSUIElement"] is True


def test_thin_executables_take_one_clud_per_architecture(tmp_path) -> None:
    webterm = {"clud-2.8.70.data/scripts/clud-webterm": b"\xcf\xfa\xed\xfe" + b"w"}
    _wheel(tmp_path, "macosx_11_0_arm64", ARM, webterm)
    _wheel(tmp_path, "macosx_10_15_x86_64", X86)
    assert pkg.thin_executables(tmp_path) == {"arm64": ARM, "x86_64": X86}


def test_thin_executables_refuse_a_missing_architecture(tmp_path) -> None:
    _wheel(tmp_path, "macosx_11_0_arm64", ARM)
    with pytest.raises(SigningError, match="one x86_64 and one arm64"):
        pkg.thin_executables(tmp_path)


def test_offline_block_targets_only_the_ticket_service() -> None:
    lines = pkg.hosts_lines().splitlines()
    assert lines == [
        f"127.0.0.1 api.apple-cloudkit.com {pkg.HOSTS_MARKER}",
        f"::1 api.apple-cloudkit.com {pkg.HOSTS_MARKER}",
    ]


def test_installer_environment_is_isolated_to_the_scratch_home(tmp_path) -> None:
    env = pkg.installer_env(tmp_path)
    assert env["HOME"] == str(tmp_path)
    assert env["SHELL"] == "/bin/zsh"
    # No inherited PATH: a fresh login shell must find clud on its own.
    assert env["PATH"] == "/usr/bin:/bin:/usr/sbin:/sbin"


def test_readme_names_the_rescue_routes() -> None:
    assert "Open Anyway" in pkg.README
    assert "install.sh" in pkg.README
    assert "--installer" in pkg.README


def test_main_reports_signing_errors_without_a_traceback(tmp_path, capsys, monkeypatch) -> None:
    for name in ("MACOS_SIGNING_CERT", "APPLE_NOTARY_USER"):
        monkeypatch.delenv(name, raising=False)
    code = pkg.main(
        ["build", "--dist-dir", str(tmp_path), "--out-dir", str(tmp_path / "o"), "--version", "1"]
    )
    assert code == 1
    assert "missing macOS signing secret" in capsys.readouterr().err
