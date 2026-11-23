"""Lint rule: ban direct subprocess/PTY calls in product source.

All Rust and Python process execution must go through running-process.
This script scans product source and fails the build if a direct process API
is used.
"""

from __future__ import annotations

import ast
import os
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Patterns that indicate direct subprocess usage (banned in crates/)
BANNED_PATTERNS: list[tuple[str, str]] = [
    (r"\bstd::process::Command\b", "use running_process::NativeProcess instead"),
    (r"\bprocess::Command\b", "use running_process::NativeProcess instead"),
    (r"\bCommand::new\b", "use running_process::NativeProcess instead"),
    (r"\bstd::process::Stdio\b", "use running_process StdinMode/StderrMode instead"),
    (r"\bstd::process::Child\b", "use running_process::NativeProcess instead"),
    (r"\bstd::process::Output\b", "use running_process::NativeProcess instead"),
    (r"\buse std::process::\{", "use running_process instead of std::process"),
    # Tokio's async process API is also banned — running-process is the
    # single chokepoint. If async is needed, extend running-process.
    (r"\btokio::process\b", "use running_process::NativeProcess instead"),
    (r"\buse tokio::process\b", "use running_process instead of tokio::process"),
]

# Only std::process::exit is allowed (it's not subprocess spawning)
ALLOWED_PATTERNS: list[str] = [
    r"std::process::exit",
    r"process::exit",
]

# Product Python tools must delegate process-tree cleanup to running-process.
# These shell commands have divergent platform semantics and bypass its
# containment, verification, and diagnostic contract.
BANNED_PLATFORM_TREE_KILL_RE = re.compile(r"\b(?:taskkill|pkill|killall)\b", re.IGNORECASE)

COMMAND_BUILDER_MARKER = "running-process: command-builder"
COMMAND_IMPORT_RE = re.compile(r"^use std::process::Command;\s*(?://.*)?$")
COMMAND_BUILDER_RE = re.compile(
    r"^let\s+mut\s+(?P<name>[A-Za-z_]\w*)\s*=\s*Command::new\([^;]+\);\s*(?://.*)?$"
)


def is_allowed(line: str, previous_line: str = "") -> bool:
    """Check if the line only uses allowed std::process items."""
    if any(re.search(pat, line) for pat in ALLOWED_PATTERNS):
        return True
    # A std::process::Command may be used purely as a configuration builder
    # handed to running_process::spawn. Rustfmt can move the marker onto the
    # preceding line, so accept either location — but only for construction,
    # never for a raw `.spawn()` call. This is intentionally narrower than a
    # file exemption for production code.
    marked = COMMAND_BUILDER_MARKER in line or COMMAND_BUILDER_MARKER in previous_line
    if not marked:
        return False
    # Full-line matching prevents the marker from suppressing another banned
    # construct appended to the same line. Execution methods are checked both
    # here and across rustfmt-style multiline continuations in `scan_file`.
    return bool(COMMAND_IMPORT_RE.fullmatch(line) or COMMAND_BUILDER_RE.fullmatch(line))


def _rust_code_only(content: str) -> str:  # noqa: C901
    """Blank comments and string literals while preserving offsets/newlines."""
    out = list(content)
    index = 0
    block_depth = 0
    while index < len(content):
        if block_depth:
            if content.startswith("/*", index):
                out[index : index + 2] = "  "
                block_depth += 1
                index += 2
            elif content.startswith("*/", index):
                out[index : index + 2] = "  "
                block_depth -= 1
                index += 2
            else:
                if content[index] != "\n":
                    out[index] = " "
                index += 1
            continue

        if content.startswith("//", index):
            end = content.find("\n", index)
            end = len(content) if end == -1 else end
            out[index:end] = " " * (end - index)
            index = end
            continue
        if content.startswith("/*", index):
            out[index : index + 2] = "  "
            block_depth = 1
            index += 2
            continue

        char_literal = re.match(
            r"(?:b)?'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\}|.)|[^\\'\r\n])'",
            content[index:],
        )
        if char_literal:
            end = index + char_literal.end()
            out[index:end] = " " * (end - index)
            index = end
            continue

        raw = re.match(r'r(?P<hashes>#{0,255})"', content[index:])
        if raw:
            delimiter = '"' + raw.group("hashes")
            end = content.find(delimiter, index + raw.end())
            end = len(content) if end == -1 else end + len(delimiter)
            for pos in range(index, end):
                if content[pos] != "\n":
                    out[pos] = " "
            index = end
            continue

        if content[index] == '"':
            end = index + 1
            escaped = False
            while end < len(content):
                char = content[end]
                if char == '"' and not escaped:
                    end += 1
                    break
                escaped = char == "\\" and not escaped
                if char != "\\":
                    escaped = False
                end += 1
            for pos in range(index, end):
                if content[pos] != "\n":
                    out[pos] = " "
            index = end
            continue
        index += 1
    return "".join(out)


