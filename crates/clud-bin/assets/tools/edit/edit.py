#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# managed-by: clud
"""edit/edit.py — structured, fail-loud edits for config files.

Use this instead of ad-hoc `s.replace(...)` / `s.index(...)` splices in
heredocs, which silently do nothing when the text drifts or hit the wrong
occurrence. Every verb resolves exactly one target or exits non-zero.

Usage (through `"$CLUD_EXE" tool run edit/edit.py -- ...`):

  edit.py toml FILE get PATH
  edit.py toml FILE set PATH VALUE [--string] [--create] [--dry-run]
  edit.py toml FILE delete PATH [--dry-run]
      PATH is dotted keys; an array-of-tables element is chosen with
      `name[key=value]` (value matched as a string or bare literal) or
      `name[INDEX]`: 'host[name=dell].lane[name=obs-rust].dispatch.max'.
      VALUE is a TOML literal (8, true, "text", [1, 2]); --string takes it
      as a raw string. Comments and untouched lines are kept byte-for-byte.

  edit.py nix FILE get PATH
  edit.py nix FILE set PATH EXPR [--dry-run]
      PATH is the full attribute path ('services.x.lanes."obs-rust".max'
      or unquoted 'services.x.lanes.obs-rust.max'). Bindings written as
      `a.b = ...;`, nested `{ ... }` blocks and inline `{ a = 1; b = 2; }`
      sets all resolve. EXPR is inserted verbatim.

  edit.py md FILE table-row (--after-key K | --before-key K |
          --replace-key K | --append) --row ROW
          [--key-column N] [--table N] [--dry-run]
      Keys compare on the stripped cell text with backticks removed.
      --key-column is 1-based (default 1); --table (1-based) picks a table
      when several in the file carry the key.

  edit.py text FILE replace (--old S | --old-file P) (--new S | --new-file P)
          [--expect-count N] [--dry-run]
      Exact, non-regex replacement of every occurrence; fails unless the
      match count equals N (default 1).

--dry-run prints a unified diff and writes nothing. A real edit also
prints the diff. Writes are atomic (temp file + rename); line endings and
encoding (UTF-8) are preserved.

Exit codes:
  0   edited (or `get` printed the value; or the value was already equal,
      reported as `unchanged:` on stderr)
  2   target missing, ambiguous, or the edit would not round-trip
  64  usage error
"""

from __future__ import annotations

import argparse
import difflib
import os
import re
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

import tomllib


class EditError(Exception):
    """A missing or ambiguous target; exit 2."""


# --------------------------------------------------------------------- io


def read_text(path: Path) -> str:
    if not path.is_file():
        raise EditError(f"{path}: no such file")
    with path.open(encoding="utf-8", newline="") as fh:
        return fh.read()


def write_text(path: Path, text: str) -> None:
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="") as fh:
            fh.write(text)
        if path.exists():
            os.chmod(tmp, path.stat().st_mode & 0o7777)
        os.replace(tmp, path)
    except BaseException:
        if os.path.exists(tmp):
            os.unlink(tmp)
        raise


def unified(path: Path, old: str, new: str) -> str:
    return "".join(
        difflib.unified_diff(
            old.splitlines(keepends=True),
            new.splitlines(keepends=True),
            fromfile=f"a/{path}",
            tofile=f"b/{path}",
        )
    )


def finish(path: Path, old: str, new: str, dry_run: bool, what: str) -> int:
    if old == new:
        print(f"unchanged: {path}: {what} already has that value", file=sys.stderr)
        return 0
    sys.stdout.write(unified(path, old, new))
    if dry_run:
        print(f"dry-run: {path}: would {what}", file=sys.stderr)
    else:
        write_text(path, new)
        print(f"edited: {path}: {what}", file=sys.stderr)
    return 0


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


# ------------------------------------------------------------------- text


