"""Keep hosted SDK reclamation away from developer and self-hosted machines."""

from pathlib import Path

import pytest

from ci import hosted_disk

HOSTED = {
    "GITHUB_ACTIONS": "true",
    "RUNNER_ENVIRONMENT": "github-hosted",
    "RUNNER_OS": "Linux",
}


@pytest.mark.parametrize(
    "override",
    [
        {"GITHUB_ACTIONS": "false"},
        {"GITHUB_ACTIONS": ""},
        {"RUNNER_ENVIRONMENT": "self-hosted"},
        {"RUNNER_ENVIRONMENT": ""},
        {"RUNNER_OS": "Windows"},
        {"RUNNER_OS": "macOS"},
    ],
)
def test_reclamation_never_inspects_non_hosted_sdks(monkeypatch, override) -> None:
    def unexpected(*args):
        pytest.fail("non-hosted run must not inspect or delete SDK directories")

    monkeypatch.setattr(hosted_disk.shutil, "disk_usage", unexpected)
    monkeypatch.setattr(hosted_disk.shutil, "rmtree", unexpected)
    assert hosted_disk.reclaim(HOSTED | override) == []


def test_reclamation_removes_only_the_fixed_sdk_allowlist(monkeypatch, tmp_path: Path) -> None:
    android = tmp_path / "android"
    dotnet = tmp_path / "dotnet"
    required = tmp_path / "rust"
    for path in (android, dotnet, required):
        path.mkdir()
        (path / "payload").write_text("data")
    monkeypatch.setattr(hosted_disk, "UNUSED_SDKS", (android, dotnet))
    assert hosted_disk.reclaim(HOSTED) == [android, dotnet]
    assert not android.exists()
    assert not dotnet.exists()
    assert (required / "payload").read_text() == "data"


def test_reclamation_leaves_symlinks_and_missing_directories_alone(
    monkeypatch, tmp_path: Path
) -> None:
    sdk = tmp_path / "sdk"
    sdk.mkdir()
    missing = tmp_path / "missing"
    monkeypatch.setattr(hosted_disk, "UNUSED_SDKS", (sdk, missing))
    original = Path.is_symlink
    monkeypatch.setattr(Path, "is_symlink", lambda path: path == sdk or original(path))
    assert hosted_disk.reclaim(HOSTED) == []
    assert sdk.is_dir()


def test_reclamation_does_not_hide_cleanup_failures(monkeypatch, tmp_path: Path) -> None:
    sdk = tmp_path / "sdk"
    sdk.mkdir()
    monkeypatch.setattr(hosted_disk, "UNUSED_SDKS", (sdk,))

    def denied(path):
        raise PermissionError(path)

    monkeypatch.setattr(hosted_disk.shutil, "rmtree", denied)
    with pytest.raises(PermissionError):
        hosted_disk.reclaim(HOSTED)


def test_cleanup_runs_before_cross_toolchains_and_only_on_hosted_linux() -> None:
    action = Path(__file__).resolve().parents[1] / ".github/actions/setup-build/action.yml"
    steps = action.read_text().split("\n    - ")
    cleanup = next(i for i, step in enumerate(steps) if "-m ci.hosted_disk" in step)
    soldr = next(i for i, step in enumerate(steps) if "uses: forge-ci/setup-soldr@v0" in step)
    assert cleanup < soldr
    assert "runner.os == 'Linux'" in steps[cleanup]
    assert "runner.environment == 'github-hosted'" in steps[cleanup]
    assert 'LD_LIBRARY_PATH="$pythonLocation/lib"' in steps[cleanup]
