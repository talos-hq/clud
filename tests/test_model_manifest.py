"""Model publication checks for the Pages site."""

import json
from datetime import datetime, timezone

import pytest

from models import preserve as installer_preserve
from models import publish, refresh_openrouter
from models.manifest import build_manifest, merge_manifest, should_publish, validate_manifest


def test_latest_stable_sol_and_luna_are_selected() -> None:
    ids = ["gpt-5.6-sol", "gpt-6-sol", "gpt-6-sol-pro", "gpt-6-luna", "gpt-7-sol-preview"]
    document = build_manifest(ids, checked_at="2026-09-26T21:00:00Z")
    assert document["defaults"]["codex"] == {"model": "gpt-6-sol", "effort": "low"}
    assert document["families"] == {"sol": "gpt-6-sol", "luna": "gpt-6-luna"}


def test_missing_family_fails_without_replacing_publication() -> None:
    with pytest.raises(ValueError, match="luna"):
        build_manifest(["gpt-6-sol"], checked_at="2026-09-26T21:00:00Z")


def test_equal_numeric_versions_choose_deterministically() -> None:
    ids = ["gpt-6-sol", "gpt-6.0-sol", "gpt-6-luna"]
    first = build_manifest(ids, checked_at="2026-09-26T21:00:00Z")
    second = build_manifest(reversed(ids), checked_at="2026-09-26T21:00:00Z")
    assert first["families"] == second["families"]


def test_additive_future_fields_are_valid() -> None:
    document = build_manifest(["gpt-7-sol", "gpt-7-luna"], checked_at="2026-09-26T21:00:00Z")
    document["future"] = {"publisher": "example"}
    document["families"]["future"] = "gpt-7-terra"
    document["defaults"]["codex"]["future"] = True
    validate_manifest(document)


def test_recent_manual_publication_skips_nightly_only() -> None:
    now = datetime(2026, 9, 26, 21, tzinfo=timezone.utc)
    previous = {
        "checked_at": "2026-09-26T18:00:00Z",
        "trigger": "manual",
        "families": {"sol": "gpt-6-sol", "luna": "gpt-6-luna"},
    }
    assert not should_publish(previous, now=now, trigger="nightly")
    assert should_publish(previous, now=now, trigger="manual")


