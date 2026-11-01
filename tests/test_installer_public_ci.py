"""Install the anonymous public release bytes on each native host."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import sys
from pathlib import Path, PureWindowsPath
from urllib.request import Request, urlopen

import pytest
from running_process import PseudoTerminalProcess

from ci.installer_candidate import TARGETS
from ci.installer_public_evidence import require_evidence, require_guest_evidence
from installer.catalog import (
    ACCEPTED_ONLINE_URLS,
    ONLINE_URL,
    verify_direct_executable,
    version_key,
)
from tests.installer_windows_shells import assert_windows_shell_activation
from tests.process import run as run_process
from tests.test_installer_native_entry import installer_target  # noqa: F401


@pytest.mark.parametrize("os_name", ["darwin", "windows"])
def test_public_executable_flavor_accepts_catalog_rows_without_variant(os_name: str) -> None:
    row = {
        "platform": {"os": os_name},
        "asset": {"media_type": "application/octet-stream"},
    }
    assert public_executable_flavor(row) is None


def test_public_executable_flavor_preserves_linux_static_musl() -> None:
    assert public_executable_flavor({"variant": {"flavor": "static-musl"}}) == "static-musl"


def test_public_windows_lookup_accepts_mixed_path_separators() -> None:
    destination = PureWindowsPath(r"C:\Users\runneradmin\AppData\Local\Programs\clud\bin\clud.exe")
    found = r"C:\Users\runneradmin\AppData\Local\Programs/clud/bin\clud.exe"
    assert_public_lookup([found, "clud 2.8.19"], destination, "2.8.19", "windows")
    with pytest.raises(AssertionError):
        assert_public_lookup([found, "clud 2.8.18"], destination, "2.8.19", "windows")


def test_prior_discovery_version_ignores_binaries_that_cannot_reach_the_catalog() -> None:
    catalog = {
        "releases": [
            {"version": "2.8.31"},
            {"version": "2.8.36"},
            {"version": "2.8.47"},
            {"version": "2.8.48"},
        ]
    }
    assert prior_discovery_version(catalog, "2.8.49") == "2.8.48"
    # The first release that fetches the catalog's domain has nothing it can
    # reach behind it (2.8.47 hits the github.io redirect), so discovery is
    # unprovable rather than broken.
    assert prior_discovery_version(catalog, "2.8.48") is None
    assert prior_discovery_version(catalog, "2.8.47") is None


def test_older_installer_discovery_records_an_unreachable_prior(tmp_path: Path) -> None:
    # A skipped test produces no evidence row, and the acceptance aggregate then
    # rejects the whole release with "missing public host evidence". Record the
    # gap in the row instead so every host still reports.
    catalog = {"releases": [{"version": "2.8.47"}]}
    evidence = assert_older_installer_discovery(
        catalog,
        "linux",
        "x86_64",
        {},
        tmp_path / "clud",
        "2.8.37",
        "0" * 64,
        tmp_path,
    )
    assert evidence == {
        "unavailable": "no catalog-aware prior release",
        "catalog_move_version": "2.8.48",
        "legacy_host": LEGACY_ONLINE_HOST,
    }


def require_evidence_lane_input(input_var: str, output_var: str, skip_reason: str) -> str:
    """Refuse to let a lane that owes an evidence row vanish.

    A lane whose workflow declared an evidence *output* (`output_var` is set)
    must always emit its row: a skip produces no row, and the acceptance
    aggregate rejects the whole release with "missing public host evidence"
    (run 40478241694 rolled 2.8.37 back to 2.8.31 for exactly that). A missing
    input in such a lane is a workflow defect, so this raises. With no evidence
    output at all there is no row to emit and a skip stays honest.
    """
    value = os.environ.get(input_var)
    if value:
        return value
    if os.environ.get(output_var):
        raise RuntimeError(
            f"{output_var} is set but {input_var} is not: this lane was asked for a host "
            "evidence row and cannot produce one"
        )
    # Raised rather than called: `pytest.skip()` only raises this exception, and
    # a bare call would leave the declared return type unsatisfied.
    raise pytest.skip.Exception(f"no {output_var}: {skip_reason}")


def test_public_release_probe_refuses_to_skip_when_the_lane_owes_evidence(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    # The workflow declares the evidence row it expects from this host. With that
    # row owed but no input to build it, the lane must fail loudly rather than
    # skip: a skip emits no row and the aggregate rejects a healthy release.
    monkeypatch.setenv(
        "CLUD_PUBLIC_EVIDENCE", "public-evidence/x86_64-unknown-linux-musl.json"
    )
    monkeypatch.delenv("CLUD_PUBLIC_RELEASE_TAG", raising=False)
    with pytest.raises(RuntimeError, match="CLUD_PUBLIC_RELEASE_TAG"):
        test_public_release_installs_by_name(None, tmp_path)  # type: ignore[arg-type]


def test_public_release_probe_still_skips_a_local_run_that_owes_no_row(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.delenv("CLUD_PUBLIC_EVIDENCE", raising=False)
    monkeypatch.delenv("CLUD_PUBLIC_RELEASE_TAG", raising=False)
    with pytest.raises(pytest.skip.Exception):
        test_public_release_installs_by_name(None, tmp_path)  # type: ignore[arg-type]


def public_executable_flavor(row: dict) -> str | None:
    return row.get("variant", {}).get("flavor")


def assert_public_lookup(
    lines: list[str], destination: Path | PureWindowsPath, version: str, os_name: str
) -> None:
    assert len(lines) == 2, lines
    if os_name == "windows":
        assert PureWindowsPath(lines[0]) == PureWindowsPath(destination)
    else:
        assert Path(lines[0]) == destination
    assert lines[1] == f"clud {version}"


def public_bytes(url: str, limit: int = 200 * 1024 * 1024) -> bytes:
    if not url.startswith("https://"):
        raise ValueError("public installer URL must use HTTPS")
    request = Request(url, headers={"User-Agent": "clud-public-installer-gate"})
    with urlopen(request, timeout=120) as response:
        if not response.geturl().startswith("https://"):
            raise ValueError("public installer redirected away from HTTPS")
        data = response.read(limit + 1)
    if len(data) > limit:
        raise ValueError("public installer response exceeds size limit")
    return data


def assert_public_latest_install(
    binary: Path,
    env: dict[str, str],
    destination: Path,
    version: str,
    digest: str,
    transcript: Path,
) -> None:
    """Choose the latest-default release in the shipped interactive picker."""
    process = PseudoTerminalProcess([str(binary), "--installer"], env=env, capture=True)
    try:
        process.expect("Install clud", timeout=30)
        process.write("\x1b[B\r")
        process.expect("Choose a clud release", timeout=120)
        process.write("\r")
        process.expect(f"Install clud {version} at this destination?", timeout=30)
        process.write("\x1b[A\r")
        assert process.wait(timeout=120) == 0, process.output_text
        assert "verified fresh name-based lookup" in process.output_text
    finally:
        if process.poll() is None:
            process.kill()
        transcript.parent.mkdir(parents=True, exist_ok=True)
        transcript.write_text(process.output_text, encoding="utf-8")
        process.close()
    assert hashlib.sha256(destination.read_bytes()).hexdigest() == digest
    installed = run_process(
        [str(destination), "--version"],
        env=env,
        capture_output=True,
        timeout=15,
        check=False,
    )
    assert installed.returncode == 0, installed.stderr.decode(errors="replace")
    assert installed.stdout.strip() == f"clud {version}".encode()


# The oldest public release whose installer can still fetch today's catalog.
# 2.8.35 moved the catalog from zackees.github.io to talos-hq.github.io (#2217);
# then the org's Pages custom domain made every talos-hq.github.io URL
# 301-redirect to talosbot.ai, and the installer refuses redirects, so 2.8.35
# through 2.8.47 answer `expected value at line 1 column 1` and exit (#2433).
# 2.8.48 fetches the domain first with a GitHub release fallback (#2449). An
# older binary cannot discover a newer release -- not because discovery broke,
# but because its URL did.
CATALOG_MOVE_VERSION = "2.8.48"
LEGACY_ONLINE_HOST = next(
    url for url in ACCEPTED_ONLINE_URLS if url != ONLINE_URL
).split("/")[2]


def prior_discovery_version(catalog: dict, version: str) -> str | None:
    """Newest public release old enough to be older, new enough to reach today's catalog."""
    eligible = [
        row["version"]
        for row in catalog["releases"]
        if version_key(CATALOG_MOVE_VERSION) <= version_key(row["version"]) < version_key(version)
    ]
    return max(eligible, key=version_key, default=None)


