"""The Linux x64 unit suite is split across independent runners, by file.

It is CPU-bound and a hosted runner is two physical cores, so pytest-xdist
measured slower (172 s vs ~135 s serial). Separate machines do not contend:
one job runs the Rust harnesses and two jobs run half of the pytest suite each.

The property that matters is coverage: the shards must be disjoint and together
run every test, with weights that are only a balance hint. The release gate
must then require exactly the jobs the workflow produces, or a release could
pass with a shard unproven.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from types import SimpleNamespace

import pytest

from ci import pytest_shard, run_bundle
from ci.ci_matrix import LINUX_X64_UNIT_SHARDS, MACOS_X64_INTEGRATION_SHARDS
from ci.env import is_clud_session_var
from ci.pytest_shard import SHARD_OPTION, applied_marker, assign_files, load_weights, parse_spec
from ci.run_bundle import Shard, parse_shard, run_unit_suite, shard_was_applied

ROOT = Path(__file__).resolve().parent.parent
CI_YML = ROOT / ".github" / "workflows" / "ci.yml"
RUN_TESTS = ROOT / ".github" / "workflows" / "_run-tests.yml"


# ------------------------------------------------------------------ parsing --


@pytest.mark.parametrize(
    ("name", "expected"),
    [
        ("all", Shard(rust=True, python=True)),
        ("rust", Shard(rust=True, python=False)),
        ("py1of2", Shard(rust=False, python=True, spec="1/2")),
        ("py2of2", Shard(rust=False, python=True, spec="2/2")),
    ],
)
def test_parse_shard(name: str, expected: Shard) -> None:
    assert parse_shard(name) == expected


@pytest.mark.parametrize("name", ["", "py", "py0of2", "py3of2", "py1of0", "rust2", "ALL"])
def test_unknown_shard_names_fail_closed(name: str) -> None:
    with pytest.raises(ValueError, match="unknown unit shard"):
        parse_shard(name)


@pytest.mark.parametrize(("spec", "expected"), [("1/2", (0, 2)), ("2/2", (1, 2)), ("3/3", (2, 3))])
def test_parse_spec(spec: str, expected: tuple[int, int]) -> None:
    assert parse_spec(spec) == expected


@pytest.mark.parametrize("spec", ["", "1", "0/2", "3/2", "1/0", "a/b", "1/2/3"])
def test_bad_shard_spec_fails_closed(spec: str) -> None:
    with pytest.raises(ValueError, match=r"must look like|need 1 <= k <= n"):
        parse_spec(spec)


# --------------------------------------------------------------- assignment --


def _shards(counts: dict[str, int], weights: dict[str, float], n: int) -> list[set[str]]:
    assignment = assign_files(counts, weights, n)
    return [{name for name, shard in assignment.items() if shard == k} for k in range(n)]


@pytest.mark.parametrize("n", [1, 2, 3, 5])
def test_shards_are_disjoint_and_cover_every_file(n: int) -> None:
    counts = {f"tests/test_{i}.py": (i % 7) + 1 for i in range(41)}
    weights = {name: float(i % 5) for i, name in enumerate(counts) if i % 3}
    shards = _shards(counts, weights, n)
    assert set().union(*shards) == set(counts)
    assert sum(len(shard) for shard in shards) == len(counts)


def test_assignment_is_deterministic_and_order_independent() -> None:
    counts = {f"tests/test_{i}.py": i + 1 for i in range(20)}
    weights = {f"tests/test_{i}.py": float(i) for i in range(0, 20, 2)}
    first = assign_files(counts, weights, 3)
    shuffled = dict(reversed(list(counts.items())))
    assert assign_files(shuffled, weights, 3) == first


def test_unrecorded_files_are_weighted_by_test_count() -> None:
    counts = {"tests/test_big.py": 500, "tests/test_a.py": 1, "tests/test_b.py": 1}
    shards = _shards(counts, {}, 2)
    big = next(shard for shard in shards if "tests/test_big.py" in shard)
    assert big == {"tests/test_big.py"}  # the rest balance against it


@pytest.mark.parametrize(
    "body", ["[]", "null", '{"tests/test_a.py": null, "tests/test_b.py": true}']
)
def test_malformed_weights_fall_back_to_test_counts(tmp_path: Path, body: str) -> None:
    path = tmp_path / "weights.json"
    path.write_text(body, encoding="utf-8")
    assert load_weights(path) == {}


def test_recorded_weights_beat_test_counts() -> None:
    counts = {"tests/test_slow.py": 1, "tests/test_fast1.py": 50, "tests/test_fast2.py": 50}
    weights = {"tests/test_slow.py": 100.0, "tests/test_fast1.py": 1.0, "tests/test_fast2.py": 1.0}
    shards = _shards(counts, weights, 2)
    slow = next(shard for shard in shards if "tests/test_slow.py" in shard)
    assert slow == {"tests/test_slow.py"}


@pytest.mark.parametrize("n", [1, 2, 3, 5])
def test_native_window_files_always_land_on_the_first_shard(n: int) -> None:
    # #2054: only the first Python shard installs xvfb/weston/mesa.
    counts = {f"tests/test_{i}.py": 30 for i in range(12)}
    counts.update(dict.fromkeys(pytest_shard.NATIVE_WINDOW_FILES, 1))
    assignment = assign_files(counts, {}, n)
    assert {assignment[name] for name in pytest_shard.NATIVE_WINDOW_FILES} == {0}


def test_native_window_files_exist() -> None:
    root = Path(__file__).resolve().parent.parent
    for name in pytest_shard.NATIVE_WINDOW_FILES:
        assert (root / name).is_file(), f"{name} was renamed; update NATIVE_WINDOW_FILES"


def test_only_the_first_python_shard_installs_the_native_window_runtime() -> None:
    root = Path(__file__).resolve().parent.parent
    workflow = (root / ".github/workflows/_run-tests.yml").read_text(encoding="utf-8")
    expected = "native-window-runtime: ${{ inputs.shard == 'all' || inputs.shard == 'py1of3' }}"
    assert expected in workflow


def test_checked_in_weights_split_the_suite_into_balanced_halves() -> None:
    weights = {k: v for k, v in load_weights().items() if not k.startswith("tests/integration/")}
    assert weights, "ci/unit_shard_weights.json is missing or unreadable"
    counts = dict.fromkeys(weights, 1)
    for n in (2, 3):
        loads = [0.0] * n
        for name, shard in assign_files(counts, weights, n).items():
            loads[shard] += weights[name]
        assert max(loads) / (sum(loads) / n) < 1.1, loads


def test_macos_x64_integration_shards_fit_the_job_ceiling() -> None:
    """~25 min at the slowest speed measured: the heaviest of two stays near half."""
    weights = {k: v for k, v in load_weights().items() if k.startswith("tests/integration/")}
    assert weights, "integration weights are missing"
    count = len(MACOS_X64_INTEGRATION_SHARDS)
    loads = [0.0] * count
    for name, shard in assign_files(dict.fromkeys(weights, 1), weights, count).items():
        loads[shard] += weights[name]
    assert max(loads) / sum(loads) < 0.55, loads


def test_macos_x64_matrix_matches_the_shard_table() -> None:
    block = (
        CI_YML.read_text(encoding="utf-8").split("\n  test-macos-x64:\n", 1)[1].split("\n\n", 1)[0]
    )
    assert "shard: ${{ matrix.shard }}" in block
    assert re.findall(r"suite: integration, shard: (\w+)", block) == list(
        MACOS_X64_INTEGRATION_SHARDS
    )


def test_integration_suite_accepts_python_shards_and_refuses_rust(monkeypatch) -> None:
    assert parse_shard("py2of3").spec == "2/3"
    assert run_bundle.shard_pytest_args(parse_shard("py2of3"), ["-x"]) == [
        "-p",
        "ci.pytest_shard",
        SHARD_OPTION,
        "2/3",
        "-x",
    ]
    assert run_bundle.shard_pytest_args(parse_shard("all"), ["-x"]) == ["-x"]
    with pytest.raises(SystemExit):
        run_bundle.main(["--bundle", "x", "--suite", "integration", "--shard", "rust"])
    with pytest.raises(SystemExit):
        run_bundle.main(["--bundle", "x", "--suite", "harness", "--shard", "py1of3"])


def test_integration_shard_that_did_not_filter_fails(monkeypatch, tmp_path: Path, capsys) -> None:
    log = tmp_path / "pytest-integration.log"
    log.write_text("no marker\n", encoding="utf-8")
    monkeypatch.setattr(run_bundle, "pytest_log_path", lambda suite: log)
    assert not run_bundle.shard_filter_held(parse_shard("py1of3"), "integration")
    assert "did not filter" in capsys.readouterr().err
    log.write_text(applied_marker("1/3") + " 1 of 3 tests\n", encoding="utf-8")
    assert run_bundle.shard_filter_held(parse_shard("py1of3"), "integration")
    assert run_bundle.shard_filter_held(parse_shard("all"), "integration")


def test_weights_file_is_plain_seconds_per_test_file() -> None:
    raw = json.loads(pytest_shard.WEIGHTS_PATH.read_text(encoding="utf-8"))
    assert raw
    for name, seconds in raw.items():
        assert re.fullmatch(r"tests/.+\.py", name), name
        assert isinstance(seconds, (int, float)), name
        assert seconds >= 0, name


def test_missing_weights_file_only_costs_balance(tmp_path: Path) -> None:
    assert load_weights(tmp_path / "absent.json") == {}
    bad = tmp_path / "bad.json"
    bad.write_text("{not json", encoding="utf-8")
    assert load_weights(bad) == {}


# ------------------------------------------------------------------- plugin --


class _Hook:
    def __init__(self) -> None:
        self.deselected: list[object] = []

    def pytest_deselected(self, items: list[object]) -> None:
        self.deselected.extend(items)


def _config(spec: str | None) -> SimpleNamespace:
    def getoption(name: str) -> str | None:
        return spec if name == SHARD_OPTION else None

    return SimpleNamespace(hook=_Hook(), getoption=getoption)


def _items(files: dict[str, int]) -> list[SimpleNamespace]:
    return [
        SimpleNamespace(nodeid=f"{name}::test_{i}")
        for name, count in files.items()
        for i in range(count)
    ]


def test_plugin_keeps_only_this_shards_files_and_reports_the_rest(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    files = {f"tests/test_{i}.py": (i % 4) + 1 for i in range(12)}
    monkeypatch.setattr(pytest_shard, "load_weights", lambda: {})
    kept: list[set[str]] = []
    for spec in ("1/2", "2/2"):
        items = _items(files)
        config = _config(spec)
        pytest_shard.pytest_collection_modifyitems(config, items)  # type: ignore[arg-type]
        kept.append({item.nodeid for item in items})
        assert len(items) + len(config.hook.deselected) == sum(files.values())
    every = {item.nodeid for item in _items(files)}
    assert kept[0] | kept[1] == every
    assert not kept[0] & kept[1]


def test_plugin_announces_itself_with_the_marker_run_bundle_checks(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(pytest_shard, "load_weights", lambda: {})
    pytest_shard.pytest_collection_modifyitems(_config("2/3"), _items({"tests/test_a.py": 2}))  # type: ignore[arg-type]
    assert applied_marker("2/3") in capsys.readouterr().out


def test_plugin_is_inert_without_a_shard_spec() -> None:
    items = _items({"tests/test_a.py": 3})
    pytest_shard.pytest_collection_modifyitems(_config(None), items)  # type: ignore[arg-type]
    assert len(items) == 3


def test_the_shard_option_is_not_an_environment_variable() -> None:
    """tests/conftest.py scrubs CLUD_* session variables, which is how an env-var
    version of this split silently ran the whole suite in every shard."""
    assert SHARD_OPTION.startswith("--")
    assert is_clud_session_var("CLUD_PYTEST_SHARD"), "the scrub that motivated the option changed"


# ---------------------------------------------------------- run_bundle glue --


@pytest.fixture
def recorded(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> dict[str, object]:
    calls: dict[str, object] = {"harnesses": 0, "pytest": []}

    def fake_harnesses(bundle: Path, manifest: dict, env: dict[str, str]) -> int:
        calls["harnesses"] = int(calls["harnesses"]) + 1  # type: ignore[call-overload]
        return 0

    def fake_pytest(marker: str, env: dict[str, str], extra: list[str], *, suite: str) -> int:
        calls["pytest"].append((marker, dict(env), list(extra), suite))  # type: ignore[attr-defined]
        if SHARD_OPTION in extra:  # what the real plugin prints after filtering
            spec = extra[extra.index(SHARD_OPTION) + 1]
            (tmp_path / "pytest-unit.log").write_text(f"{applied_marker(spec)} 1 of 2 tests\n")
        return 0

    monkeypatch.setattr(run_bundle, "run_harnesses", fake_harnesses)
    monkeypatch.setattr(run_bundle, "run_pytest", fake_pytest)
    monkeypatch.setattr(run_bundle, "LOG_DIR", tmp_path)
    return calls


def test_rust_shard_runs_harnesses_and_no_pytest(recorded: dict[str, object]) -> None:
    assert run_unit_suite(Path("b"), {}, {}, parse_shard("rust"), []) == 0
    assert recorded["harnesses"] == 1
    assert recorded["pytest"] == []


def test_python_shard_runs_pytest_with_its_slice_and_no_harnesses(
    recorded: dict[str, object],
) -> None:
    assert run_unit_suite(Path("b"), {}, {"X": "1"}, parse_shard("py2of2"), ["-q"]) == 0
    assert recorded["harnesses"] == 0
    ((marker, env, extra, suite),) = recorded["pytest"]  # type: ignore[misc]
    assert (marker, suite) == ("not integration", "unit")
    assert env == {"X": "1"}
    assert extra == ["-p", "ci.pytest_shard", "--clud-shard", "2/2", "-q"]


def test_all_runs_everything_unsharded(recorded: dict[str, object]) -> None:
    assert run_unit_suite(Path("b"), {}, {}, parse_shard("all"), []) == 0
    assert recorded["harnesses"] == 1
    ((_, _, extra, _),) = recorded["pytest"]  # type: ignore[misc]
    assert SHARD_OPTION not in extra
    assert "ci.pytest_shard" not in extra


def test_a_failing_rust_shard_skips_pytest(monkeypatch: pytest.MonkeyPatch) -> None:
    def no_pytest(*_args: object, **_kwargs: object) -> int:
        pytest.fail("pytest must not run after a Rust failure")

    monkeypatch.setattr(run_bundle, "run_harnesses", lambda *_: 1)
    monkeypatch.setattr(run_bundle, "run_pytest", no_pytest)
    assert run_unit_suite(Path("b"), {}, {}, parse_shard("all"), []) == 1


def test_a_shard_that_did_not_filter_fails_instead_of_running_everything(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """The regression that motivated the guard: pytest exits 0 having run it all."""
    monkeypatch.setattr(run_bundle, "run_pytest", lambda *a, **k: 0)
    monkeypatch.setattr(run_bundle, "LOG_DIR", tmp_path)
    (tmp_path / "pytest-unit.log").write_text("collected 1652 items\n1449 passed\n")
    assert run_unit_suite(Path("b"), {}, {}, parse_shard("py1of2"), []) == 1
    assert "did not filter the collection" in capsys.readouterr().err
    assert not shard_was_applied("1/2")
    (tmp_path / "pytest-unit.log").write_text(f"{applied_marker('1/2')} 800 of 1652 tests\n")
    assert shard_was_applied("1/2")
    assert not shard_was_applied("2/2")


# ------------------------------------------------- workflow + release gate --


def test_the_unit_job_is_a_matrix_over_exactly_the_declared_shards() -> None:
    text = CI_YML.read_text(encoding="utf-8")
    block = text.split("\n  test-linux-x64-unit:\n", 1)[1].split(
        "\n  test-linux-x64-integration:\n", 1
    )[0]
    listed = re.search(r"^        shard: \[([^\]]+)\]$", block, re.MULTILINE)
    assert listed, "test-linux-x64-unit has no shard matrix"
    assert tuple(part.strip() for part in listed.group(1).split(",")) == LINUX_X64_UNIT_SHARDS
    assert "shard: ${{ matrix.shard }}" in block
    assert "suite: unit" in block
    assert "fail-fast: false" in block


def test_the_reusable_workflow_forwards_the_shard() -> None:
    text = RUN_TESTS.read_text(encoding="utf-8")
    assert "--shard ${{ inputs.shard }}" in text
    # Matrix shards upload concurrently; a shared artifact name would collide.
    assert "inputs.shard != 'all'" in text