def scan_file(path: Path) -> list[tuple[int, str, str]]:  # noqa: C901
    """Scan a single file for banned patterns. Returns (line_num, line, reason)."""
    violations: list[tuple[int, str, str]] = []
    try:
        content = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return violations

    previous_line = ""
    command_builders: set[str] = set()
    for line_num, line in enumerate(content.splitlines(), start=1):
        stripped = line.strip()
        # Skip comments
        if stripped.startswith("//"):
            previous_line = line
            continue
        # Skip if it's an allowed usage
        if is_allowed(stripped, previous_line.strip()):
            if match := COMMAND_BUILDER_RE.fullmatch(stripped):
                command_builders.add(match.group("name"))
            previous_line = line
            continue
        for pattern, reason in BANNED_PATTERNS:
            if re.search(pattern, stripped):
                violations.append((line_num, stripped, reason))
                break  # One violation per line is enough
        previous_line = line

    # A marked Command is configuration-only: it may be handed to
    # running_process, but it may never execute directly. Match across
    # whitespace/newlines so rustfmt cannot turn a raw launch into a bypass.
    if command_builders:
        code_only = _rust_code_only(content)
        for name in command_builders:
            raw_execution = re.compile(
                rf"\b{re.escape(name)}\s*\.\s*(?:spawn|status|output)\s*\("
            )
            for match in raw_execution.finditer(code_only):
                line_num = code_only.count("\n", 0, match.start()) + 1
                line = content.splitlines()[line_num - 1].strip()
                violations.append(
                    (line_num, line, "hand std::process::Command to running_process")
                )

    return violations


def scan_platform_tree_kills(path: Path) -> list[tuple[int, str]]:
    """Reject direct platform tree-kill command arguments, not prose mentions."""
    try:
        content = path.read_text(encoding="utf-8")
        tree = ast.parse(content, filename=str(path))
    except (OSError, UnicodeDecodeError, SyntaxError):
        return []

    lines = content.splitlines()
    violations: list[tuple[int, str]] = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        for arg in node.args:
            if not isinstance(arg, ast.List | ast.Tuple) or not arg.elts:
                continue
            first = arg.elts[0]
            if not isinstance(first, ast.Constant) or not isinstance(first.value, str):
                continue
            if BANNED_PLATFORM_TREE_KILL_RE.fullmatch(first.value):
                line_num = first.lineno
                line = lines[line_num - 1].strip()
                violations.append((line_num, line))
    return violations


#: Directory components whose Python files are none of this repo's business:
#: virtualenvs, build output, caches, agent worktrees, and checkouts of other
#: people's repositories.
#:
#: Spell these exactly as they appear on disk. `Path.parts` yields each
#: component verbatim, so a leading dot is part of the name -- `extern-repos`
#: does not match `.extern-repos`, and an entry that never matches is an
#: exclusion that silently does nothing.
EXCLUDED_PYTHON_PARTS = frozenset(
    {
        ".git",
        ".venv",
        ".clud",
        ".extern-repos",
        "target",
        "__pycache__",
        "worktrees",
    }
)


def is_excluded_python_path(relative: Path) -> bool:
    """Whether a repo-relative path lies under an excluded directory."""
    return any(part in EXCLUDED_PYTHON_PARTS for part in relative.parts)


