"""Automatic Windows lanes for path-sensitive PRs (DD-224, ci/windows_sensitive.py)."""

from __future__ import annotations

from pathlib import Path

import pytest

from ci import ci_matrix
from ci.ci_matrix import resolve_tier, selected, with_auto_windows
from ci.windows_sensitive import (
    WINDOWS_SENSITIVE_PATHS,
    python_test_hazards,
    sensitive_path,
    windows_reasons,
)

REPO = Path(__file__).resolve().parents[1]
CI_YML = REPO / ".github" / "workflows" / "ci.yml"
# Kept out of `.py` files so the content check never flags this test itself.
FIXTURES = REPO / "tests" / "fixtures" / "windows_sensitive"

def fixture(name: str) -> str:
    return (FIXTURES / name).read_text(encoding="utf-8")


# The #2683 fixture #2831 removed: an extensionless `gh` script on PATH.
PATH_STUB_TEST = fixture("path_stub.txt")

# The #2827 assertion #2831 made POSIX-only.
EXEC_BIT_TEST = fixture("exec_bit.txt")

# The in-process pattern #2831 replaced the stub with: no file, no PATH.
IN_PROCESS_STUB_TEST = fixture("in_process_stub.txt")

# File sets of the PRs whose Windows regressions reached the release gate.
INCIDENT_PRS = {
    2737: [
        "crates/clud-bin/src/block_bad_cmd_repeat.rs",
        "crates/clud-bin/src/block_bad_cmd_repeat_tests.rs",
        "docs/architecture/hook-dispatch.md",
        "tests/test_poll_note_hook.py",
    ],
    2788: [
        "crates/clud-bin/src/block_bad_cmd_reread.rs",
        "crates/clud-bin/src/block_bad_cmd_reread_tests.rs",
        "docs/architecture/hook-dispatch.md",
    ],
    2683: [
        "crates/clud-bin/assets/tools/github/pr_status.py",
        "crates/clud-bin/src/block_bad_cmd_grind_caps.rs",
        "crates/clud-bin/src/command/builder.rs",
        "crates/clud-bin/src/command/tests/loop_contract.rs",
        "crates/clud-bin/src/tools.rs",
        "docs/architecture/bounded-output.md",
        "tests/test_pr_status.py",
    ],
}


def no_files(_path: str) -> str | None:
    return None


@pytest.mark.parametrize(
    "path",
    [
        "crates/clud-bin/src/block_bad_cmd_repeat.rs",
        "crates/clud-bin/src/block_bad_cmd.rs",
        "crates/clud-bin/src/clud_hooks_run.rs",
        "crates/clud-bin/src/home.rs",
        "crates/clud-bin/src/shim_resolve.rs",
        "crates/clud-bin/src/shim_main/mod.rs",
        "crates/clud-bin/src/platform/windows/console.rs",
        "crates/tap/src/platform/mod.rs",
        "crates/clud-bin/src/path_norm.rs",
    ],
)
def test_path_sensitive_areas_match(path: str) -> None:
    assert sensitive_path(path), path


@pytest.mark.parametrize(
    "path",
    [
        "docs/architecture/ci.md",
        "README.md",
        "crates/clud-bin/src/args.rs",
        "crates/clud-bin/src/command/builder.rs",
        "crates/clud-bin/assets/tools/github/pr_status.py",
        "ci/ci_matrix.py",
        # A doc that merely names an area is not the area.
        "docs/block_bad_cmd.md",
    ],
)
def test_other_paths_do_not_match(path: str) -> None:
    assert sensitive_path(path) is None, path


def test_every_pattern_names_a_tracked_file() -> None:
    """A renamed area must not leave a dead pattern behind."""
    tracked = [
        p.relative_to(REPO).as_posix() for p in (REPO / "crates").rglob("*") if p.is_file()
    ]
    for pattern in WINDOWS_SENSITIVE_PATHS:
        assert any(sensitive_path(path) == pattern for path in tracked), pattern


def test_content_detector_flags_the_incident_patterns() -> None:
    assert python_test_hazards(PATH_STUB_TEST) == ["puts an executable stub on PATH"]
    assert python_test_hazards(EXEC_BIT_TEST) == ["checks POSIX execute bits"]
    assert python_test_hazards(fixture("x_ok.txt")) == [
        "checks POSIX execute bits"
    ]


def test_content_detector_passes_the_portable_patterns() -> None:
    assert python_test_hazards(IN_PROCESS_STUB_TEST) == []
    # An executable without PATH, or PATH without an executable, is not a stub.
    assert python_test_hazards(fixture("chmod_only.txt")) == []
    assert python_test_hazards(fixture("path_only.txt")) == []


def test_content_check_applies_only_to_changed_python_tests() -> None:
    texts = {
        "tests/test_pr_status.py": PATH_STUB_TEST,
        "tests/harness/test_rm.py": EXEC_BIT_TEST,
        "ci/sign_macos_release.py": EXEC_BIT_TEST,
    }
    assert windows_reasons(texts, texts.get) == [
        "tests/test_pr_status.py puts an executable stub on PATH",
        "tests/harness/test_rm.py checks POSIX execute bits",
    ]
    # A test the PR deleted cannot break on Windows.
    assert windows_reasons(["tests/test_pr_status.py"], no_files) == []


