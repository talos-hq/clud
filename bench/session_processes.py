"""Read-only Linux prototype for matching processes to a clud launch."""

from __future__ import annotations

import argparse
import json
from dataclasses import asdict, dataclass
from pathlib import Path


@dataclass(frozen=True)
class Process:
    pid: int
    parent: int
    session: int
    start: int
    state: str
    name: str
    origin: int | None = None


def read_stat(path: Path) -> Process:
    prefix, fields = path.read_text(errors="replace").rsplit(") ", 1)
    pid, name = prefix.split(" (", 1)
    state, parent, _, session, *rest = fields.split()
    return Process(int(pid), int(parent), int(session), int(rest[15]), state, name)


def read_process(path: Path) -> Process:
    first = read_stat(path / "stat")
    try:
        variables = (path / "environ").read_bytes().split(b"\0")
    except OSError:
        variables = []
    value = next(
        (item.removeprefix(b"RUNNING_PROCESS_ORIGINATOR=CLUD:") for item in variables
         if item.startswith(b"RUNNING_PROCESS_ORIGINATOR=CLUD:")),
        b"",
    )
    last = read_stat(path / "stat")
    if first.start != last.start:
        raise ValueError("process changed while sampling")
    return Process(
        last.pid, last.parent, last.session, last.start, last.state, last.name,
        int(value) if value.isdigit() and len(value) <= 10 else None,
    )


def snapshot(proc: Path) -> dict[int, Process]:
    processes = {}
    for path in proc.iterdir():
        if path.name.isdecimal():
            try:
                process = read_process(path)
            except (OSError, ValueError, IndexError):
                continue
            processes[process.pid] = process
    return processes


def match_session(
    processes: dict[int, Process], owner: int, owner_start: int,
    *, next_owner_start: int | None = None,
) -> list[dict]:
    """Use start ticks from this boot, including for an exited launch's owner."""
    root = processes.get(owner)
    root_matches = root is not None and root.start == owner_start
    matched = {owner: "session root"} if root_matches else {}
    upper = next_owner_start
    if root is not None and root.start > owner_start:
        upper = min(upper, root.start) if upper is not None else root.start
    ordered = sorted(processes.values(), key=lambda row: (row.start, row.pid))
    eligible = [
        row for row in ordered if row.pid != owner and row.start >= owner_start
        and (upper is None or row.start < upper)
    ]
    matched.update((row.pid, "originator tag") for row in eligible if row.origin == owner)
    # Same-tick forks can have wrapped PIDs, so walk until the matched graph is closed.
    changed = True
    while changed:
        changed = False
        for row in eligible:
            parent = processes.get(row.parent)
            if (
                row.pid not in matched and row.origin is None and row.parent in matched
                and parent is not None and parent.start <= row.start
            ):
                matched[row.pid] = "parent chain"
                changed = True
    return [
        {**asdict(row), "matched_by": matched[row.pid], "zombie": row.state == "Z"}
        for row in ordered if row.pid in matched
    ]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("owner", type=int, help="CLUD originator PID")
    parser.add_argument("--owner-start", type=int, help="recorded start ticks for an exited owner")
    parser.add_argument("--next-owner-start", type=int, help="known PID reuse boundary in ticks")
    args = parser.parse_args()
    processes = snapshot(Path("/proc"))
    root = processes.get(args.owner)
    start = args.owner_start
    if start is None:
        if root is None or root.name != "clud" or root.state == "Z":
            parser.error("provide the original owner's start ticks for an exited launch")
        start = root.start
    print(json.dumps(
        match_session(processes, args.owner, start, next_owner_start=args.next_owner_start),
        indent=2,
    ))


if __name__ == "__main__":
    main()