def test_public_sources_merge_new_models_and_retain_missing_family(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def catalog(url: str) -> dict:
        if url == publish.OPENROUTER_MODELS:
            return {"data": [{"id": "openai/gpt-7-sol"}]}
        assert url == publish.MODELS_DEV
        return {"openai": {"models": {"gpt-7-sol": {}}}}

    monkeypatch.setattr(publish, "fetch_json", catalog)
    previous = build_manifest(["gpt-6-sol", "gpt-6-luna"], checked_at="2026-09-25T21:00:00Z")
    document = merge_manifest(
        previous, publish.observed_model_ids(), checked_at="2026-09-26T21:00:00Z", trigger="manual"
    )
    assert document["families"] == {
        "sol": "gpt-7-sol", "luna": "gpt-6-luna", "terra": "gpt-6.1-terra"
    }


def test_source_outages_never_remove_published_families(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    previous = build_manifest(["gpt-7-sol", "gpt-7-luna"], checked_at="2026-09-25T21:00:00Z")

    def one_source(url: str) -> dict:
        if url == publish.OPENROUTER_MODELS:
            raise OSError("offline")
        return {"openai": {"models": {"gpt-6-sol": {}, "gpt-6-luna": {}}}}

    monkeypatch.setattr(publish, "fetch_json", one_source)
    merged = merge_manifest(
        previous, publish.observed_model_ids(), checked_at="2026-09-26T21:00:00Z", trigger="manual"
    )
    assert merged["families"] == {**previous["families"], "terra": "gpt-6.1-terra"}

    def both_offline(url: str) -> dict:
        raise OSError("offline")

    monkeypatch.setattr(publish, "fetch_json", both_offline)
    assert publish.observed_model_ids() == set()
    retained = merge_manifest(previous, [], checked_at="2026-09-26T21:00:00Z", trigger="manual")
    assert retained["families"] == {**previous["families"], "terra": "gpt-6.1-terra"}


def test_blocked_chatgpt_model_rolls_back_even_when_catalogs_and_previous_list_it() -> None:
    previous = build_manifest(["gpt-6.1-sol", "gpt-6-luna"], checked_at="2026-09-30T11:13:37Z")
    document = merge_manifest(
        previous,
        ["gpt-6.1-sol", "gpt-6-sol", "gpt-6-luna"],
        checked_at="2026-10-03T21:00:00Z",
        trigger="manual",
        blocked_ids=["gpt-6.1-sol"],
    )
    assert document["families"] == {
        "sol": "gpt-6-sol", "luna": "gpt-6-luna", "terra": "gpt-6.1-terra"
    }
    assert document["defaults"]["codex"]["model"] == "gpt-6-sol"


def test_model_stage_applies_server_policy_to_existing_publication(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    previous = build_manifest(["gpt-6.1-sol", "gpt-6-luna"], checked_at="2026-09-30T11:13:37Z")
    policy = tmp_path / "policy.json"
    policy.write_text(json.dumps({"blocked_chatgpt_backend_models": ["gpt-6.1-sol"]}))
    monkeypatch.setattr(publish, "POLICY_PATH", policy)
    monkeypatch.setattr(publish, "_previous_document", lambda: previous)
    monkeypatch.setattr(publish, "observed_model_ids", lambda: {"gpt-6.1-sol", "gpt-6-luna"})
    monkeypatch.setattr(publish, "fetch", lambda _: b"preserved installer")
    assert publish.stage_site(
        tmp_path, trigger="manual", now=datetime(2026, 10, 3, 21, tzinfo=timezone.utc)
    ) == "publish"
    staged = json.loads((tmp_path / "models" / "manifest.json").read_text())
    assert staged["families"]["sol"] == "gpt-6-sol"


def test_reviewed_baseline_upgrades_legacy_family_during_outage() -> None:
    previous = build_manifest(["gpt-5.6-sol", "gpt-5.6-luna"], checked_at="2026-09-25T21:00:00Z")
    merged = merge_manifest(previous, [], checked_at="2026-09-26T21:00:00Z", trigger="manual")
    assert merged["families"] == {
        "sol": "gpt-6.1-sol", "luna": "gpt-6-luna", "terra": "gpt-6.1-terra"
    }


def test_model_only_stage_preserves_installer_bytes(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    now = datetime(2026, 9, 26, 21, tzinfo=timezone.utc)
    monkeypatch.setattr(publish, "_previous_document", lambda: None)
    monkeypatch.setattr(publish, "observed_model_ids", lambda: set())
    original = {
        f"{publish.PAGES}/{name}": (name + " contents").encode()
        for name in publish.published_paths()
    }
    monkeypatch.setattr(publish, "fetch", lambda url: original[url])
    assert publish.stage_site(tmp_path, trigger="manual", now=now) == "publish"
    for url, body in original.items():
        assert (tmp_path / url.removeprefix(f"{publish.PAGES}/")).read_bytes() == body
    assert (tmp_path / "models" / "manifest.json").exists()


def test_issue_1480_model_stage_preserves_added_installer_asset(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    from installer import site as installer_site

    monkeypatch.setattr(installer_site, "STATIC_ASSETS", {"install/site.css": b"body {}"})
    monkeypatch.setattr(publish, "_previous_document", lambda: None)
    monkeypatch.setattr(publish, "observed_model_ids", lambda: set())
    copied = []

    def fake_fetch(url: str) -> bytes:
        copied.append(url)
        return b"body {}" if url.endswith("site.css") else b"existing"

    monkeypatch.setattr(publish, "fetch", fake_fetch)
    now = datetime(2026, 9, 26, 21, tzinfo=timezone.utc)
    assert publish.stage_site(tmp_path, trigger="manual", now=now) == "publish"
    assert (tmp_path / "install" / "site.css").read_bytes() == b"body {}"
    assert f"{publish.PAGES}/install/site.css" in copied


def test_issue_1480_installer_stage_preserves_published_model_document(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    document = build_manifest(["gpt-6-sol", "gpt-6-luna"], checked_at="2026-09-26T21:00:00Z")
    original = json.dumps(document, indent=2).encode()
    monkeypatch.setattr(installer_preserve, "fetch", lambda _: original)
    assert installer_preserve.preserve(tmp_path)
    assert (tmp_path / "models" / "manifest.json").read_bytes() == original


def test_recent_manual_and_unchanged_do_not_stage(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    now = datetime(2026, 9, 26, 21, tzinfo=timezone.utc)
    previous = build_manifest(
        ["gpt-6.1-sol", "gpt-6-luna", "gpt-6.1-terra"],
        checked_at="2026-09-26T20:00:00Z",
    )
    monkeypatch.setattr(publish, "_previous_document", lambda: previous)
    monkeypatch.setattr(publish, "observed_model_ids", lambda: set())
    assert publish.stage_site(tmp_path, trigger="nightly", now=now) == "recent-manual"
    assert publish.stage_site(tmp_path, trigger="manual", now=now) == "unchanged"
    assert not list(tmp_path.iterdir())


def test_openrouter_refresh_selects_latest_stable_sol_terra_luna() -> None:
    ids = {
        "gpt-6-sol", "gpt-6.1-sol", "gpt-6.1-sol-pro",
        "gpt-5.6-terra", "gpt-6.1-terra", "gpt-6-luna",
    }
    assert refresh_openrouter.latest_models(ids) == {
        "sol": "gpt-6.1-sol", "terra": "gpt-6.1-terra", "luna": "gpt-6-luna",
    }


def test_bundled_openrouter_models_are_valid() -> None:
    from models.manifest import snapshot_families

    assert snapshot_families() == {
        "sol": "gpt-6.1-sol", "terra": "gpt-6.1-terra", "luna": "gpt-6-luna",
    }