def text_replace(text: str, old: str, new: str, expect: int) -> str:
    if not old:
        raise EditError("--old is empty")
    if old == new:
        raise EditError("--old and --new are identical; nothing to do")
    starts = []
    pos = text.find(old)
    while pos != -1:
        starts.append(pos)
        pos = text.find(old, pos + len(old))
    if len(starts) != expect:
        lines = ", ".join(str(line_of(text, s)) for s in starts[:20]) or "none"
        raise EditError(
            f"--old matched {len(starts)} time(s) (lines: {lines}); --expect-count is {expect}"
        )
    return text.replace(old, new)


# --------------------------------------------------------------- markdown


def split_cells(row: str) -> list[str]:
    body = row.strip()
    if not (body.startswith("|") and body.endswith("|")) or len(body) < 2:
        raise EditError(f"not a table row (must start and end with '|'): {row!r}")
    cells, cur, i, tick = [], "", 1, False
    while i < len(body) - 1:
        ch = body[i]
        if ch == "\\" and i + 1 < len(body) - 1:
            cur += body[i : i + 2]
            i += 2
            continue
        if ch == "`":
            tick = not tick
        if ch == "|" and not tick:
            cells.append(cur)
            cur = ""
        else:
            cur += ch
        i += 1
    cells.append(cur)
    return cells


def cell_key(cell: str) -> str:
    return cell.strip().replace("`", "").strip()


SEP_RE = re.compile(r"^\s*\|?(\s*:?-{1,}:?\s*\|)+\s*:?-*:?\s*\|?\s*$")


@dataclass
class Table:
    header: int  # line index of header row
    rows: list[int]  # line indexes of body rows
    width: int


def find_tables(lines: list[str]) -> list[Table]:
    tables, i = [], 0
    while i + 1 < len(lines):
        if lines[i].lstrip().startswith("|") and SEP_RE.match(lines[i + 1].rstrip("\r\n")):
            width = len(split_cells(lines[i].rstrip("\r\n")))
            j = i + 2
            while j < len(lines) and lines[j].lstrip().startswith("|"):
                j += 1
            tables.append(Table(i, list(range(i + 2, j)), width))
            i = j
        else:
            i += 1
    return tables


def row_key(lines: list[str], idx: int, col: int) -> str | None:
    cells = split_cells(lines[idx].rstrip("\r\n"))
    return cell_key(cells[col]) if col < len(cells) else None


def pick_row(lines, tables, key, col, table_no):
    hits = []
    for t_no, t in enumerate(tables, 1):
        if table_no and t_no != table_no:
            continue
        hits += [(t, r) for r in t.rows if row_key(lines, r, col) == key]
    if not hits:
        raise EditError(f"no table row with key {key!r} in column {col + 1}")
    if len(hits) > 1:
        where = ", ".join(str(r + 1) for _, r in hits)
        raise EditError(f"key {key!r} is ambiguous (lines {where}); pass --table N")
    return hits[0]


def md_table_row(text: str, args: argparse.Namespace) -> str:
    lines = text.splitlines(keepends=True)
    tables = find_tables(lines)
    if not tables:
        raise EditError("no Markdown table in file")
    if args.table and not 1 <= args.table <= len(tables):
        raise EditError(f"--table {args.table}: file has {len(tables)} table(s)")
    col = args.key_column - 1
    row = args.row.rstrip("\r\n")
    new_key = cell_key(split_cells(row)[col]) if col < len(split_cells(row)) else None
    anchor = args.after_key or args.before_key or args.replace_key
    if args.append:
        if not args.table and len(tables) > 1:
            raise EditError(f"--append: file has {len(tables)} tables; pass --table N")
        table = tables[(args.table or 1) - 1]
        at = (table.rows[-1] if table.rows else table.header + 1) + 1
        replace = False
    else:
        table, idx = pick_row(lines, tables, anchor, col, args.table)
        at, replace = (
            (idx, True) if args.replace_key else (idx + (args.after_key is not None), False)
        )
    if len(split_cells(row)) != table.width:
        raise EditError(f"--row has {len(split_cells(row))} cells; the table has {table.width}")
    if not replace or new_key != anchor:
        dup = [r for r in table.rows if row_key(lines, r, col) == new_key]
        if dup:
            raise EditError(f"table already has a row keyed {new_key!r} (line {dup[0] + 1})")
    eol = "\r\n" if lines[table.header].endswith("\r\n") else "\n"
    if replace:
        lines[at] = row + eol
    else:
        if at > 0 and not lines[at - 1].endswith("\n"):
            lines[at - 1] += eol
        lines.insert(at, row + eol)
    return "".join(lines)


