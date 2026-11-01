"""Managed pre-push migration preserves custom hooks and opt-ins (#2265)."""

from __future__ import annotations

import base64
import os
from pathlib import Path

import pytest

from ci import pre_push_policy
from ci.process import run

LEGACY = base64.b64decode(
    (Path(__file__).parent / "fixtures/ci_lint_pre_push_legacy.b64").read_bytes()
)


def test_explicit_migration_preserves_the_exact_hook_in_a_backup(tmp_path):
    hook = tmp_path / "pre-push"
    hook.write_bytes(LEGACY)
    hook.chmod(0o755)
    backup = pre_push_policy.disable_managed(hook, "shadow")
    assert backup is not None
    assert backup.parent == hook.parent
    assert backup.read_bytes() == LEGACY
    if os.name != "nt":
        assert backup.stat().st_mode & 0o111
    assert not hook.exists()
    assert pre_push_policy.disable_managed(hook, "shadow") is None
    assert backup.read_bytes() == LEGACY


def test_windows_line_endings_are_recognized_without_normalizing_backup(tmp_path):
    hook = tmp_path / "pre-push"
    original = LEGACY.replace(b"\n", b"\r\n")
    hook.write_bytes(original)
    backup = pre_push_policy.disable_managed(hook, "shadow")
    assert backup is not None
    assert backup.read_bytes() == original


def test_custom_hook_and_explicit_enforcement_are_never_replaced(tmp_path):
    hook = tmp_path / "pre-push"
    custom = LEGACY + b"# user addition\n"
    hook.write_bytes(custom)
    with pytest.raises(ValueError, match="unrecognized"):
        pre_push_policy.disable_managed(hook, "shadow")
    assert hook.read_bytes() == custom
    hook.write_bytes(LEGACY)
    with pytest.raises(ValueError, match="shadow"):
        pre_push_policy.disable_managed(hook, "enforce")
    assert hook.read_bytes() == LEGACY


def test_symlink_hook_is_not_followed(tmp_path):
    target = tmp_path / "target"
    target.write_bytes(LEGACY)
    hook = tmp_path / "pre-push"
    try:
        hook.symlink_to(target)
    except OSError:
        pytest.skip("symlinks are unavailable")
    with pytest.raises(ValueError, match="regular"):
        pre_push_policy.disable_managed(hook, "shadow")
    assert hook.is_symlink()
    assert target.read_bytes() == LEGACY


