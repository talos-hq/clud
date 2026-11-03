#!/usr/bin/env python
"""Round 4: rerun both frozen sets without changing scopes, gold or recall@5."""

import hashlib
import json
import re
import shlex
import sys
from pathlib import Path

from retrieve_bench_heldout import baseline, covered, timed


def original_baseline(task):
    outputs = [timed(["sh", "-c", cmd]) for cmd in task["A"]["commands"]]
    read = shlex.split(task["A"]["commands"][1])
    items = [(read[-1], int(a), int(b)) for a, b in re.findall(r"(\d+),(\d+)p", " ".join(read))]
    return dict(
        commands=task["A"]["commands"],
        bytes=sum(len(out.encode()) for out, _ in outputs),
        seconds=sum(sec for _, sec in outputs),
        calls=2,
        items=items,
        evidence="".join(out for out, _ in outputs),
        recall_at_5=covered(task["ground_truth"], items[:5]),
    )


def measure(task, heldout):
    gold = task["ground_truth"]
    if isinstance(gold, dict):
        gold = [gold]
    task = dict(task, ground_truth=gold)
    for g in gold:
        data = Path(g["path"]).read_bytes()
        assert hashlib.sha256(data).hexdigest() == g["sha256"]
        assert g["anchor"] in data.decode().splitlines()[g["line"] - 1]
    a = baseline(task) if heldout else original_baseline(task)
    command = [sys.executable, "code/retrieve.py", task["query"]]
    for scope in task.get("scopes", []):
        command.extend(["--scope", scope])
    output, elapsed = timed(command)
    # Preserve actual output order: compact anchors are individual locations.
    items = []
    for row in output.splitlines():
        exact = re.match(r"^! (.+?):(\d+) anchors=", row)
        context = re.match(r"^@ (.+?):(\d+)-(\d+) ", row)
        if exact:
            items.append((exact[1], int(exact[2]), int(exact[2])))
        elif context:
            items.append((context[1], int(context[2]), int(context[3])))
    e = dict(
        command=shlex.join(command),
        bytes=len(output.encode()),
        calls=1,
        seconds=elapsed,
        items=items,
        evidence=output,
        recall_at_5=covered(gold, items[:5]),
        all_emitted_coverage=covered(gold, items),
        floor_anchors=sum(row.startswith("! ") for row in output.splitlines()),
    )
    for result in (a, e):
        result["followups"] = int(not result["recall_at_5"])
        if result["followups"]:
            repair = "; ".join(
                f"sed -n {max(1, g['line'] - 2)},{g['line'] + 4}p {shlex.quote(g['path'])}"
                for g in gold
            )
            body, duration = timed(["sh", "-c", repair])
            result.update(
                repair_command=repair, repair_bytes=len(body.encode()), repair_seconds=duration
            )
    return dict(
        id=task["id"],
        query=task["query"],
        scopes=task.get("scopes", []),
        ground_truth=gold,
        A=a,
        E3=e,
    )


def main():
    report = dict(schema=4, budget_bytes=1536, sets={})
    for name, filename in [
        ("original", "retrieve_bench.json"),
        ("heldout", "retrieve_bench_heldout.json"),
    ]:
        frozen = json.loads(Path(__file__).with_name(filename).read_text())
        tasks = [measure(t, name == "heldout") for t in frozen["tasks"]]
        aggregate = {
            arm: {
                key: sum(t[arm][key] for t in tasks)
                for key in ("bytes", "calls", "recall_at_5", "followups", "seconds")
            }
            for arm in ("A", "E3")
        }
        report["sets"][name] = dict(
            source_commit=frozen["source_commit"], tasks=tasks, aggregate=aggregate
        )
        print(name, json.dumps(aggregate))
        for t in tasks:
            a, e = t["A"], t["E3"]
            print(
                f"| {t['id']} | {a['bytes']:,} / {e['bytes']:,} | 2 / 1 | "
                f"{int(a['recall_at_5'])} / {int(e['recall_at_5'])} | "
                f"{a['followups']} / {e['followups']} | {a['seconds']:.4f} / {e['seconds']:.4f} |"
            )
    Path(__file__).with_name("retrieve_bench_round4.json").write_text(
        json.dumps(report, indent=2) + "\n"
    )


if __name__ == "__main__":
    main()
