"""Target discovery for the grind integrator's cross-check stage (#1430)."""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "crates" / "clud-bin" / "assets" / "tools" / "git" / "ci_targets.py"
INTEGRATE = ROOT / "crates" / "clud-bin" / "assets" / "skills" / "grind-integrate" / "SKILL.md"


@pytest.fixture
def ct():
    name = "clud_test_ci_targets"
    spec = importlib.util.spec_from_file_location(name, SCRIPT)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    try:
        yield module
    finally:
        sys.modules.pop(name, None)


def _repo(tmp_path: Path, workflow: str = "", toolchain: str = "") -> Path:
    (tmp_path / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
    if workflow:
        wf = tmp_path / ".github" / "workflows"
        wf.mkdir(parents=True)
        (wf / "ci.yml").write_text(workflow, encoding="utf-8")
    if toolchain:
        (tmp_path / "rust-toolchain.toml").write_text(toolchain, encoding="utf-8")
    return tmp_path


def _triples(result: dict) -> set[str]:
    return {t["triple"] for t in result["targets"]}


def test_runs_on_families_map_to_their_triples(ct, tmp_path: Path) -> None:
    repo = _repo(
        tmp_path,
        "jobs:\n  a:\n    strategy:\n      matrix:\n"
        "        os: [ubuntu-24.04, windows-latest, macos-latest, macos-13, ubuntu-24.04-arm,"
        " windows-11-arm]\n",
    )
    assert _triples(ct.discover(repo)) == {
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "aarch64-pc-windows-msvc",
    }


def test_explicit_target_triples_and_the_host_exclusion(ct, tmp_path: Path) -> None:
    repo = _repo(
        tmp_path,
        "steps:\n  - run: cargo check --target x86_64-unknown-linux-musl\n"
        "  - run: cargo check --target x86_64-pc-windows-msvc # from linux\n"
        "  # cargo check --target aarch64-apple-darwin is only a comment\n",
    )
    assert _triples(ct.discover(repo)) == {"x86_64-unknown-linux-musl", "x86_64-pc-windows-msvc"}
    assert _triples(ct.discover(repo, host="x86_64-unknown-linux-musl")) == {
        "x86_64-pc-windows-msvc"
    }


def test_rust_toolchain_targets_count(ct, tmp_path: Path) -> None:
    repo = _repo(
        tmp_path,
        toolchain=(
            '[toolchain]\nchannel = "1.95"\n'
            'targets = ["aarch64-apple-darwin", "wasm32-unknown-unknown"]\n'
        ),
    )
    result = ct.discover(repo)
    assert _triples(result) == {"aarch64-apple-darwin"}
    assert result["skipped"] == [
        {
            "triple": "wasm32-unknown-unknown",
            "reason": "not a triple soldr owns a cross toolchain for",
        }
    ]


def test_a_triple_soldr_cannot_prepare_is_skipped_with_a_reason(ct, tmp_path: Path) -> None:
    repo = _repo(tmp_path, "steps:\n  - run: cargo check --target i686-pc-windows-gnu\n")
    result = ct.discover(repo)
    assert result["targets"] == []
    assert [s["triple"] for s in result["skipped"]] == ["i686-pc-windows-gnu"]
    assert result["skipped"][0]["reason"]


def test_a_non_rust_repo_prints_one_line_and_no_stage(ct, tmp_path: Path, capsys) -> None:
    (tmp_path / "package.json").write_text("{}", encoding="utf-8")
    assert ct.discover(tmp_path) == {"rust": False, "targets": [], "skipped": []}
    assert ct.main(["--root", str(tmp_path)]) == ct.EXIT_OK
    assert capsys.readouterr().out.strip() == "not a Rust project"


def test_cli_prints_json_for_a_rust_repo(ct, tmp_path: Path, capsys) -> None:
    repo = _repo(tmp_path, "runs-on: windows-latest\n")
    assert ct.main(["--root", str(repo)]) == ct.EXIT_OK
    assert json.loads(capsys.readouterr().out)["targets"][0]["triple"] == "x86_64-pc-windows-msvc"


def test_integrator_runs_the_multi_platform_lint_gate_before_verification() -> None:
    text = " ".join(INTEGRATE.read_text(encoding="utf-8").split())
    assert "3b. **Multi-platform lint gate, required before every push" in text
    assert text.index("3b. **Multi-platform") < text.index("4. **Verify.**")
    assert "[workspace.metadata.soldr].targets" in text
    assert "soldr dylint --all -- --workspace --all-targets --target <triple>" in text
