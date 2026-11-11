"""Inspect or explicitly disable a known legacy local-gate pre-push hook.

The hook is shared by linked worktrees. Never change an unrecognized hook or
an enforcing repository policy; disabling is an explicit local user action.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import os
import secrets
import stat
import sys
from pathlib import Path

import tomllib
from running_process import TimeoutExpired

from ci.process import run

# Exact bytes of the durable uvx hook observed in #2265. A marker alone is
# insufficient: users may have added their own commands to a managed hook.
KNOWN_LEGACY_SHA256 = frozenset(
    {
        "aea476fed787e6f9051e86ceb060b0ad9fbccaec801536a4ac56a7b04abf251c",  # LF
        "8d697e6bffefb56db116c90ebe89c72df8681dd43fc9a1a5f9b317de25396b28",  # CRLF
    }
)
MAX_HOOK_BYTES = 32768


def hook_path(repo: Path, *, env: dict[str, str] | None = None) -> Path:
    result = run(
        ["git", "rev-parse", "--git-path", "hooks/pre-push"],
        cwd=repo,
        env=env,
        capture_output=True,
        text=True,
        check=False,
        timeout=10,
    )
    if result.returncode != 0:
        raise ValueError("git could not determine the pre-push hook path")
    raw = result.stdout.removesuffix("\n").removesuffix("\r")
    if not raw or any(ord(character) < 32 or ord(character) == 127 for character in raw):
        raise ValueError("git returned an empty or unsafe hook path")
    path = Path(raw)
    return path if path.is_absolute() else (repo / path).absolute()


def _read_regular_hook(hook: Path) -> tuple[bytes, os.stat_result] | None:
    try:
        before = hook.lstat()
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(before.st_mode):
        raise ValueError("pre-push hook must be a regular file, not a link or special file")
    if before.st_size > MAX_HOOK_BYTES:
        raise ValueError("pre-push hook is too large to identify safely")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(hook, flags)
    with os.fdopen(fd, "rb") as stream:
        opened = os.fstat(stream.fileno())
        if not stat.S_ISREG(opened.st_mode) or (opened.st_dev, opened.st_ino) != (
            before.st_dev,
            before.st_ino,
        ):
            raise ValueError("pre-push hook changed while opening")
        contents = stream.read(MAX_HOOK_BYTES + 1)
    if len(contents) > MAX_HOOK_BYTES:
        raise ValueError("pre-push hook is too large to identify safely")
    return contents, opened


def _rename_noreplace(source: Path, destination: Path) -> None:
    """Atomically move without replacing an existing destination, or fail closed."""
    if os.name == "nt":
        source.rename(destination)  # Windows rename fails when destination exists.
        return
    libc = ctypes.CDLL(None, use_errno=True)
    if sys.platform.startswith("linux"):
        function = getattr(libc, "renameat2", None)
        if function is None:
            raise OSError("atomic no-replace rename is unavailable")
        function.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        result = function(-100, os.fsencode(source), -100, os.fsencode(destination), 1)
    elif sys.platform == "darwin":
        function = getattr(libc, "renamex_np", None)
        if function is None:
            raise OSError("atomic no-replace rename is unavailable")
        function.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
        result = function(os.fsencode(source), os.fsencode(destination), 4)
    else:
        raise OSError("atomic no-replace rename is unavailable on this host")
    if result != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(destination))


def disable_managed(hook: Path, mode: str) -> Path | None:
    """Move the exact known hook aside, only when explicitly requested."""
    if mode != "shadow":
        raise ValueError("refusing to disable a hook without a shadow local-gate policy")
    found = _read_regular_hook(hook)
    if found is None:
        return None
    contents, opened = found
    if hashlib.sha256(contents).hexdigest() not in KNOWN_LEGACY_SHA256:
        raise ValueError("unrecognized pre-push hook; inspect it and preserve it manually")
    current = hook.lstat()
    if not stat.S_ISREG(current.st_mode) or (
        current.st_dev,
        current.st_ino,
        current.st_size,
        current.st_mtime_ns,
        current.st_ctime_ns,
    ) != (opened.st_dev, opened.st_ino, opened.st_size, opened.st_mtime_ns, opened.st_ctime_ns):
        raise ValueError("pre-push hook changed before migration")
    # A random backup name avoids overwriting an earlier migration or a user file.
    backup = hook.with_name(f"pre-push.clud-disabled-{secrets.token_hex(16)}")
    if backup.exists() or backup.is_symlink():
        raise ValueError("backup path already exists; retry")
    _rename_noreplace(hook, backup)
    try:
        moved = _read_regular_hook(backup)
        matches = (
            moved is not None
            and moved[0] == contents
            and (moved[1].st_dev, moved[1].st_ino, moved[1].st_size, moved[1].st_mtime_ns)
            == (opened.st_dev, opened.st_ino, opened.st_size, opened.st_mtime_ns)
        )
    except (OSError, ValueError):
        matches = False
    if not matches:
        try:
            _rename_noreplace(backup, hook)
        except OSError as error:
            raise ValueError(
                f"hook changed; moved file preserved at {backup}; restore it manually"
            ) from error
        raise ValueError("hook changed during migration; the moved file was restored")
    return backup


def _mode(repo: Path) -> str:
    with (repo / "local-gate.toml").open("rb") as stream:
        config = tomllib.load(stream)
    gate = config.get("gate")
    if not isinstance(gate, dict) or not isinstance(gate.get("mode"), str):
        raise ValueError("local-gate.toml does not declare a gate mode")
    return gate["mode"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument(
        "--disable-managed", action="store_true", help="move the recognized hook to a backup"
    )
    args = parser.parse_args()
    try:
        repo = args.repo.resolve(strict=True)
        mode = _mode(repo)
        hook = hook_path(repo)
        found = _read_regular_hook(hook)
        if not args.disable_managed:
            state = (
                "absent"
                if found is None
                else (
                    "known managed"
                    if hashlib.sha256(found[0]).hexdigest() in KNOWN_LEGACY_SHA256
                    else "unrecognized"
                )
            )
            print(f"local-gate mode: {mode}; pre-push: {hook}; hook: {state}")
            return 0
        backup = disable_managed(hook, mode)
        print(f"disabled managed hook; backup: {backup}" if backup else "no pre-push hook exists")
        return 0
    except (OSError, ValueError, TimeoutExpired) as error:
        print(f"pre-push policy: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
