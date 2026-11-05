//! The product registry (#2609): identity only, no logic.
//!
//! Adding a product is a row here plus whatever the configure step needs to
//! write that product's config. The runner in [`super::command`] never names a
//! product.

/// One installable agent product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Product {
    /// The word the user types after `clud install`.
    pub target: &'static str,
    /// Human name used in prompts and reports.
    pub display: &'static str,
    /// `owner/repo` the bootstrap is fetched from.
    pub repo: &'static str,
    /// Path of the bootstrap script inside that repository.
    pub bootstrap: &'static str,
    /// Branch the bootstrap is fetched from by default.
    pub branch: &'static str,
    /// Environment variable the bootstrap reads its install directory from.
    pub dir_env: &'static str,
    /// Default install directory, as a path under the user's home.
    pub default_dir: &'static str,
    /// Environment variable that relocates the product's config home.
    pub home_env: &'static str,
    /// Default config home, as a path under the user's home.
    pub default_home: &'static str,
    /// Config file name inside that home.
    pub config_file: &'static str,
    /// The executable the product installs, for the closing hint.
    pub binary: &'static str,
}

impl Product {
    /// `https://raw.githubusercontent.com/<repo>/<branch>/<bootstrap>`.
    pub fn bootstrap_url(&self, branch: &str) -> String {
        format!(
            "https://raw.githubusercontent.com/{}/{}/{}",
            self.repo, branch, self.bootstrap
        )
    }

    /// `owner/repo@branch/path`, for a report line that fits one terminal row.
    pub fn bootstrap_label(&self, branch: &str) -> String {
        format!("{}@{branch}/{}", self.repo, self.bootstrap)
    }

    /// The human command that reconfigures this product later.
    pub fn setup_hint(&self) -> String {
        format!("{} setup clud", self.binary)
    }
}

/// Every product `clud install` accepts. Order is the order the unknown-target
/// error lists them in.
pub const PRODUCTS: &[Product] = &[
    Product {
        target: "hermes",
        display: "hermes-slim",
        repo: "talos-hq/hermes-slim",
        bootstrap: "slim/install.sh",
        branch: "main",
        dir_env: "HERMES_SLIM_DIR",
        default_dir: ".hermes-slim",
        home_env: "HERMES_HOME",
        default_home: ".hermes",
        config_file: "config.yaml",
        binary: "hermes",
    },
    Product {
        target: "talos",
        display: "talos",
        repo: "talos-hq/talos",
        bootstrap: "scripts/install.sh",
        branch: "main",
        dir_env: "HERMES_INSTALL_DIR",
        default_dir: ".hermes",
        home_env: "HERMES_HOME",
        default_home: ".hermes",
        config_file: "config.yaml",
        binary: "talos",
    },
];

/// Resolve a target word case-insensitively.
pub fn find(target: &str) -> Option<&'static Product> {
    PRODUCTS
        .iter()
        .find(|product| product.target.eq_ignore_ascii_case(target))
}

/// The accepted targets, for an error message.
pub fn names() -> String {
    PRODUCTS
        .iter()
        .map(|product| product.target)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_product_is_findable_by_its_own_target() {
        for product in PRODUCTS {
            assert_eq!(find(product.target).map(|p| p.target), Some(product.target));
        }
    }

    #[test]
    fn lookup_ignores_case_and_never_matches_a_stranger() {
        assert_eq!(find("HERMES").map(|p| p.target), Some("hermes"));
        assert!(find("codex").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn targets_are_unique_and_lowercase() {
        let mut seen = Vec::new();
        for product in PRODUCTS {
            assert!(
                product.target.chars().all(|c| c.is_ascii_lowercase()),
                "{} must be lowercase",
                product.target
            );
            assert!(
                !seen.contains(&product.target),
                "duplicate {}",
                product.target
            );
            seen.push(product.target);
        }
    }

    #[test]
    fn bootstrap_url_joins_repo_branch_and_path() {
        let hermes = find("hermes").expect("hermes");
        assert_eq!(
            hermes.bootstrap_url("main"),
            "https://raw.githubusercontent.com/talos-hq/hermes-slim/main/slim/install.sh"
        );
        assert!(hermes.bootstrap_url("v1").contains("/v1/"));
    }

    #[test]
    fn the_reconfigure_hint_names_the_product_binary() {
        assert_eq!(
            find("hermes").expect("hermes").setup_hint(),
            "hermes setup clud"
        );
        assert_eq!(
            find("talos").expect("talos").setup_hint(),
            "talos setup clud"
        );
    }

    #[test]
    fn names_lists_every_target_in_registry_order() {
        assert_eq!(names(), "hermes, talos");
    }
}