# ------------------------------------------------------------------- toml

SEG_RE = re.compile(
    r"""\s*(?:"((?:[^"\\]|\\.)*)"|'([^']*)'|([A-Za-z0-9_-]+))\s*(?:\[\s*([^\]]*?)\s*\])?\s*(?:\.|$)"""
)


@dataclass
class Seg:
    key: str
    sel: str | None  # "k=v" or "N" or None


def parse_path(path: str) -> list[Seg]:
    segs, pos = [], 0
    while pos < len(path):
        m = SEG_RE.match(path, pos)
        if not m or m.end() == pos:
            raise EditError(f"bad path {path!r} near {path[pos:]!r}")
        key = next(g for g in m.groups()[:3] if g is not None)
        segs.append(Seg(key, m.group(4)))
        pos = m.end()
    if not segs or path.rstrip().endswith("."):
        raise EditError(f"bad path {path!r}")
    return segs


def toml_keys(raw: str) -> list[str]:
    """Split a TOML dotted key (`a."b.c".d`) into parts."""
    parts = parse_path(raw.strip())
    if any(p.sel is not None for p in parts):
        raise EditError(f"unexpected '[' in key {raw!r}")
    return [p.key for p in parts]


HEADER_RE = re.compile(r"^\s*(\[\[|\[)\s*(.+?)\s*(\]\]|\])\s*(#.*)?$")
KV_RE = re.compile(r"""^(\s*)((?:"(?:[^"\\]|\\.)*"|'[^']*'|[A-Za-z0-9_.\s-])+?)\s*=""")


@dataclass
class Section:
    keys: list[str]
    is_array: bool
    start: int  # header line index, -1 for root
    end: int  # exclusive body end
    parent: int | None = None  # index in sections list


def toml_sections(lines: list[str]) -> list[Section]:
    secs = [Section([], False, -1, len(lines))]
    in_multi = None
    for i, line in enumerate(lines):
        in_multi = _multiline_state(line, in_multi)
        if in_multi is not None and not line.startswith(in_multi):
            continue
        m = HEADER_RE.match(line.rstrip("\r\n"))
        if m and in_multi is None:
            secs[-1].end = i
            secs.append(Section(toml_keys(m.group(2)), m.group(1) == "[[", i, len(lines)))
    for n, sec in enumerate(secs[1:], 1):
        best = 0
        for p in range(n - 1, 0, -1):
            pk = secs[p].keys
            if len(pk) < len(sec.keys) and sec.keys[: len(pk)] == pk:
                best = p
                break
        sec.parent = best
    return secs


def _multiline_state(line: str, state: str | None) -> str | None:
    for q in ('"""', "'''"):
        n = line.count(q)
        if state == q and n % 2 == 1:
            return None
        if state is None and n % 2 == 1:
            return q
    return state


def _sel_matches(lines: list[str], sec: Section, sel: str, index: int) -> bool:
    if sel.isdigit():
        return int(sel) == index
    if "=" not in sel:
        raise EditError(f"bad selector [{sel}]: use [key=value] or [INDEX]")
    k, v = (s.strip() for s in sel.split("=", 1))
    hit = _find_kv(lines, sec, [k])
    if hit is None:
        return False
    try:
        got = tomllib.loads("v = " + hit.value)["v"]
    except tomllib.TOMLDecodeError:
        return hit.value == v
    try:
        want = tomllib.loads("v = " + v)["v"]
    except tomllib.TOMLDecodeError:
        want = v
    return got == want or str(got) == v


@dataclass
class KV:
    line: int
    vstart: int  # column where the value text starts
    vend: int  # column where the value text ends (before comment/eol)
    value: str