@pytest.mark.parametrize("number", sorted(INCIDENT_PRS))
def test_each_incident_pr_would_have_selected_windows(number: int) -> None:
    files = INCIDENT_PRS[number]
    reasons = windows_reasons(files, no_files)
    assert reasons, number
    tier = resolve_tier("pull_request", "", "")
    assert with_auto_windows(tier, "pull_request", reasons) == "windows"


def test_the_2683_test_alone_would_have_selected_windows() -> None:
    """Without its hook file, the PATH stub itself still triggers."""
    texts = {"tests/test_pr_status.py": PATH_STUB_TEST}
    assert windows_reasons(texts, texts.get)


def test_the_2827_test_would_have_selected_windows() -> None:
    texts = {"tests/test_sign_macos_release.py": EXEC_BIT_TEST}
    assert windows_reasons(texts, texts.get) == [
        "tests/test_sign_macos_release.py checks POSIX execute bits"
    ]


@pytest.mark.parametrize(
    ("event", "labels", "reasons", "expected"),
    [
        # A changed hook file selects Windows without a label.
        ("pull_request", "", ["x"], "windows"),
        # A docs-only PR stays minimal.
        ("pull_request", "", [], "minimal"),
        # Labels still work, with or without a sensitive change.
        ("pull_request", "ci-windows", [], "windows"),
        ("pull_request", "ci-windows", ["x"], "windows"),
        ("pull_request", "ci-test", ["x"], "extended"),
        ("pull_request", "ci-full", ["x"], "full"),
        # macOS + Windows needs both native sets: only full has them.
        ("pull_request", "ci-macos", ["x"], "full"),
        ("pull_request", "ci-macos", [], "macos"),
        # The queue and main pushes stay minimal: the PR already proved it.
        ("merge_group", "", ["x"], "minimal"),
        ("push", "", ["x"], "minimal"),
    ],
)
def test_auto_windows_decision(event, labels, reasons, expected) -> None:
    tier = resolve_tier(event, "", labels)
    assert with_auto_windows(tier, event, reasons) == expected


def test_auto_windows_selects_the_same_lanes_as_the_label() -> None:
    auto = with_auto_windows("minimal", "pull_request", ["x"])
    assert selected(auto) == selected(resolve_tier("pull_request", "", "ci-windows"))


def _run_main(tmp_path, monkeypatch, capsys, changed: list[str]) -> dict[str, str]:
    listing = tmp_path / "changed-files.txt"
    listing.write_text("\n".join(changed) + "\n", encoding="utf-8")
    monkeypatch.delenv("GITHUB_OUTPUT", raising=False)
    monkeypatch.chdir(tmp_path)
    assert ci_matrix.main(["--event-name", "pull_request", "--changed-files", str(listing)]) == 0
    lines = capsys.readouterr().out.splitlines()
    return dict(line.split("=", 1) for line in lines if "=" in line and not line.startswith("::"))


def test_main_emits_windows_mode_for_a_hook_change(tmp_path, monkeypatch, capsys) -> None:
    out = _run_main(tmp_path, monkeypatch, capsys, INCIDENT_PRS[2788])
    assert out["tier"] == "windows"
    assert "x86_64-pc-windows-msvc" in out["build"]
    assert "block_bad_cmd_reread.rs" in out["auto_windows"]


def test_main_keeps_a_docs_only_pr_minimal(tmp_path, monkeypatch, capsys) -> None:
    out = _run_main(tmp_path, monkeypatch, capsys, ["docs/architecture/ci.md"])
    assert out["tier"] == "minimal"
    assert out["auto_windows"] == "[]"


def test_main_reads_python_tests_from_the_checkout(tmp_path, monkeypatch, capsys) -> None:
    (tmp_path / "tests").mkdir()
    (tmp_path / "tests" / "test_stub.py").write_text(PATH_STUB_TEST, encoding="utf-8")
    out = _run_main(tmp_path, monkeypatch, capsys, ["tests/test_stub.py"])
    assert out["tier"] == "windows"


def test_a_pull_request_without_a_file_list_fails_closed(monkeypatch) -> None:
    monkeypatch.delenv("CHANGED_FILES", raising=False)
    with pytest.raises(ValueError, match="changed-files"):
        ci_matrix.main(["--event-name", "pull_request"])


def test_workflow_lists_changed_files_only_for_pull_requests() -> None:
    text = CI_YML.read_text(encoding="utf-8")
    static = text.split("\n  static:\n", 1)[1].split("\n  static-checks:\n", 1)[0]
    step = static.split("- name: List the pull request's changed files", 1)[1]
    step = step.split("- name:", 1)[0]
    assert "if: github.event_name == 'pull_request'" in step
    assert '"$BASE_SHA...$HEAD_SHA"' in step
    assert 'CHANGED_FILES=' in step
    # The list must exist before the mode resolves.
    assert static.index("List the pull request's changed files") < static.index(
        "Resolve CI mode and candidate SHA"
    )