def assert_older_installer_discovery(
    catalog: dict,
    os_name: str,
    arch: str,
    env: dict[str, str],
    destination: Path,
    version: str,
    digest: str,
    directory: Path,
) -> dict:
    """A fixed older public executable must discover and install today's stable."""
    prior_version = prior_discovery_version(catalog, version)
    if prior_version is None:
        # No stable-catalog release is both new enough to reach today's catalog
        # URL and older than this release, so cross-version discovery has nothing
        # it can download from. Record that gap in the evidence instead of
        # skipping: the acceptance aggregate requires one row per host, and a
        # skipped test would silently drop this host from it.
        return {
            "unavailable": "no catalog-aware prior release",
            "catalog_move_version": CATALOG_MOVE_VERSION,
            "legacy_host": LEGACY_ONLINE_HOST,
        }
    assert version_key(prior_version) < version_key(version)
    release = next(row for row in catalog["releases"] if row["version"] == prior_version)
    matches = [
        row for row in release["platforms"]
        if row["platform"]["os"] == os_name
        and row["platform"]["arch"] == arch
        and row["asset"]["media_type"] == "application/octet-stream"
        and (os_name != "linux" or row["variant"] == {"flavor": "static-musl"})
    ]
    assert len(matches) == 1, matches
    asset = matches[0]["asset"]
    payload = public_bytes(asset["urls"][0])
    assert len(payload) == asset["size_bytes"]
    assert hashlib.sha256(payload).hexdigest() == asset["sha256"]
    verify_direct_executable(payload, os_name, arch, public_executable_flavor(matches[0]))
    binary = directory / asset["filename"]
    binary.write_bytes(payload)
    binary.chmod(0o755)
    source = run_process(
        [str(binary), "--version"], env=env, capture_output=True, timeout=15, check=False
    )
    assert source.returncode == 0, source.stderr.decode(errors="replace")
    assert source.stdout.strip() == f"clud {prior_version}".encode()
    transcript = Path(os.environ["CLUD_PUBLIC_EVIDENCE"]).with_suffix(".older.terminal.log")
    assert_public_latest_install(binary, env, destination, version, digest, transcript)
    return {
        "source_version": prior_version,
        "source_sha256": asset["sha256"],
        "source_url": asset["urls"][0],
        "selected_version": version,
        "installed_sha256": digest,
        "transcript": transcript.name,
    }


