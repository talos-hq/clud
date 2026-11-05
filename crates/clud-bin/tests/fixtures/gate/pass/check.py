"""A real passing fixture: compile the repository's source and exercise its sum."""
from pathlib import Path

source = Path("sum.py").read_text(encoding="utf-8")
namespace = {}
exec(compile(source, "sum.py", "exec"), namespace)
assert namespace["add"](2, 3) == 5