def _value_span(line: str, start: int) -> tuple[int, int]:
    i, depth, quote = start, 0, None
    body = line.rstrip("\r\n")
    while i < len(body):
        ch = body[i]
        if quote:
            if ch == "\\" and quote == '"':
                i += 1
            elif ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
        elif ch in "[{":
            depth += 1
        elif ch in "]}":
            depth -= 1
        elif ch == "#":
            break
        i += 1
    if quote or depth:
        raise EditError("value spans several lines; use `edit text` for it")
    vend = len(body[:i].rstrip())
    vstart = start + (len(body[start:]) - len(body[start:].lstrip()))
    return vstart, vend


def _find_kv(lines: list[str], sec: Section, keys: list[str]) -> KV | None:
    hits = []
    for i in range(sec.start + 1, sec.end):
        m = KV_RE.match(lines[i])
        if not m or lines[i].lstrip().startswith("#"):
            continue
        if toml_keys(m.group(2)) == keys:
            vs, ve = _value_span(lines[i], m.end())
            hits.append(KV(i, vs, ve, lines[i][vs:ve]))
    if len(hits) > 1:
        raise EditError(f"key {'.'.join(keys)} appears {len(hits)} times in one table")
    return hits[0] if hits else None


def _children(secs, parent_idx, keys, is_array):
    return [
        n
        for n, s in enumerate(secs)
        if n and s.parent == parent_idx and s.keys == keys and s.is_array == is_array
    ]


def toml_resolve(lines: list[str], segs: list[Seg]) -> tuple[Section, list[str], int]:
    """Return (section holding the key, remaining dotted key, section idx)."""
    secs = toml_sections(lines)
    cur, base = 0, []
    last_sel = max((i for i, s in enumerate(segs) if s.sel is not None), default=-1)
    for i, seg in enumerate(segs[: last_sel + 1]):
        keys = [*base, seg.key]
        if seg.sel is None:
            base = keys
            continue
        elems = _children(secs, cur, keys, True)
        if not elems:
            raise EditError(f"no [[{'.'.join(keys)}]] tables under {_where(segs, i)}")
        match = [n for k, n in enumerate(elems) if _sel_matches(lines, secs[n], seg.sel, k)]
        if len(match) != 1:
            kind = "matches nothing" if not match else f"is ambiguous ({len(match)} tables)"
            raise EditError(f"selector {seg.key}[{seg.sel}] {kind}")
        cur, base = match[0], keys
    rest = [s.key for s in segs[last_sel + 1 :]]
    for cut in range(len(rest) - 1, 0, -1):
        sub = _children(secs, cur, base + rest[:cut], False)
        if len(sub) == 1:
            return secs[sub[0]], rest[cut:], sub[0]
    return secs[cur], rest, cur


def _where(segs: list[Seg], i: int) -> str:
    return ".".join(f"{s.key}[{s.sel}]" if s.sel else s.key for s in segs[:i]) or "the root"


def toml_value(raw: str, as_string: bool) -> str:
    if as_string:
        return '"' + raw.replace("\\", "\\\\").replace('"', '\\"') + '"'
    try:
        tomllib.loads("v = " + raw)
    except tomllib.TOMLDecodeError as err:
        raise EditError(
            f"VALUE {raw!r} is not a TOML literal ({err}); quote strings or pass --string"
        ) from err
    return raw


def _lookup(data, segs: list[Seg]):
    cur = data
    for seg in segs:
        if not isinstance(cur, dict) or seg.key not in cur:
            raise KeyError(seg.key)
        cur = cur[seg.key]
        if seg.sel is not None:
            cur = _select(cur, seg.sel)
    return cur


def _select(items, sel: str):
    if sel.isdigit():
        return items[int(sel)]
    k, v = (s.strip() for s in sel.split("=", 1))
    try:
        want = tomllib.loads("v = " + v)["v"]
    except tomllib.TOMLDecodeError:
        want = v
    hits = [t for t in items if k in t and (t[k] == want or str(t[k]) == v)]
    if len(hits) != 1:
        raise KeyError(sel)
    return hits[0]


