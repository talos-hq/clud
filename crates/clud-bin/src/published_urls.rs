//! Where clud fetches its published metadata from, in order (#2449).
//!
//! Every list starts at GitHub Pages and ends with a GitHub-owned URL, so a
//! Pages problem degrades to the release fallback instead of breaking every
//! installed clud. `ci/fetch_fallbacks.py` fails `bash lint` when a fetch site
//! bypasses these lists or a list loses its GitHub fallback.

/// Releases of this repository; `latest/download/<asset>` 302s to the newest
/// published release's asset.
pub const GITHUB_RELEASES: &str = "https://github.com/talos-hq/clud/releases";

/// The stable install catalog. The GitHub fallback is the newest release's
/// candidate catalog, which lists every release and names that release as its
/// `candidate` channel.
pub const INSTALL_CATALOG: &[&str] = &[
    "https://talos-hq.github.io/clud/install/manifest.json",
    "https://github.com/talos-hq/clud/releases/latest/download/installer-candidate-manifest.json",
];

/// The Codex model manifest. The GitHub fallback is published as a release
/// asset by `model-pages.yml`; the built-in table stays the last resort.
pub const MODEL_MANIFEST: &[&str] = &[
    "https://talos-hq.github.io/clud/models/manifest.json",
    "https://github.com/talos-hq/clud/releases/latest/download/models-manifest.json",
];

/// True for a URL served by GitHub itself rather than GitHub Pages.
pub fn is_github_owned(url: &str) -> bool {
    url.starts_with("https://github.com/") || url.starts_with("https://raw.githubusercontent.com/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_published_list_ends_with_a_github_fallback() {
        for list in [INSTALL_CATALOG, MODEL_MANIFEST] {
            assert!(list.len() >= 2, "{list:?}: primary plus a fallback");
            assert!(!is_github_owned(list[0]), "{list:?}: primary is Pages");
            assert!(is_github_owned(list[list.len() - 1]), "{list:?}");
            assert!(list.iter().all(|url| url.starts_with("https://")));
        }
    }
}