def test_public_aggregate_rejects_missing_and_changed_host_bytes() -> None:
    tag = "2.10.0"
    expected = {}
    rows = []
    for target, (os_name, arch, _kind, _suffix) in TARGETS.items():
        url = f"https://github.com/talos-hq/clud/releases/download/{tag}/clud-{tag}-{target}"
        digest = "c" * 64 if target == "aarch64-unknown-linux-musl" else "a" * 64
        expected[target] = {"version": tag, "sha256": digest, "size_bytes": 100, "url": url}
        rows.append({
            "mode": "candidate",
            "tag": tag,
            "version": tag,
            "os": os_name,
            "arch": arch,
            "sha256": digest,
            "size_bytes": 100,
            "asset_url": url,
            "resolved_path": "/user/bin/clud",
        })
    require_evidence(rows, expected, "candidate", tag)
    with pytest.raises(ValueError, match="missing"):
        require_evidence(rows[:-1], expected, "candidate", tag)
    with pytest.raises(ValueError, match="differs"):
        require_evidence([*rows[:-1], {**rows[-1], "sha256": "b" * 64}], expected, "candidate", tag)

    nixos = [
        {
            "mode": "candidate", "tag": tag, "version": tag, "host_arch": arch,
            "nix_ld": state,
            "sha256": expected[f"{arch}-unknown-linux-musl"]["sha256"],
            "resolved_path": "/home/alice/.local/bin/clud",
        }
        for arch in ("x86_64", "aarch64")
        for state in ("disabled", "enabled")
    ]
    distros = [
        {
            "mode": "candidate", "tag": tag, "version": tag,
            "host_arch": arch, "distro": distro,
            "sha256": expected[f"{arch}-unknown-linux-musl"]["sha256"],
            "resolved_path": "/home/alice/.local/bin/clud",
        }
        for distro, arch in (
            ("archlinux", "x86_64"), ("alpine", "x86_64"),
            ("ubuntu", "x86_64"), ("ubuntu", "aarch64"),
            ("debian", "x86_64"), ("debian", "aarch64"),
            ("fedora", "x86_64"), ("fedora", "aarch64"),
        )
    ]
    require_guest_evidence(nixos, distros, expected, "candidate", tag)
    for index in range(len(nixos)):
        with pytest.raises(ValueError, match="missing"):
            require_guest_evidence(
                nixos[:index] + nixos[index + 1:], distros, expected, "candidate", tag
            )
    with pytest.raises(ValueError, match="missing"):
        require_guest_evidence([*nixos[:-1], nixos[0]], distros, expected, "candidate", tag)
    for index in range(len(distros)):
        with pytest.raises(ValueError, match="missing"):
            require_guest_evidence(
                nixos, distros[:index] + distros[index + 1:], expected, "candidate", tag
            )
    with pytest.raises(ValueError, match="missing"):
        require_guest_evidence(nixos, [*distros[:-1], distros[0]], expected, "candidate", tag)
    wrong_arch = {**distros[-1], "sha256": expected["x86_64-unknown-linux-musl"]["sha256"]}
    with pytest.raises(ValueError, match="differs"):
        require_guest_evidence(
            nixos,
            [*distros[:-1], wrong_arch],
            expected,
            "candidate",
            tag,
        )
    with pytest.raises(ValueError, match="missing"):
        require_guest_evidence(
            nixos,
            [row for row in distros if row["distro"] != "debian"],
            expected,
            "candidate",
            tag,
        )
    with pytest.raises(ValueError, match="missing"):
        require_guest_evidence(nixos[:-1], distros, expected, "candidate", tag)
    with pytest.raises(ValueError, match="differs"):
        require_guest_evidence(
            nixos,
            [*distros[:-1], {**distros[-1], "sha256": "b" * 64}],
            expected,
            "candidate",
            tag,
        )