def toml_edit(text: str, verb: str, path: str, value: str | None, create: bool) -> str:
    segs = parse_path(path)
    lines = text.splitlines(keepends=True)
    sec, rest, _ = toml_resolve(lines, segs)
    hit = _find_kv(lines, sec, rest)
    if verb == "get":
        if hit is None:
            raise EditError(f"{path}: not found")
        return hit.value
    if hit is None and not (verb == "set" and create):
        raise EditError(f"{path}: not found (pass --create to add it)")
    if verb == "delete":
        del lines[hit.line]
    elif hit is None:
        _insert_kv(lines, sec, rest, value)
    else:
        ln = lines[hit.line]
        lines[hit.line] = ln[: hit.vstart] + value + ln[hit.vend :]
    new = "".join(lines)
    _verify_toml(new, segs, verb, value)
    return new


def _insert_kv(lines: list[str], sec: Section, rest: list[str], value: str) -> None:
    at = sec.end
    while at - 1 > sec.start and not lines[at - 1].strip():
        at -= 1
    eol = "\r\n" if any(ln.endswith("\r\n") for ln in lines) else "\n"
    if at > 0 and not lines[at - 1].endswith("\n"):
        lines[at - 1] += eol
    key = ".".join(k if re.fullmatch(r"[A-Za-z0-9_-]+", k) else f'"{k}"' for k in rest)
    lines.insert(at, f"{key} = {value}{eol}")


def _verify_toml(new: str, segs: list[Seg], verb: str, value: str | None) -> None:
    try:
        data = tomllib.loads(new)
    except tomllib.TOMLDecodeError as err:
        raise EditError(f"edit would produce invalid TOML: {err}") from err
    try:
        got = _lookup(data, segs)
    except (KeyError, IndexError, TypeError):
        if verb == "delete":
            return
        raise EditError("edit did not land on the requested path; refusing") from None
    if verb == "delete" or got != tomllib.loads("v = " + value)["v"]:
        raise EditError("edit did not land on the requested path; refusing")


# -------------------------------------------------------------------- nix


@dataclass
class Binding:
    path: list[str]
    start: int  # offset of the value expression
    end: int  # offset of the terminating ';'


IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_'-]*")