def test_shared_worktree_and_configured_hook_paths_are_resolved(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    env = os.environ.copy()
    env.update(
        {
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Fixture",
            "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
            "GIT_COMMITTER_NAME": "Fixture",
            "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
        }
    )
    run(["git", "init", "-q"], cwd=repo, env=env, check=True, timeout=30)
    run(
        ["git", "commit", "--allow-empty", "-qm", "fixture"],
        cwd=repo,
        env=env,
        check=True,
        timeout=30,
    )
    worktree = tmp_path / "worktree"
    run(
        ["git", "worktree", "add", "-q", "-b", "fixture", str(worktree)],
        cwd=repo,
        env=env,
        check=True,
        timeout=30,
    )
    assert pre_push_policy.hook_path(repo, env=env) == pre_push_policy.hook_path(worktree, env=env)
    hooks = tmp_path / "custom-hooks"
    hooks.mkdir()
    run(["git", "config", "core.hooksPath", str(hooks)], cwd=repo, env=env, check=True, timeout=30)
    assert pre_push_policy.hook_path(worktree, env=env) == hooks / "pre-push"


def test_migration_refuses_a_hook_edited_after_inspection(tmp_path, monkeypatch):
    hook = tmp_path / "pre-push"
    original = LEGACY
    hook.write_bytes(original)
    real_read = pre_push_policy._read_regular_hook

    def read_then_edit(path):
        found = real_read(path)
        path.write_bytes(original + b"# user edit\n")
        return found

    monkeypatch.setattr(pre_push_policy, "_read_regular_hook", read_then_edit)
    with pytest.raises(ValueError, match="changed"):
        pre_push_policy.disable_managed(hook, "shadow")
    assert hook.read_bytes() == original + b"# user edit\n"


def test_backup_collision_preserves_existing_file(tmp_path, monkeypatch):
    hook = tmp_path / "pre-push"
    hook.write_bytes(LEGACY)
    backup = tmp_path / "pre-push.clud-disabled-collision"
    backup.write_bytes(b"existing user data")
    monkeypatch.setattr(pre_push_policy.secrets, "token_hex", lambda _size: "collision")
    with pytest.raises(ValueError, match="backup"):
        pre_push_policy.disable_managed(hook, "shadow")
    assert hook.read_bytes() == LEGACY
    assert backup.read_bytes() == b"existing user data"


def test_hook_replaced_at_rename_is_restored(tmp_path, monkeypatch):
    hook = tmp_path / "pre-push"
    hook.write_bytes(LEGACY)
    real_rename = pre_push_policy._rename_noreplace
    calls = 0

    def replace_before_rename(source, destination):
        nonlocal calls
        calls += 1
        if calls == 1:
            source.rename(tmp_path / "original")
            source.write_bytes(b"custom replacement")
        real_rename(source, destination)

    monkeypatch.setattr(pre_push_policy, "_rename_noreplace", replace_before_rename)
    with pytest.raises(ValueError, match="changed"):
        pre_push_policy.disable_managed(hook, "shadow")
    assert hook.read_bytes() == b"custom replacement"
    assert (tmp_path / "original").read_bytes() == LEGACY


def test_backup_created_at_rename_is_never_overwritten(tmp_path, monkeypatch):
    hook = tmp_path / "pre-push"
    hook.write_bytes(LEGACY)
    real_rename = pre_push_policy._rename_noreplace
    monkeypatch.setattr(pre_push_policy.secrets, "token_hex", lambda _size: "collision")
    backup = tmp_path / "pre-push.clud-disabled-collision"

    def create_before_rename(source, destination):
        destination.write_bytes(b"concurrent data")
        real_rename(source, destination)

    monkeypatch.setattr(pre_push_policy, "_rename_noreplace", create_before_rename)
    with pytest.raises(FileExistsError):
        pre_push_policy.disable_managed(hook, "shadow")
    assert hook.read_bytes() == LEGACY
    assert backup.read_bytes() == b"concurrent data"


def test_failed_restore_preserves_both_hook_and_moved_file(tmp_path, monkeypatch):
    hook = tmp_path / "pre-push"
    hook.write_bytes(LEGACY)
    real_rename = pre_push_policy._rename_noreplace
    calls = 0

    def replace_and_reoccupy(source, destination):
        nonlocal calls
        calls += 1
        if calls == 1:
            source.rename(tmp_path / "original")
            source.write_bytes(b"custom replacement")
            real_rename(source, destination)
            hook.write_bytes(b"new active hook")
        else:
            real_rename(source, destination)

    monkeypatch.setattr(pre_push_policy, "_rename_noreplace", replace_and_reoccupy)
    with pytest.raises(ValueError, match="preserved at"):
        pre_push_policy.disable_managed(hook, "shadow")
    assert hook.read_bytes() == b"new active hook"
    backups = list(tmp_path.glob("pre-push.clud-disabled-*"))
    assert len(backups) == 1
    assert backups[0].read_bytes() == b"custom replacement"
    assert (tmp_path / "original").read_bytes() == LEGACY


def test_git_timeout_is_reported_without_traceback(tmp_path, monkeypatch, capsys):
    (tmp_path / "local-gate.toml").write_text('[gate]\nmode = "shadow"\n')

    def timed_out(*_args, **_kwargs):
        raise pre_push_policy.TimeoutExpired("git", 10)

    monkeypatch.setattr(pre_push_policy, "run", timed_out)
    monkeypatch.setattr(pre_push_policy.sys, "argv", ["pre_push_policy", "--repo", str(tmp_path)])
    assert pre_push_policy.main() == 1
    captured = capsys.readouterr()
    assert "pre-push policy:" in captured.err
    assert "Traceback" not in captured.err
