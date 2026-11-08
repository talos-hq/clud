//! Presentation masking and session-owned reversible gateway redaction.
//! Display helpers never mutate operational argv or vault values.

use std::sync::OnceLock;

mod detect;
mod logging;
pub use logging::redact_for_log;
pub mod policy;
pub mod reversible;
mod session;
pub mod status;
pub mod stream;

pub fn mask_key(key: &str) -> String {
    let suffix: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("****{suffix}")
}

pub fn redact_text(value: &str) -> String {
    static KEY_PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = KEY_PATTERN.get_or_init(|| {
        regex::Regex::new(r"sk-[A-Za-z0-9_-]{17,}").expect("static API-key pattern")
    });
    pattern
        .replace_all(value, |matched: &regex::Captures<'_>| mask_key(&matched[0]))
        .into_owned()
}

pub fn redact_args(args: &[String]) -> Vec<String> {
    args.iter().map(|arg| redact_text(arg)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_fixture_key_without_changing_non_secrets() {
        let key = "sk-0123456789abcdef0123456789abcdef";
        assert_eq!(redact_text(key), "****cdef");
        assert_eq!(
            redact_text(&format!("--deepseek={key}")),
            "--deepseek=****cdef"
        );
        assert_eq!(redact_text("--add-dir"), "--add-dir");
    }
}
