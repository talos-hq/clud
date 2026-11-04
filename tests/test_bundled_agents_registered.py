"""Every bundled agent file must be registered, or it is never installed.

The grind-chaperone agent shipped without a ``BUNDLED_CLAUDE_FILES`` entry,
so every enqueued /grind goal ended "agent type 'grind-chaperone' not found".
This reads the source tree, which the Rust test bundle does not carry.
"""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
AGENTS = ROOT / "crates" / "clud-bin" / "assets" / "agents"
REGISTRY = ROOT / "crates" / "clud-bin" / "src" / "claude_files.rs"


def test_every_agent_asset_is_registered() -> None:
    registry = REGISTRY.read_text(encoding="utf-8")
    agents = sorted(p.name for p in AGENTS.glob("*.md") if p.name != "README.md")
    assert agents, AGENTS
    missing = [name for name in agents if f'rel_path: "agents/{name}"' not in registry]
    assert missing == [], f"not in BUNDLED_CLAUDE_FILES, so never installed: {missing}"