def iter_python_files() -> list[Path]:
    """Yield Python source without descending into ignored runtime trees."""
    paths: list[Path] = []
    for root, directories, files in os.walk(ROOT):
        directories[:] = [
            directory
            for directory in directories
            if directory not in EXCLUDED_PYTHON_PARTS
        ]
        paths.extend(Path(root, filename) for filename in files if filename.endswith(".py"))
    return sorted(paths)


def scan_python_file(path: Path) -> list[tuple[int, str, str]]:
    """Reject Python's raw subprocess module in product tooling."""
    try:
        content = path.read_text(encoding="utf-8")
        tree = ast.parse(content, filename=str(path))
    except (OSError, UnicodeDecodeError, SyntaxError):
        return []

    lines = content.splitlines()
    violations: list[tuple[int, str, str]] = []
    seen: set[int] = set()
    for node in ast.walk(tree):
        banned = False
        if isinstance(node, ast.Import):
            banned = any(alias.name == "subprocess" for alias in node.names)
        elif isinstance(node, ast.ImportFrom):
            banned = node.module == "subprocess"
        elif isinstance(node, ast.Attribute):
            banned = isinstance(node.value, ast.Name) and node.value.id == "subprocess"
        if not banned or node.lineno in seen:
            continue
        seen.add(node.lineno)
        line = lines[node.lineno - 1].strip() if node.lineno <= len(lines) else ""
        violations.append(
            (node.lineno, line, "use the Python running_process package instead")
        )
    return violations


