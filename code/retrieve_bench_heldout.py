#!/usr/bin/env python
"""Reproduce the frozen matched-scope Round 3 benchmark (local reads only)."""

import hashlib
import json
import re
import shlex
import sys
import time
from pathlib import Path

import retrieve


def timed(argv):
    start = time.perf_counter()
    output = retrieve.run(argv)
    return output, time.perf_counter() - start


def covered(gold, items):
    return all(any(p == g["path"] and a <= g["line"] <= b for p, a, b in items) for g in gold)


def baseline(task):
    pattern = task["pattern"]
    command = [
        "rg",
        "--sort",
        "path",
        "-n",
        "--with-filename",
        "-i",
        "-e",
        pattern,
        *task["scopes"],
    ]
    output, elapsed = timed(command)
    hits = [re.match(r"(.+?):(\d+):", row) for row in output.splitlines()]
    hits = [m for m in hits if m][:5]
    windows = [(m[1], max(1, int(m[2]) - 5), int(m[2]) + 25) for m in hits]
    merged = []
    for p, a, b in windows:
        if merged and merged[-1][0] == p and a <= merged[-1][2] + 1:
            merged[-1] = p, merged[-1][1], max(b, merged[-1][2])
        else:
            merged.append((p, a, b))
    read = "; ".join(f"sed -n {a},{b}p {shlex.quote(p)}" for p, a, b in merged) or ":"
    body, duration = timed(["sh", "-c", read])
    return dict(
        commands=[shlex.join(command), read],
        bytes=len((output + body).encode()),
        calls=2,
        seconds=elapsed + duration,
        items=merged,
        recall_at_5=covered(task["ground_truth"], windows),
        evidence=output + body,
        lexical_first5_hit=covered(
            task["ground_truth"], [(m[1], int(m[2]), int(m[2])) for m in hits]
        ),
    )


def main():
    path = Path(__file__).with_name("retrieve_bench_heldout.json")
    bench = json.loads(path.read_text())
    for task in bench["tasks"]:
        for gold in task["ground_truth"]:
            data = Path(gold["path"]).read_bytes()
            assert hashlib.sha256(data).hexdigest() == gold["sha256"]
            assert data.decode().splitlines()[gold["line"] - 1] == gold["anchor"]
        task["A"] = baseline(task)
        cmd = [sys.executable, "code/retrieve.py", task["query"]]
        for scope in task["scopes"]:
            cmd.extend(["--scope", scope])
        output, elapsed = timed(cmd)
        items = [
            (p, int(a), int(b)) for p, a, b in re.findall(r"^@ (.+?):(\d+)-(\d+) ", output, re.M)
        ]
        assert len(output.encode()) <= 1536
        task["E2"] = dict(
            command=shlex.join(cmd),
            bytes=len(output.encode()),
            calls=1,
            seconds=elapsed,
            items=items,
            evidence=output,
            recall_at_5=covered(task["ground_truth"], items[:5]),
        )
        for arm in ("A", "E2"):
            result = task[arm]
            result["followups"] = int(not result["recall_at_5"])
            if result["followups"]:
                repair = "; ".join(
                    f"sed -n {max(1, g['line'] - 2)},{g['line'] + 4}p {shlex.quote(g['path'])}"
                    for g in task["ground_truth"]
                )
                body, duration = timed(["sh", "-c", repair])
                result.update(
                    repair_command=repair, repair_bytes=len(body.encode()), repair_seconds=duration
                )
        a, e = task["A"], task["E2"]
        print(
            f"| {task['id']} | {a['bytes']} / {e['bytes']} | 2 / 1 | "
            f"{int(a['recall_at_5'])} / {int(e['recall_at_5'])} | "
            f"{a['followups']} / {e['followups']} | "
            f"{a['seconds']:.4f} / {e['seconds']:.4f} |"
        )
    path.write_text(json.dumps(bench, indent=2) + "\n")
    for arm in ("A", "E2"):
        print(
            arm,
            {
                k: sum(t[arm][k] for t in bench["tasks"])
                for k in ("bytes", "calls", "recall_at_5", "followups", "seconds")
            },
        )


if __name__ == "__main__":
    main()
