#!/usr/bin/env python
"""POSIX spike: deterministic rg evidence packs; no installed dependencies.

Continuation is a candidate index tied to a content fingerprint; repeat the same
query with --continue HANDLE. Internal rg output is local, never model context.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import tempfile
from pathlib import Path

STOP = set(
    (
        "where what which is the a an for of in to and file find registered decodes merges matcher"
    ).split()
)
DEF = re.compile(
    r"^\s*(?:(?:pub(?:\([^)]*\))?|async|unsafe)\s+)*(?:fn|def|class|struct|enum|trait)\s+(\w+)|^(#{1,6})\s+(.+)"
)


def run(argv):
    """Drain to a temporary file so a full pipe cannot deadlock waitpid."""
    with tempfile.TemporaryFile() as out:
        pid = os.posix_spawnp(
            argv[0],
            argv,
            os.environ,
            file_actions=[
                (os.POSIX_SPAWN_DUP2, out.fileno(), 1),
                (os.POSIX_SPAWN_DUP2, out.fileno(), 2),
            ],
        )
        _, status = os.waitpid(pid, 0)
        out.seek(0)
        data = out.read()
    if os.waitstatus_to_exitcode(status) not in (0, 1):
        raise RuntimeError(data.decode(errors="replace"))
    return data.decode(errors="replace")


def terms(query):
    raw = re.findall(r"[A-Za-z_][\w./-]*", query)
    result = []
    for word in raw:
        split = re.sub(r"([a-z])([A-Z])", r"\1_\2", word)
        result.extend([word.lower(), *re.split(r"[_./-]", split.lower())])
    return sorted(set(result) - STOP - {""})


def infer_scopes(files, words):
    """Prefer exact layout stems; broad fallback preserves discovery."""
    exact = [p for p in files if Path(p).stem.lower() in words]
    if exact:
        return sorted(exact)[:80]
    return ["crates", "src", "docs", "ci"]


def search(query, scopes=()):
    words = terms(query)
    files = run(["rg", "--files", "-g", "!code/**", "-g", "!docs/spikes/**"]).splitlines()
    hints = [p for p in files if any(w in Path(p).name.lower() for w in words if len(w) > 3)]
    patterns = [re.escape(w) for w in words if len(w) > 2]
    if not patterns:
        return [], []
    common = [
        "rg",
        "-n",
        "--no-heading",
        "--with-filename",
        "-i",
        "-m",
        "40",
        "-g",
        "*.rs",
        "-g",
        "*.py",
        "-g",
        "*.md",
        "-g",
        "!code/**",
        "-g",
        "!docs/spikes/**",
    ]
    roots = list(scopes) or infer_scopes(files, words)
    passes = [[*common, "-e", "|".join(patterns), *roots]]
    if hints and not scopes and len(roots) == 4 and roots[0] == "crates":
        passes.append([*common, "-e", "|".join(patterns), *hints[:80]])
    passes.append([*common, "-F", "-e", query, *roots])
    identifiers = re.findall(r"[A-Za-z_][A-Za-z_0-9]*", query)
    exact = "|".join(r"\b" + re.escape(x) + r"\b" for x in identifiers if x.lower() not in STOP)
    if exact:
        passes.append([*common, "-e", exact, *roots])
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        outputs = list(pool.map(run, passes))
    hits = {}
    for output in outputs:
        for row in output.splitlines():
            match = re.match(r"(.+?):(\d+):(.*)", row)
            if match:
                path, line, text = match.groups()
                hits[path, int(line)] = text
    # Exact whole-token query terms bypass search caps, ranking and window deduplication.
    exact_pattern = r"\b(?:" + "|".join(re.escape(w) for w in words) + r")\b"
    unlimited = common[:]
    at = unlimited.index("-m")
    del unlimited[at : at + 2]
    exact_output = run([*unlimited, "-e", exact_pattern, *roots])
    anchors = []
    for row in exact_output.splitlines():
        match = re.match(r"(.+?):(\d+):(.*)", row)
        if match:
            path, line, text = match.groups()
            matched = sorted(set(re.findall(exact_pattern, text, re.I)))
            anchors.append(dict(path=path, line=int(line), anchors=matched))
    anchors.sort(key=lambda x: (x["path"], x["line"]))
    return anchors, rank(hits, words)


def rank(hits, words):
    candidates = []
    cache = {}
    for (path, line), text in hits.items():
        if path not in cache:
            lines = Path(path).read_text(errors="replace").splitlines()
            definitions = [
                (i + 1, DEF.match(s))
                for i, s in enumerate(lines)
                if DEF.match(s) and (not s.lstrip().startswith("#") or path.endswith(".md"))
            ]
            cache[path] = lines, definitions
        lines, definitions = cache[path]
        previous = [(n, m) for n, m in definitions if n <= line]
        start, match = previous[-1] if previous else (line, None)
        following = [n for n, _ in definitions if n > line]
        end = following[0] - 1 if following else len(lines)
        # Cheap lexical boundary expansion only; never claim AST completeness.
        if end - start > 45 or line - start > 40:
            start, end = max(1, line - 2), min(len(lines), line + 4)
        symbol = (match.group(1) or match.group(3)) if match else "(context)"
        coverage = sum(w in text.lower() for w in words)
        path_score = sum(w in path.lower() for w in words)
        heading = sum(w in symbol.lower() for w in words)
        definition = bool(DEF.match(text))
        doc_comment = text.lstrip().startswith(("///", "//!"))
        call_site = bool(re.search(r"\w+\s*\(", text))
        category = 3 if definition else 2 if doc_comment else 1 if call_site else 0
        score = category * 100 + coverage * 4 + min(path_score, 4) * 2 + heading * 3
        score -= int("test" in path) * 5
        reason = (
            f"terms={coverage};path={path_score};heading={heading};"
            f"kind={category};distance={line - start}"
        )
        start, end = max(start, line - 1), min(end, line + 2)
        candidates.append(
            dict(
                path=path,
                start=start,
                end=end,
                symbol=symbol,
                reason=reason,
                score=score,
                distance=line - start,
                lines=lines,
            )
        )
    candidates.sort(key=lambda x: (-x["score"], x["distance"], x["path"], x["start"]))
    merged = []
    for item in candidates:
        overlap = next(
            (
                x
                for x in merged
                if x["path"] == item["path"]
                and x["start"] <= item["end"] + 1
                and item["start"] <= x["end"] + 1
            ),
            None,
        )
        if overlap:
            # Preserve rank; merge bounded windows, drop redundant wider windows.
            if max(overlap["end"], item["end"]) - min(overlap["start"], item["start"]) <= 5:
                overlap["start"] = min(overlap["start"], item["start"])
                overlap["end"] = max(overlap["end"], item["end"])
        else:
            merged.append(item)
    return merged


def render_context(items, budget, handle):
    fingerprint = hashlib.sha256(
        json.dumps(
            [
                (x["path"], x["start"], x["end"], x["lines"][x["start"] - 1 : x["end"]])
                for x in items
            ],
            ensure_ascii=False,
        ).encode()
    ).hexdigest()[:12]
    offset = 0
    if handle:
        stamp, index = handle.split(":")
        if stamp != fingerprint:
            raise ValueError("stale continuation: corpus or query changed")
        offset = int(index)
        if offset < 0 or offset > len(items):
            raise ValueError("invalid continuation index")
    # Reserve enough space for footer, even when no evidence fits.
    output = ""
    index = offset
    for item in items[offset : offset + 5]:
        header = (
            f"@ {item['path']}:{item['start']}-{item['end']} [{item['symbol']}] {item['reason']}\n"
        )
        body = "".join(
            f"{n}| {item['lines'][n - 1]}\n" for n in range(item["start"], item["end"] + 1)
        )
        block = header + body
        while len((output + block).encode()) > budget - 160 and body:
            body = "\n".join(body.split("\n")[:-2]) + "\n" if "\n" in body.rstrip("\n") else ""
            shown = len(body.splitlines())
            header = (
                f"@ {item['path']}:{item['start']}-{item['start'] + shown - 1} "
                f"[{item['symbol']}] {item['reason']};"
                f"omitted_lines={item['end'] - item['start'] + 1 - shown}\n"
            )
            block = header + body
        if not body or len((output + block).encode()) > budget - 160:
            break
        output += block
        index += 1
    footer = (
        f"TRUNCATED={index < len(items)} emitted={index - offset} "
        f"omitted={len(items) - index} continue={fingerprint}:{index}\n"
    )
    if index == offset and index < len(items):
        footer += "Next item exceeds budget; retry with larger --max-bytes.\n"
    return output + footer


def render(result, budget, handle):
    """The complete floor precedes optional ranked expansion, even on continuation."""
    anchors, items = result
    floor = "".join(
        f"! {a['path']}:{a['line']} anchors={','.join(a['anchors'])}\n" for a in anchors
    )
    remaining = budget - len(floor.encode())
    if remaining < 256:
        footer = (
            f"FLOOR anchors={len(anchors)} expansions=0 "
            f"budget_exceeded={len(floor.encode()) > budget}\n"
        )
        return floor + footer if len((floor + footer).encode()) <= budget else floor
    return floor + render_context(items, remaining, handle)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("query")
    parser.add_argument("--max-bytes", type=int, default=1536)
    parser.add_argument("--scope", action="append", default=[])
    parser.add_argument("--continue", dest="handle")
    args = parser.parse_args()
    if args.max_bytes < 256:
        parser.error("--max-bytes must be at least 256")
    print(render(search(args.query, args.scope), args.max_bytes, args.handle), end="")


if __name__ == "__main__":
    main()