def main() -> int:
    # Scan all .rs files in crates/ (not testbins/ — mocks can use std::process)
    crates_dir = ROOT / "crates"
    if not crates_dir.is_dir():
        print("No crates/ directory found, skipping banned import check.", file=sys.stderr)
        return 0

    # trampoline.rs is exempt — `relay_child_and_wait` must spawn the cached
    # clud with raw std::process::Command, because NativeProcess's
    # kill-on-close Job Object would capture everything the relayed clud
    # starts (#333). The daemon detach that used to live there now goes
    # through running_process::spawn_daemon_* (#1186).
    #
    # process_tree.rs is exempt — production code uses sysinfo (no subprocess
    # spawning), but the #[cfg(test)] tests deliberately use std::process::
    # Command to spawn fixture trees *without* Containment::Contained. Using
    # NativeProcess in those tests would attach a Job Object that already
    # kills descendants on close, masking whether kill_tree's own walk
    # actually works.
    # win32_hooking_probe.rs is exempt — #468 is a research-only ignored
    # integration test that deliberately constructs raw Win32 jobs, suspended
    # children, and injection targets. running-process containment would mask
    # exactly the primitives under measurement.
    #
    # shim_main.rs (the `clud-shim` personality of `clud`, #1551) is exempt —
    # its entire purpose is to `execvp`
    # (Unix) or `CreateProcess` (Windows) and replace itself with the
    # resolved Python interpreter or the real gh / pinned watcher. running-process's NativeProcess
    # always spawns under containment, which is precisely the wrong
    # semantics for a relay binary. See #406 / #409.
    #
    # ctrlc_signal_kinds.rs / ctrlc_windows_events.rs are exempt (#517) —
    # these tests send a real OS signal / console-control event to the
    # exact spawned pid (`libc::kill` / `GenerateConsoleCtrlEvent`) and
    # assert on clud's captured interrupt reason. NativeProcess always
    # spawns under containment (a Windows Job Object), which is
    # irrelevant to what's under test and would only add noise.
    #
    # cpu_banner.rs is exempt (#540) — production code uses sysinfo (no
    # subprocess spawning), but the #[cfg(test)] #[ignore]d sampler-cost
    # bench deliberately uses std::process::Command to spawn a 55-process
    # fixture subtree, mirroring the process_tree.rs exemption.
    #
    # tool_shell_lifecycle_windows.rs is exempt (#616) — it must build raw
    # process trees inside the production foreground Job Object. NativeProcess
    # would add its own containment and mask the completion-port lifecycle.
    #
    # reaper_daemon_survival_windows.rs is exempt (#674) — the daemon-survival
    # suite must build raw process trees inside the production foreground Job
    # Object, and its sccache-shaped case turns on the *absence* of the
    # RUNNING_PROCESS_IS_DAEMON marker. NativeProcess would attach its own
    # containment and set that marker, erasing the signal under test.
    # reaper_orphan_sweep_survival.rs is exempt (#688) — the cross-platform
    # half of the same suite. Its sccache-shaped stub must detach on its own
    # and carry an inherited CLUD: originator tag *without* the daemon marker;
    # NativeProcess would set that marker and erase the signal under test.
    # reaper_session_members.rs is exempt because a native process wrapper
    # can change containment and daemon markers in the session fixture.
    # reaper_batch_drain_windows.rs is exempt (#706) — the completion-port
    # drain test needs a burst of children to land in the *tracker's* job so
    # their notifications queue on its port. NativeProcess would attach its own
    # Job Object to each child, moving the notifications off the port under
    # test and leaving nothing to batch.
    #
    # process_identity.rs is exempt (#643) — one test
    # (`an_exited_process_is_dead_even_while_its_handle_remains_open`) must hold
    # the raw `std::process::Child` handle open *across* the liveness check.
    # On Windows an open handle keeps the PID reserved, which is precisely the
    # condition under test; NativeProcess manages its handle internally and does
    # not guarantee it survives `wait()`, so wrapping this would delete the
    # scenario rather than exercise it.
    # process_identity_zombie_tests.rs is exempt (#1993) — its Linux fixture
    # must keep a raw child unreaped with waitid(WNOWAIT). NativeProcess would
    # reap the child and erase the zombie state the identity probe must reject.
    # subprocess_capture_lifecycle_windows.rs is exempt (#634) — its compiled
    # fixture must spawn a descendant before any containment is attached. The
    # production side of the same test goes through ManagedSubprocess; using it
    # for the fixture would make the test green even if the race returned.
    # daemon_spawn_hygiene.rs is exempt (#1186) — it hands `clud` a
    # deliberately inheritable pipe fd to prove the daemon does not keep it
    # open. NativeProcess sweeps every fd above 2 in the child, which would
    # remove that descriptor before `clud` ever saw it and make the test pass
    # whether or not the daemon leaks.
    exempt = {
        "trampoline.rs",
        "process_identity.rs",
        "process_identity_zombie_tests.rs",
        "reaper_batch_drain_windows.rs",
        "reaper_daemon_survival_windows.rs",
        "reaper_orphan_sweep_survival.rs",
        "reaper_session_members.rs",
        "process_tree.rs",
        "win32_hooking_probe.rs",
        "shim_main.rs",
        "ctrlc_signal_kinds.rs",
        "ctrlc_windows_events.rs",
        "cpu_banner.rs",
        "tool_shell_lifecycle_windows.rs",
        "subprocess_capture_lifecycle_windows.rs",
        "daemon_spawn_hygiene.rs",
    }
    rs_files = sorted(crates_dir.rglob("*.rs"))
    total_violations = 0

    for path in rs_files:
        if path.name in exempt:
            continue
        violations = scan_file(path)
        for line_num, line, reason in violations:
            rel = path.relative_to(ROOT)
            print(f"{rel}:{line_num}: BANNED — {reason}", file=sys.stderr)
            print(f"  {line}", file=sys.stderr)
            total_violations += 1

    python_files = iter_python_files()
    for path in python_files:
        for line_num, line, reason in scan_python_file(path):
            rel = path.relative_to(ROOT)
            print(f"{rel}:{line_num}: BANNED â€” {reason}", file=sys.stderr)
            print(f"  {line}", file=sys.stderr)
            total_violations += 1
        for line_num, line in scan_platform_tree_kills(path):
            rel = path.relative_to(ROOT)
            print(
                f"{rel}:{line_num}: BANNED — use running-process cross-platform tree cleanup",
                file=sys.stderr,
            )
            print(f"  {line}", file=sys.stderr)
            total_violations += 1

    if total_violations > 0:
        print(
            f"\n{total_violations} banned import(s) found. "
            "All subprocess execution must use running-process.",
            file=sys.stderr,
        )
        return 1

    print("No banned subprocess imports found.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
