"""Explicit metered-provider consent for tests that launch a metered route.

clud is subscription-first: a metered provider (OpenRouter, DeepSeek, Kimi)
is reached only with ``CLUD_ALLOW_METERED=1`` and a matching
``CLUD_METERED_PROVIDER``. Tests that deliberately exercise such a route call
these helpers, so the consent is visible at the call site.
"""

from __future__ import annotations

from collections.abc import Sequence

METERED = ("openrouter", "deepseek", "kimi")
_FLAGS = {"--openrouter": "openrouter", "--deepseek": "deepseek", "--kimi": "kimi"}


def consent(env: dict[str, str], provider: str) -> dict[str, str]:
    """Return a copy of ``env`` consenting to ``provider``'s metered billing."""
    out = dict(env)
    out["CLUD_ALLOW_METERED"] = "1"
    out["CLUD_METERED_PROVIDER"] = provider
    out.pop("CLUD_PROVIDER", None)
    return out


def _model_provider(model: str) -> str | None:
    lowered = model.lower()
    for provider in METERED:
        if lowered.startswith(provider):
            return provider
    if lowered.startswith("~") or "/" in lowered:
        return "openrouter"
    return None


def provider_in(argv: Sequence[str]) -> str | None:
    """The metered provider an argv selects, if any (before any ``--``)."""
    args = list(argv)
    for index, raw in enumerate(args):
        if raw == "--":
            break
        # clud corrects `-deepseek` and Unicode-dash spellings before parsing.
        name, _, inline = raw.partition("=")
        stripped = name.lstrip("-\u2010\u2011\u2012\u2013\u2014\u2212")
        arg = f"--{stripped}" if stripped != name else name
        if arg in _FLAGS:
            return _FLAGS[arg]
        following = inline or (args[index + 1] if index + 1 < len(args) else "")
        if arg == "--provider" and following in METERED:
            return following
        if arg == "--model" and _model_provider(following):
            return _model_provider(following)
    return None


def consent_for(env: dict[str, str], argv: Sequence[str]) -> dict[str, str]:
    """Consent to the metered provider ``argv`` selects; else return ``env``."""
    provider = provider_in(argv)
    if provider is None:
        return env
    listed = [name.strip() for name in env.get("CLUD_METERED_PROVIDER", "").split(",")]
    if env.get("CLUD_ALLOW_METERED") == "1" and provider in listed and "CLUD_PROVIDER" not in env:
        return env
    return consent(env, provider)


UNIFIED = "codex,openrouter,deepseek,kimi"


def consent_unified(env: dict[str, str]) -> dict[str, str]:
    """Consent to every metered route a unified gateway session may take."""
    return consent(env, UNIFIED)