class NixScanner:
    """Finds `attrpath = expr;` bindings in every attribute set."""

    def __init__(self, src: str) -> None:
        self.s = src
        self.bindings: list[Binding] = []

    def skip_ws(self, i: int) -> int:
        s = self.s
        while i < len(s):
            if s[i].isspace():
                i += 1
            elif s[i] == "#":
                j = s.find("\n", i)
                i = len(s) if j == -1 else j + 1
            elif s.startswith("/*", i):
                j = s.find("*/", i + 2)
                if j == -1:
                    raise EditError("unterminated /* comment")
                i = j + 2
            else:
                break
        return i

    def skip_string(self, i: int) -> int:
        s = self.s
        if s[i] == '"':
            i += 1
            while i < len(s) and s[i] != '"':
                if s[i] == "\\":
                    i += 1
                elif s.startswith("${", i):
                    i = self.skip_expr_until(i + 2, "}") + 1
                    continue
                i += 1
            if i >= len(s):
                raise EditError("unterminated string")
            return i + 1
        i += 2  # ''
        while i < len(s):
            if s.startswith("'''", i) or s.startswith("''$", i) or s.startswith("''\\", i):
                i += 3
            elif s.startswith("''", i):
                return i + 2
            elif s.startswith("${", i):
                i = self.skip_expr_until(i + 2, "}") + 1
            else:
                i += 1
        raise EditError("unterminated '' string")

    def skip_expr_until(self, i: int, stops: str, prefix: list[str] | None = None) -> int:
        """Scan an expression; return offset of the first top-level char in `stops`.

        `prefix` is the attribute path that attribute sets found here bind
        under; None means "do not record" (let bindings, patterns). An empty
        `stops` scans to end of file.
        """
        s, closers = self.s, {"{": "}", "[": "]", "(": ")"}
        while True:
            i = self.skip_ws(i)
            if i >= len(s):
                if not stops:
                    return i
                raise EditError(f"unexpected end of file (wanted one of {stops!r})")
            ch = s[i]
            if ch in stops:
                return i
            word = IDENT.match(s, i)
            if word:
                i = self.keyword(word)
            elif ch == '"' or s.startswith("''", i):
                i = self.skip_string(i)
            elif ch == "{":
                i = self.attrset_or_pattern(i, prefix)
            elif ch in "[(":
                i = self.skip_expr_until(i + 1, closers[ch], prefix) + 1
            elif ch in "})]":
                raise EditError(f"unbalanced {ch!r} at line {line_of(s, i)}")
            else:
                i += 1

    def keyword(self, word: re.Match) -> int:
        if word.group(0) in ("with", "assert"):
            return self.skip_expr_until(word.end(), ";") + 1
        if word.group(0) == "let":
            return self.let_bindings(word.end())
        return word.end()

    def let_bindings(self, i: int) -> int:
        while True:
            i = self.skip_ws(i)
            m = IDENT.match(self.s, i)
            if m and m.group(0) == "in":
                return m.end()
            if m and m.group(0) == "inherit":
                i = self.skip_expr_until(m.end(), ";") + 1
                continue
            _, i = self.attrpath(i)
            if not self.s.startswith("=", i):
                raise EditError(f"expected '=' in let at line {line_of(self.s, i)}")
            i = self.skip_expr_until(i + 1, ";") + 1

    def attrset_or_pattern(self, i: int, prefix: list[str] | None) -> int:
        j = self.skip_ws(i + 1)
        if j < len(self.s) and self.s[j] == "}":
            return j + 1
        if self._looks_like_pattern(j):
            return self.skip_expr_until(i + 1, "}") + 1
        return self.attrset_body(j, prefix)

    def _looks_like_pattern(self, j: int) -> bool:
        if self.s.startswith("...", j):
            return True
        m = IDENT.match(self.s, j)
        if not m:
            return False
        k = self.skip_ws(m.end())
        return k < len(self.s) and self.s[k] in ",?}"

    def attrpath(self, i: int) -> tuple[list[str], int]:
        parts = []
        while True:
            i = self.skip_ws(i)
            if self.s.startswith('"', i):
                end = self.skip_string(i)
                parts.append(self.s[i + 1 : end - 1])
                i = end
            else:
                m = IDENT.match(self.s, i)
                if not m:
                    raise EditError(f"cannot parse attribute name at line {line_of(self.s, i)}")
                parts.append(m.group(0))
                i = m.end()
            i = self.skip_ws(i)
            if not self.s.startswith(".", i):
                return parts, i
            i += 1

    def attrset_body(self, i: int, prefix: list[str] | None) -> int:
        s = self.s
        while True:
            i = self.skip_ws(i)
            if i >= len(s):
                raise EditError("unterminated attribute set")
            if s[i] == "}":
                return i + 1
            m = IDENT.match(s, i)
            if m and m.group(0) == "inherit":
                i = self.skip_expr_until(m.end(), ";") + 1
                continue
            parts, i = self.attrpath(i)
            if not s.startswith("=", i):
                raise EditError(f"expected '=' after {'.'.join(parts)} at line {line_of(s, i)}")
            full = None if prefix is None else prefix + parts
            start = self.skip_ws(i + 1)
            end = self.skip_expr_until(start, ";", full)
            if full is not None:
                self.bindings.append(Binding(full, start, end))
            i = end + 1

    def scan(self) -> list[Binding]:
        self.skip_expr_until(0, "", [])
        return self.bindings


def nix_edit(text: str, verb: str, path: str, value: str | None) -> str:
    want = [s.key for s in parse_path(path)]
    if any(s.sel for s in parse_path(path)):
        raise EditError("nix paths take no [selectors]")
    bindings = NixScanner(text).scan()
    hits = [b for b in bindings if b.path == want]
    if not hits:
        raise EditError(f"{path}: not found{_nix_hint(bindings, want)}")
    if len(hits) > 1:
        where = ", ".join(str(line_of(text, b.start)) for b in hits)
        raise EditError(f"{path}: ambiguous, bound {len(hits)} times (lines {where})")
    b = hits[0]
    if verb == "get":
        return text[b.start : b.end].rstrip()
    old_val = text[b.start : b.end]
    trail = old_val[len(old_val.rstrip()) :]
    new = text[: b.start] + value.strip() + trail + text[b.end :]
    after = [x for x in NixScanner(new).scan() if x.path == want]
    if len(after) != 1 or new[after[0].start : after[0].end].rstrip() != value.strip():
        raise EditError("EXPR changes the file's structure (unbalanced or contains ';'); refusing")
    return new


