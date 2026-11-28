"""Unit coverage for the bundled `edit/edit.py` structured-edit tool."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

TOOL_DIR = Path(__file__).resolve().parents[1] / "crates" / "clud-bin" / "assets" / "tools" / "edit"


def _load():
    if str(TOOL_DIR) not in sys.path:
        sys.path.insert(0, str(TOOL_DIR))
    import edit

    return edit


edit = _load()

FLEET = """\
# fleet inventory
[defaults]
max = 1 # global default

[[host]]
name = "dell-jw22zc3"   # the big one

  [[host.lane]]
  name = "obs-rust"
  # dispatch knobs
  dispatch.max = 4   # keep low
  [host.lane.limits]
  cpu = 8

  [[host.lane]]
  name = "fastled"
  dispatch.max = 2

[[host]]
name = "e640"

  [[host.lane]]
  name = "obs-rust"
  dispatch.max = 1
"""


def _run(*argv: str) -> int:
    return edit.main(list(argv))


def _write(tmp_path: Path, name: str, body: str) -> Path:
    p = tmp_path / name
    p.write_bytes(body.encode())
    return p


# ------------------------------------------------------------------- toml


def test_toml_set_selector_preserves_comments(tmp_path: Path) -> None:
    out = edit.toml_edit(
        FLEET, "set", "host[name=dell-jw22zc3].lane[name=obs-rust].dispatch.max", "8", False
    )
    assert "  dispatch.max = 8   # keep low\n" in out
    assert out.replace("dispatch.max = 8   # keep low", "dispatch.max = 4   # keep low") == FLEET


def test_toml_subtable_header_and_index_selector() -> None:
    out = edit.toml_edit(FLEET, "set", "host[0].lane[name=obs-rust].limits.cpu", "16", False)
    assert "  cpu = 16\n" in out
    got = edit.toml_edit(FLEET, "get", "host[name=e640].lane[0].dispatch.max", None, False)
    assert got == "1"


def test_toml_root_table_and_get() -> None:
    assert edit.toml_edit(FLEET, "get", "defaults.max", None, False) == "1"


def test_toml_missing_path_fails(tmp_path: Path) -> None:
    with pytest.raises(edit.EditError, match="not found"):
        edit.toml_edit(FLEET, "set", "host[name=e640].lane[name=obs-rust].dispatch.min", "3", False)
    with pytest.raises(edit.EditError, match="matches nothing"):
        edit.toml_edit(FLEET, "get", "host[name=nope].lane[0].dispatch.max", None, False)


def test_toml_ambiguous_selector_fails() -> None:
    doubled = FLEET.replace('name = "e640"', 'name = "dell-jw22zc3"')
    with pytest.raises(edit.EditError, match="ambiguous"):
        edit.toml_edit(doubled, "set", "host[name=dell-jw22zc3].lane[0].dispatch.max", "8", False)


def test_toml_create_and_delete() -> None:
    out = edit.toml_edit(
        FLEET, "set", "host[name=e640].lane[name=obs-rust].dispatch.min", "0", True
    )
    assert "dispatch.min = 0\n" in out
    out = edit.toml_edit(FLEET, "delete", "host[name=e640].lane[0].dispatch.max", None, False)
    assert out.count("dispatch.max") == FLEET.count("dispatch.max") - 1
    assert "dispatch.max = 2" in out


def test_toml_value_must_be_literal() -> None:
    with pytest.raises(edit.EditError, match="--string"):
        edit.toml_value("hello world", False)
    assert edit.toml_value('a"b', True) == '"a\\"b"'


def test_toml_cli_dry_run_writes_nothing(tmp_path: Path, capsys) -> None:
    p = _write(tmp_path, "fleet.toml", FLEET)
    rc = _run("toml", str(p), "set", "host[name=e640].lane[0].dispatch.max", "5", "--dry-run")
    assert rc == 0
    assert p.read_text() == FLEET
    assert "+  dispatch.max = 5" in capsys.readouterr().out
    assert _run("toml", str(p), "set", "host[name=e640].lane[0].dispatch.max", "5") == 0
    assert "dispatch.max = 5" in p.read_text()
    assert _run("toml", str(p), "get", "host[name=zzz].lane[0].x") == 2


# -------------------------------------------------------------------- nix

NIX = """\
{ config, lib, pkgs, ... }:
let
  cfg = { max = 99; };
