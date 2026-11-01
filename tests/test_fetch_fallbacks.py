"""ci/fetch_fallbacks.py (#2449): published fetches keep a GitHub fallback."""

from __future__ import annotations

from ci import fetch_fallbacks as guard

RUST = "crates/clud-bin/src/published_urls.rs"


def test_the_tree_passes() -> None:
    assert guard.main() == 0


def test_a_stray_published_url_is_flagged_outside_registries_and_tests() -> None:
    line = 'const URL: &str = "https://talosbot.ai/clud/models/manifest.json";\n'
    assert guard.stray_urls("crates/clud-bin/src/foo.rs", line)
    assert not guard.stray_urls(RUST, line)
    assert not guard.stray_urls("crates/clud-bin/src/self_install/catalog.rs", line)
    in_tests = "fn f() {}\n#[cfg(test)]\nmod tests {\n" + line + "}\n"
    assert not guard.stray_urls("crates/clud-bin/src/foo.rs", in_tests)
    assert not guard.is_scanned("crates/clud-bin/src/foo_tests.rs")
    assert not guard.is_scanned("tests/test_x.py")


def test_a_registry_must_end_with_a_github_owned_url() -> None:
    good = (
        'pub const INSTALL_CATALOG: &[&str] = &["https://talosbot.ai/clud/install/manifest.json",'
        ' "https://github.com/o/r/releases/latest/download/c.json"];\n'
        'pub const MODEL_MANIFEST: &[&str] = &["https://talosbot.ai/clud/models/manifest.json",'
        ' "https://raw.githubusercontent.com/o/r/main/m.json"];\n'
    )
    assert guard.registry_problems(RUST, good) == []
    no_fallback = good.replace("https://github.com/o/r/releases/latest/download/c.json",
                               "https://mirror.example/c.json")
    assert any("GitHub-owned" in p for p in guard.registry_problems(RUST, no_fallback))
    redirecting = good.replace("https://talosbot.ai/clud/install", "https://o.github.io/clud/install")
    assert any("github.io" in p for p in guard.registry_problems(RUST, redirecting))
    missing = good.split("\n", 1)[1]
    assert any("INSTALL_CATALOG" in p for p in guard.registry_problems(RUST, missing))


def test_python_f_string_entries_count() -> None:
    text = (
        'MODEL_MANIFEST_URLS = (\n    f"{PAGES}/models/manifest.json",\n'
        '    "https://github.com/o/r/releases/latest/download/models-manifest.json",\n)\n'
    )
    assert guard.registry_problems("models/publish.py", text) == []