def _nix_hint(bindings: list[Binding], want: list[str]) -> str:
    best = max((_common(b.path, want) for b in bindings), default=0)
    if not best:
        return ""
    return f" (deepest existing prefix: {'.'.join(want[:best])})"


def _common(a: list[str], b: list[str]) -> int:
    n = 0
    while n < min(len(a), len(b)) and a[n] == b[n]:
        n += 1
    return n


# -------------------------------------------------------------------- cli


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="edit.py", description=__doc__.split("\n\n")[0])
    fam = p.add_subparsers(dest="family", required=True)

    tp = fam.add_parser("toml")
    tp.add_argument("file", type=Path)
    t = tp.add_subparsers(dest="verb", required=True)
    for verb in ("get", "set", "delete"):
        sp = t.add_parser(verb)
        sp.add_argument("path")
        if verb == "set":
            sp.add_argument("value")
            sp.add_argument("--string", action="store_true")
            sp.add_argument("--create", action="store_true")
        if verb != "get":
            sp.add_argument("--dry-run", action="store_true")

    n = fam.add_parser("nix")
    n.add_argument("file", type=Path)
    nv = n.add_subparsers(dest="verb", required=True)
    nv.add_parser("get").add_argument("path")
    ns = nv.add_parser("set")
    ns.add_argument("path")
    ns.add_argument("value")
    ns.add_argument("--dry-run", action="store_true")

    m = fam.add_parser("md")
    m.add_argument("file", type=Path)
    mr = m.add_subparsers(dest="verb", required=True).add_parser("table-row")
    where = mr.add_mutually_exclusive_group(required=True)
    where.add_argument("--after-key")
    where.add_argument("--before-key")
    where.add_argument("--replace-key")
    where.add_argument("--append", action="store_true")
    mr.add_argument("--row", required=True)
    mr.add_argument("--key-column", type=int, default=1)
    mr.add_argument("--table", type=int)
    mr.add_argument("--dry-run", action="store_true")

    x = fam.add_parser("text")
    x.add_argument("file", type=Path)
    xr = x.add_subparsers(dest="verb", required=True).add_parser("replace")
    old = xr.add_mutually_exclusive_group(required=True)
    old.add_argument("--old")
    old.add_argument("--old-file", type=Path)
    new = xr.add_mutually_exclusive_group(required=True)
    new.add_argument("--new")
    new.add_argument("--new-file", type=Path)
    xr.add_argument("--expect-count", type=int, default=1)
    xr.add_argument("--dry-run", action="store_true")
    return p


def run(args: argparse.Namespace) -> int:
    text = read_text(args.file)
    dry = getattr(args, "dry_run", False)
    if args.family == "toml":
        value = toml_value(args.value, args.string) if args.verb == "set" else None
        out = toml_edit(text, args.verb, args.path, value, getattr(args, "create", False))
    elif args.family == "nix":
        out = nix_edit(text, args.verb, args.path, getattr(args, "value", None))
    elif args.family == "md":
        if args.key_column < 1:
            raise EditError("--key-column is 1-based")
        out = md_table_row(text, args)
        return finish(args.file, text, out, dry, "insert/replace table row")
    else:
        old = read_text(args.old_file) if args.old_file else args.old
        new = read_text(args.new_file) if args.new_file else args.new
        if args.expect_count < 1:
            raise EditError("--expect-count must be >= 1")
        out = text_replace(text, old, new, args.expect_count)
        return finish(args.file, text, out, dry, f"replace {args.expect_count} occurrence(s)")
    if args.verb == "get":
        print(out)
        return 0
    return finish(args.file, text, out, dry, f"{args.verb} {args.path}")


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return 64 if exc.code else 0
    try:
        return run(args)
    except EditError as err:
        print(f"error: {args.file}: {err}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
