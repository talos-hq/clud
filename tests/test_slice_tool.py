"""Unit coverage for the bundled `code/slice.py` bounded reader (#1676)."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import sys
from pathlib import Path

TOOL = (
    Path(__file__).resolve().parents[1]
    / "crates"
    / "clud-bin"
    / "assets"
    / "tools"
    / "code"
    / "slice.py"
)


def _load():
    spec = importlib.util.spec_from_file_location("clud_test_slice", TOOL)
    assert spec
    assert spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["clud_test_slice"] = module
    spec.loader.exec_module(module)
    return module


slice_tool = _load()


def run(argv: list[str]) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = slice_tool.main(argv)
    return code, out.getvalue(), err.getvalue()


def rows(text: str) -> list[str]:
    return [line for line in text.splitlines() if not line.startswith("# ")]


def make(tmp_path: Path, lines: int = 100, name: str = "src.rs") -> Path:
    path = tmp_path / name
    path.write_text("".join(f"line {i}\n" for i in range(1, lines + 1)))
    return path


def test_ranges_are_line_numbered_and_merged_without_duplicates(tmp_path):
    path = make(tmp_path)
    code, out, _ = run([str(path), "--lines", "3-5", "--lines", "5-7", "--lines", "9"])
    assert code == 0
    assert rows(out) == [
        "3| line 3",
        "4| line 4",
        "5| line 5",
        "6| line 6",
        "7| line 7",
        "9| line 9",
    ]
    assert "100 lines" in out.splitlines()[0]


def test_open_ended_and_single_line_ranges(tmp_path):
    path = make(tmp_path, 10)
    code, out, _ = run([str(path), "--lines", "8-", "--lines", "1"])
    assert code == 0
    assert rows(out) == [
        "1| line 1",
        "8| line 8",
        "9| line 9",
        "10| line 10",
    ]


def test_default_selector_is_a_bounded_head(tmp_path):
    path = make(tmp_path, 200)
    code, out, _ = run([str(path)])
    assert code == 0
    assert len(rows(out)) == 40
    assert rows(out)[-1] == "40| line 40"


def test_byte_cap_prints_footer_with_resume_line(tmp_path):
    path = make(tmp_path, 400)
    code, out, _ = run([str(path), "--lines", "1-400", "--max-bytes", "500"])
    assert code == 3
    body = rows(out)
    assert body, "at least one line is printed even when the cap is tiny"
    footer = out.splitlines()[-1]
    assert footer.startswith("# slice: truncated at 500 B")
    resume = int(footer.split("--lines ")[1].split("-")[0])
    assert resume == int(body[-1].split("|")[0]) + 1
    assert len(out.encode()) <= 500 + len(out.splitlines()[-1].encode()) + 1


def test_each_range_keeps_a_share_of_the_cap(tmp_path):
    path = make(tmp_path, 400)
    code, out, _ = run([str(path), "--lines", "1-300", "--lines", "390-400", "--max-bytes", "1200"])
    assert code == 3
    body = rows(out)
    second = [row for row in body if int(row.split("|")[0]) >= 390]
    assert len(second) >= 3, "the short range must not be starved by the long one"
    assert second[0] == "390| line 390"
    assert "range(s) cut" in out.splitlines()[-1]


def test_grep_with_context_and_hit_cap(tmp_path):
    path = tmp_path / "app.py"
    path.write_text("".join(f"value_{i} = {i}\n" for i in range(1, 31)))
    code, out, _ = run([str(path), "--grep", r"value_(2|3)\b", "-C", "1"])
    assert code == 0
    assert rows(out) == [
        "1| value_1 = 1",
        "2| value_2 = 2",
        "3| value_3 = 3",
        "4| value_4 = 4",
    ]
    code, out, _ = run([str(path), "--grep", "value_", "--max-hits", "3", "--max-bytes", "4000"])
    assert code == 3
    assert "3 of 30 matches shown" in out.splitlines()[-1]


def test_grep_without_match_is_reported(tmp_path):
    path = make(tmp_path, 10)
    code, out, err = run([str(path), "--grep", "zzz"])
    assert code == 3
    assert out == ""
    assert "no line matching" in err


def test_long_line_is_clamped_not_dropped(tmp_path):
    path = tmp_path / "min.js"
    path.write_text("x" * 1200 + "\n")
    code, out, _ = run([str(path), "--lines", "1", "--max-line", "100"])
    assert code == 0
    assert rows(out)[0] == f"1| {'x' * 100}… (+1100 chars)"


def test_binary_missing_and_usage_errors(tmp_path):
    binary = tmp_path / "blob.bin"
    binary.write_bytes(b"\x00\x01\x02")
    assert run([str(binary)])[0] == 2
    assert run([str(tmp_path / "absent")])[0] == 2
    assert run([str(make(tmp_path)), "--lines", "x"])[0] == 1
    assert run([str(make(tmp_path)), "--max-bytes", "-1"])[0] == 1
