"""Refresh the model document without rebuilding the release-history catalog."""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from installer.site import published_paths
from models.manifest import merge_manifest, should_publish, validate_manifest

# The canonical custom domain; github.io 301-redirects to it (#2449).
PAGES = "https://talosbot.ai/clud"
# GitHub-owned fallback for the model document (published by model-pages.yml).
MODEL_MANIFEST_URLS = (
    f"{PAGES}/models/manifest.json",
    "https://github.com/talos-hq/clud/releases/latest/download/models-manifest.json",
)
OPENROUTER_MODELS = "https://openrouter.ai/api/v1/models"
MODELS_DEV = "https://models.dev/api.json"
MAX_RESPONSE = 8_000_000
POLICY_PATH = Path(__file__).with_name("selection-policy.json")


def fetch(url: str) -> bytes:
    headers = {"User-Agent": "clud-model-publisher/1"}
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=15) as response:
        body = response.read(MAX_RESPONSE + 1)
    if len(body) > MAX_RESPONSE:
        raise ValueError(f"response too large: {url}")
    return body


def fetch_json(url: str) -> dict[str, Any]:
    result = json.loads(fetch(url))
    if not isinstance(result, dict):
        raise ValueError(f"expected JSON object: {url}")
    return result


def _model_ids(payload: dict[str, Any], *, openrouter: bool) -> set[str]:
    entries = payload.get("data")
    if not isinstance(entries, list):
        raise ValueError("model catalog has no data array")
    ids = set()
    for entry in entries:
        if not isinstance(entry, dict) or not isinstance(entry.get("id"), str):
            continue
        model_id = entry["id"]
        if openrouter:
            if not model_id.startswith("openai/"):
                continue
            model_id = model_id.removeprefix("openai/")
        ids.add(model_id)
    return ids


def _models_dev_ids(payload: dict[str, Any]) -> set[str]:
    openai = payload.get("openai")
    if not isinstance(openai, dict) or not isinstance(openai.get("models"), dict):
        raise ValueError("models.dev has no OpenAI model catalog")
    return set(openai["models"])


def observed_model_ids() -> set[str]:
    """Union surviving public catalogs; failures cannot remove previous IDs."""
    observed: set[str] = set()
    for name, url, extract in (
        ("OpenRouter", OPENROUTER_MODELS, lambda data: _model_ids(data, openrouter=True)),
        ("models.dev", MODELS_DEV, _models_dev_ids),
    ):
        try:
            observed.update(extract(fetch_json(url)))
        except (OSError, ValueError, TypeError, KeyError) as exc:
            print(f"warning: {name} catalog unavailable: {exc}", file=sys.stderr)
    return observed


def _previous_document() -> dict[str, Any] | None:
    error: Exception | None = None
    for url in MODEL_MANIFEST_URLS:
        try:
            document = fetch_json(url)
        except urllib.error.HTTPError as exc:
            if exc.code == 404 and url == MODEL_MANIFEST_URLS[0]:
                return None
            error = exc
            continue
        except OSError as exc:
            error = exc
            continue
        validate_manifest(document)
        return document
    raise OSError("no published model manifest is reachable") from error


def stage_site(destination: Path, *, trigger: str, now: datetime) -> str:
    """Return 'publish', 'unchanged', or 'recent-manual'. Errors leave Pages untouched."""
    previous = _previous_document()
    if not should_publish(previous, now=now, trigger=trigger):
        return "recent-manual"
    checked_at = now.astimezone(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
    policy = json.loads(POLICY_PATH.read_text(encoding="utf-8"))
    blocked_ids = policy["blocked_chatgpt_backend_models"]
    if not isinstance(blocked_ids, list) or not all(isinstance(value, str) for value in blocked_ids):
        raise ValueError("invalid blocked ChatGPT backend model policy")
    document = merge_manifest(
        previous, observed_model_ids(), checked_at=checked_at, trigger=trigger,
        blocked_ids=blocked_ids,
    )
    validate_manifest(document)
    if previous and previous.get("families") == document["families"]:
        return "unchanged"
    # Pages deployment replaces the entire site. Preserve the already-verified
    # installer subtree byte for byte, and only then add the new model document.
    for relative in published_paths():
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(fetch(f"{PAGES}/{relative}"))
    (destination / ".nojekyll").write_text("", encoding="utf-8")
    model_path = destination / "models" / "manifest.json"
    model_path.parent.mkdir(parents=True, exist_ok=True)
    model_path.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return "publish"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("destination", type=Path)
    parser.add_argument("--trigger", choices=("manual", "nightly"), required=True)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    result = stage_site(
        args.destination,
        trigger=args.trigger,
        now=datetime.now(timezone.utc),
    )
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as output:
            output.write(f"result={result}\n")
    print(result, file=sys.stderr)


if __name__ == "__main__":
    main()
