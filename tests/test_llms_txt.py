"""`llms.txt` (llmstxt.org) stays a working, compact index for agents.

Every link must name a file in this repository, the MCP tool list must match
the tools `src/clud/mcp_server.py` registers, and the agent entry points must
link the index and the generated CLI grammar (docs/cli-grammar.md).
"""

from __future__ import annotations

import ast
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LLMS = ROOT / "llms.txt"
RAW = "https://raw.githubusercontent.com/talos-hq/clud/main/"
GRAMMAR = "docs/cli-grammar.md"


def _text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _mcp_tools() -> set[str]:
    """Names of the functions decorated with `@server.tool()`."""
    tree = ast.parse(_text(ROOT / "src" / "clud" / "mcp_server.py"))
    tools = set()
    for node in tree.body:
        if not isinstance(node, ast.AsyncFunctionDef | ast.FunctionDef):
            continue
        for decorator in node.decorator_list:
            call = decorator.func if isinstance(decorator, ast.Call) else decorator
            if isinstance(call, ast.Attribute) and call.attr == "tool":
                tools.add(node.name)
    return tools


def test_llms_txt_has_the_llmstxt_layout() -> None:
    lines = _text(LLMS).splitlines()
    assert lines[0] == "# clud"
    assert lines[2].startswith("> ")
    assert not any(line.startswith("# ") for line in lines[1:])
    sections = [line[3:] for line in lines if line.startswith("## ")]
    assert sections[0] == "Docs"
    assert sections[-1] == "Optional"


def test_llms_txt_stays_compact() -> None:
    assert len(_text(LLMS).splitlines()) <= 60


def test_every_llms_txt_link_names_a_repository_file() -> None:
    links = re.findall(re.escape(RAW) + r"([^)\s]+)", _text(LLMS))
    assert links[0] == GRAMMAR
    for relative in links:
        assert (ROOT / relative).is_file(), relative


def test_llms_txt_lists_every_mcp_tool() -> None:
    tools = _mcp_tools()
    assert {"session_start", "board_read", "serve_start"} <= tools
    line = next(line for line in _text(LLMS).splitlines() if "mcp_server.py" in line)
    listed = set(re.findall(r"`([a-z_]+)`", line.split("Tools:", 1)[1]))
    assert listed == tools


def test_readme_mcp_table_lists_every_mcp_tool() -> None:
    readme = _text(ROOT / "README.md")
    table = readme.split("| Discoverable MCP tool |", 1)[1].split("\n\n", 1)[0]
    first_cells = [row.split("|")[1] for row in table.splitlines()]
    rows = {name for cell in first_cells for name in re.findall(r"`([a-z_]+)`", cell)}
    assert rows == _mcp_tools()


def test_agent_entry_points_link_the_index_and_grammar() -> None:
    for name in ("README.md", "CLAUDE.md", "AGENTS.md"):
        text = _text(ROOT / name)
        assert "llms.txt" in text, name
        assert GRAMMAR in text, name