in
{
  # runners
  services.gh-runner-host = {
    enable = true;
    lanes.obs-rust = { labels = [ "a" ]; dispatch = { max = 4; min = 1; }; };
    lanes."fastled" = {
      dispatch.max = 2; # trailing
    };
  };
  environment.systemPackages = with pkgs; [ git ];
  networking.hostName = "x";
}
"""


def test_nix_set_inline_set() -> None:
    out = edit.nix_edit(NIX, "set", "services.gh-runner-host.lanes.obs-rust.dispatch.max", "8")
    assert "dispatch = { max = 8; min = 1; };" in out
    assert out.replace("max = 8; min", "max = 4; min") == NIX


def test_nix_set_nested_block_and_quoted() -> None:
    out = edit.nix_edit(NIX, "set", "services.gh-runner-host.lanes.fastled.dispatch.max", "6")
    assert "dispatch.max = 6; # trailing" in out
    path = 'services.gh-runner-host.lanes."fastled".dispatch.max'
    assert edit.nix_edit(NIX, "get", path, None) == "2"


def test_nix_ignores_let_bindings_and_handles_with() -> None:
    with pytest.raises(edit.EditError, match="not found"):
        edit.nix_edit(NIX, "get", "cfg.max", None)
    assert edit.nix_edit(NIX, "get", "environment.systemPackages", None) == "with pkgs; [ git ]"
    assert edit.nix_edit(NIX, "get", "networking.hostName", None) == '"x"'


def test_nix_missing_reports_prefix() -> None:
    with pytest.raises(edit.EditError, match=r"deepest existing prefix: services\.gh-runner-host"):
        edit.nix_edit(NIX, "set", "services.gh-runner-host.lanes.nope.max", "1")


def test_nix_ambiguous_fails() -> None:
    dup = NIX.replace(
        'networking.hostName = "x";',
        'networking.hostName = "x";\n  networking = { hostName = "y"; };',
    )
    with pytest.raises(edit.EditError, match="ambiguous, bound 2 times"):
        edit.nix_edit(dup, "set", "networking.hostName", '"z"')


def test_nix_rejects_structure_breaking_expr() -> None:
    with pytest.raises(edit.EditError, match="structure"):
        edit.nix_edit(NIX, "set", "networking.hostName", '"a"; foo = 1')


# --------------------------------------------------------------- markdown

MD = """\
# Policy

| ID | Rule |
|----|------|
| `GATE-013` | thirteen |
| `GATE-014` | fourteen |
| `GATE-020` | twenty |

text
"""


def _md(**kw):
    import argparse

    base = dict(
        after_key=None, before_key=None, replace_key=None, append=False, key_column=1, table=None
    )
    base.update(kw)
    return argparse.Namespace(**base)


def test_md_insert_after_key() -> None:
    out = edit.md_table_row(MD, _md(after_key="GATE-014", row="| `GATE-018` | eighteen |"))
    assert "| `GATE-014` | fourteen |\n| `GATE-018` | eighteen |\n| `GATE-020`" in out


def test_md_replace_and_append() -> None:
    out = edit.md_table_row(MD, _md(replace_key="GATE-013", row="| `GATE-013` | new |"))
    assert "| `GATE-013` | new |" in out
    assert "thirteen" not in out
    out = edit.md_table_row(MD, _md(append=True, row="| `GATE-021` | x |"))
    assert "| `GATE-020` | twenty |\n| `GATE-021` | x |\n\ntext" in out


def test_md_failures() -> None:
    with pytest.raises(edit.EditError, match="no table row with key"):
        edit.md_table_row(MD, _md(after_key="GATE-099", row="| a | b |"))
    with pytest.raises(edit.EditError, match="already has a row"):
        edit.md_table_row(MD, _md(after_key="GATE-013", row="| `GATE-014` | dup |"))
    with pytest.raises(edit.EditError, match="cells"):
        edit.md_table_row(MD, _md(after_key="GATE-013", row="| a | b | c |"))
    twice = MD + "\n| ID | X |\n|--|--|\n| GATE-014 | again |\n"
    with pytest.raises(edit.EditError, match="ambiguous"):
        edit.md_table_row(twice, _md(after_key="GATE-014", row="| z | y |"))
    out = edit.md_table_row(twice, _md(after_key="GATE-014", row="| z | y |", table=2))
    assert out.endswith("| GATE-014 | again |\n| z | y |\n")


# ------------------------------------------------------------------- text


def test_text_replace_exact_count() -> None:
    assert edit.text_replace("a b a", "a", "c", 2) == "c b c"
    with pytest.raises(edit.EditError, match=r"matched 2 time\(s\) \(lines: 1, 1\)"):
        edit.text_replace("a b a", "a", "c", 1)
    with pytest.raises(edit.EditError, match="matched 0"):
        edit.text_replace("a b a", "zz", "c", 1)
    with pytest.raises(edit.EditError, match="identical"):
        edit.text_replace("a", "a", "a", 1)


def test_text_cli_reads_old_new_from_files(tmp_path: Path, capsys) -> None:
    target = _write(tmp_path, "f.txt", "x = 'it''s'\r\nkeep\r\n")
    old = _write(tmp_path, "old", "x = 'it''s'")
    new = _write(tmp_path, "new", 'x = "quoted $HOME"')
    args = ["text", str(target), "replace", "--old-file", str(old), "--new-file", str(new)]
    assert _run(*args, "--dry-run") == 0
    assert target.read_bytes() == b"x = 'it''s'\r\nkeep\r\n"
    assert _run(*args) == 0
    assert target.read_bytes() == b'x = "quoted $HOME"\r\nkeep\r\n'
    assert _run(*args) == 2  # old text is gone now: loud failure, not a no-op
    assert "matched 0" in capsys.readouterr().err


def test_usage_errors_exit_64(tmp_path: Path) -> None:
    assert _run("toml") == 64
    assert _run("text", str(tmp_path / "nope"), "replace", "--old", "a", "--new", "b") == 2