def test_public_release_installs_by_name(installer_target, tmp_path: Path) -> None:  # noqa: F811
    tag = require_evidence_lane_input(
        "CLUD_PUBLIC_RELEASE_TAG",
        "CLUD_PUBLIC_EVIDENCE",
        "public release tag is provided by the release workflow",
    )
    mode = os.environ["CLUD_PUBLIC_MODE"]
    if mode not in {"candidate", "released"}:
        raise ValueError("invalid public release mode")
    version = tag.removeprefix("v")
    catalog_url = (
        f"https://github.com/talos-hq/clud/releases/download/{tag}/installer-candidate-manifest.json"
        if mode == "candidate"
        else ONLINE_URL
    )
    catalog = json.loads(public_bytes(catalog_url, 8 * 1024 * 1024))
    channel = "candidate" if mode == "candidate" else "latest-stable"
    assert catalog["channels"][channel] == version
    if mode == "candidate":
        assert catalog["channels"]["latest-stable"] != version
    else:
        assert "candidate" not in catalog["channels"]

    os_name = (
        "windows" if sys.platform == "win32" else "darwin" if sys.platform == "darwin" else "linux"
    )
    arch = platform.machine().lower()
    arch = {"amd64": "x86_64", "arm64": "aarch64"}.get(arch, arch)
    release = next(row for row in catalog["releases"] if row["version"] == version)
    matches = [
        row
        for row in release["platforms"]
        if row["platform"]["os"] == os_name
        and row["platform"]["arch"] == arch
        and row["asset"]["media_type"] == "application/octet-stream"
        and (os_name != "linux" or row["variant"] == {"flavor": "static-musl"})
    ]
    assert len(matches) == 1, matches
    asset = matches[0]["asset"]
    payload = public_bytes(asset["urls"][0])
    digest = hashlib.sha256(payload).hexdigest()
    assert len(payload) == asset["size_bytes"]
    assert digest == asset["sha256"]
    verify_direct_executable(payload, os_name, arch, public_executable_flavor(matches[0]))
    binary = tmp_path / asset["filename"]
    binary.write_bytes(payload)
    binary.chmod(0o755)

    env, destination = installer_target
    if os_name == "darwin":
        env["SHELL"] = "/bin/zsh"
    if mode == "candidate":
        env["CLUD_INSTALLER_CANDIDATE_TAG"] = tag
    else:
        env.pop("CLUD_INSTALLER_CANDIDATE_TAG", None)
    source = run_process(
        [str(binary), "--version"], env=env, capture_output=True, timeout=15, check=False
    )
    assert source.returncode == 0
    assert source.stdout.strip() == f"clud {version}".encode()
    result = run_process(
        [str(binary), "--installer", "--install-version", version, "--yes"],
        env=env,
        capture_output=True,
        timeout=120,
        check=False,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    assert hashlib.sha256(destination.read_bytes()).hexdigest() == digest
    older_installer = {}
    if mode == "released":
        assert_public_latest_install(
            binary, env, destination, version, digest,
            Path(os.environ["CLUD_PUBLIC_EVIDENCE"]).with_suffix(".latest.terminal.log"),
        )
        older_installer = assert_older_installer_discovery(
            catalog, os_name, arch, env, destination, version, digest, tmp_path
        )

    if os_name == "windows":
        shell_evidence = assert_windows_shell_activation(env, destination, version)
        lines = [shell_evidence["powershell-7"], f"clud {version}"]
    else:
        shell = "/bin/zsh" if os_name == "darwin" else "/bin/bash"
        fresh_env = {**env, "PATH": "/usr/bin:/bin:/usr/sbin:/sbin"}
        fresh_env.pop("BASH_ENV", None)
        lookup = run_process(
            [shell, "-l", "-c", "command -v clud; clud --version"],
            env=fresh_env,
            capture_output=True,
            timeout=25,
            check=False,
        )
        assert lookup.returncode == 0, lookup.stderr.decode(errors="replace")
        lines = lookup.stdout.decode(errors="replace").strip().splitlines()
    assert_public_lookup(lines, destination, version, os_name)
    evidence = {
        "mode": mode,
        "tag": tag,
        "version": version,
        "os": os_name,
        "arch": arch,
        "sha256": digest,
        "size_bytes": len(payload),
        "asset_url": asset["urls"][0],
        "resolved_path": lines[0],
        "older_installer": older_installer,
    }
    if os_name == "windows":
        evidence["shell_lookup"] = shell_evidence
    output = Path(os.environ["CLUD_PUBLIC_EVIDENCE"])
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(evidence, sort_keys=True) + "\n", encoding="utf-8")
    print("PUBLIC_EVIDENCE " + json.dumps(evidence, sort_keys=True))
