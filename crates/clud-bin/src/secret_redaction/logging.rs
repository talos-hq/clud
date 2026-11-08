//! One-way persistence redaction; never use this on operational input.
use std::sync::OnceLock;

/// Redact credential shapes without a vault, environment access or mutable state.
/// The reversible gateway continues to own execution placeholders (#2211).
pub fn redact_for_log(text: &str) -> String {
    // Structured hook events escape shell quotes and PEM newlines. Work on their
    // string values before serializing, so the same detector covers both forms.
    if let Ok(mut value @ (serde_json::Value::Array(_) | serde_json::Value::Object(_))) =
        serde_json::from_str::<serde_json::Value>(text)
    {
        let original = value.clone();
        redact_json(&mut value);
        if value == original {
            return text.to_owned();
        }
        return value.to_string();
    }
    redact_plain(&redact_embedded_strings(text))
}

fn redact_embedded_strings(text: &str) -> String {
    static QUOTED: OnceLock<regex::Regex> = OnceLock::new();
    let quoted = QUOTED.get_or_init(|| {
        regex::Regex::new(r#""(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'"#)
            .expect("static quoted string pattern")
    });
    quoted
        .replace_all(text, |captures: &regex::Captures<'_>| {
            let original = &captures[0];
            let decoded = if original.starts_with('\'') {
                decode_single_quoted(&original[1..original.len() - 1])
            } else if let Ok(decoded) = serde_json::from_str::<String>(original) {
                decoded
            } else {
                return original.to_owned();
            };
            let redacted = redact_plain(&decoded);
            if redacted == decoded {
                original.to_owned()
            } else {
                serde_json::to_string(&redacted).expect("serialize redacted string")
            }
        })
        .into_owned()
}

fn decode_single_quoted(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => result.push('\n'),
            Some('r') => result.push('\r'),
            Some('t') => result.push('\t'),
            Some(ch @ ('\\' | '\'')) => result.push(ch),
            Some(ch) => {
                result.push('\\');
                result.push(ch);
            }
            None => result.push('\\'),
        }
    }
    result
}

fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = redact_plain(text),
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        serde_json::Value::Object(items) => items.values_mut().for_each(redact_json),
        _ => {}
    }
}

fn contextual_patterns() -> &'static Vec<(&'static str, regex::Regex)> {
    static PATTERNS: OnceLock<Vec<(&str, regex::Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            ("gh-secret", r#"(?s)(?:printf|echo)\s+(?:'[^']*'|"(?:\\.|[^"\\])*"|[^|;\n])*?(?P<secret>'[^']*'|"(?:\\.|[^"\\])*")\s*\|\s*gh\s+secret\s+set\b"#),
            ("password", r#"(?:^|[\s;|&])(?:-p|--(?:password|token|passwd|access-token|auth-token))(?:=|\s+)(?P<secret>'[^']*'|"(?:\\.|[^"\\])*"|[^\s;|&]+)"#),
            ("password", r#"(?:^|[\s;|&])-p(?P<secret>'[^']*'|"(?:\\.|[^"\\])*"|[^\s=;|&-][^\s;|&]*)"#),
            ("authorization", r#"(?i)\bauthorization:[ \t]*(?P<secret>"(?:\\.|[^"\\])*"|'[^']*'|[^\r\n'"\\]+)"#),
            ("apple-password", r#"(?:'(?P<secret>[a-z]{4}(?:-[a-z]{4}){3})'|"(?P<double>[a-z]{4}(?:-[a-z]{4}){3})")"#),
        ]
        .into_iter()
        .map(|(kind, pattern)| (kind, regex::Regex::new(pattern).expect("static log pattern")))
        .collect()
    })
}

fn redact_plain(text: &str) -> String {
    let mut ranges: Vec<_> = super::detect::detect(text, std::iter::empty())
        .into_iter()
        .map(|m| (m.start, m.end, m.kind))
        .collect();
    for (kind, pattern) in contextual_patterns() {
        for captures in pattern.captures_iter(text) {
            let matched = captures
                .name("secret")
                .or_else(|| captures.name("double"))
                .expect("context pattern captures the credential value");
            ranges.push((matched.start(), matched.end(), *kind));
        }
    }
    ranges.sort_by_key(|&(start, end, _)| (start, std::cmp::Reverse(end)));
    let mut merged: Vec<(usize, usize, &str)> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut().filter(|last| range.0 < last.1) {
            last.1 = last.1.max(range.1);
        } else {
            merged.push(range);
        }
    }
    let mut result = String::new();
    let mut cursor = 0;
    for (start, end, kind) in merged {
        result.push_str(&text[cursor..start]);
        result.push_str(&format!("[REDACTED:{kind}]"));
        cursor = end;
    }
    result.push_str(&text[cursor..]);
    result
}

#[cfg(test)]
mod tests {
    use super::redact_for_log;

    #[test]
    fn log_redaction_shapes() {
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"] {
            let fake = format!("{prefix}{}", "FAKE".repeat(22));
            assert_eq!(redact_for_log(&fake), "[REDACTED:github]");
        }
        for (fake, kind) in [
            ("AKIAFAKEFAKEFAKEFAKE", "aws"),
            ("ASIAFAKEFAKEFAKEFAKE", "aws"),
            (
                "-----BEGIN PRIVATE KEY-----\nFAKE\n-----END PRIVATE KEY-----",
                "pem",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nFAKE\n-----END RSA PRIVATE KEY-----",
                "pem",
            ),
            ("eyJhbGciOiJub25lIn0.eyJmYWtlIjp0cnVlfQ.ZmFrZQ", "jwt"),
            ("eyJhbGciOiJub25lIn0.eyJmYWtlIjp0cnVlfQ.", "jwt"),
        ] {
            assert_eq!(redact_for_log(fake), format!("[REDACTED:{kind}]"));
        }
    }

    #[test]
    fn log_redaction_contexts() {
        for command in [
            "login -p 'fake credential'",
            "login --password=fake-value",
            "login -pfake-value",
            "login -p\"fake value\"",
            "login --token \"fake token\"",
            "login --access-token=fake",
            "login --password abcd-efgh-ijkl-mnop",
            "curl -H 'Authorization: Bearer fake-header-value'",
            r#"Authorization: "fake-header-value""#,
            "printf '%s' 'fake-value' | gh secret set FAKE",
            "printf '%s' \"fake-value\" | gh secret set FAKE",
            "echo 'fake-value' | gh secret set FAKE",
        ] {
            let redacted = redact_for_log(command);
            assert!(!redacted.contains("fake"), "{command}: {redacted}");
            assert!(redacted.contains("[REDACTED:"), "{redacted}");
            assert_eq!(redact_for_log(&redacted), redacted);
        }
        assert_eq!(
            redact_for_log("echo 'abcd-efgh-ijkl-mnop'"),
            "echo '[REDACTED:apple-password]'"
        );
        assert_eq!(
            redact_for_log(r#""abcd-efgh-ijkl-mnop""#),
            r#""[REDACTED:apple-password]""#
        );
    }

    #[test]
    fn log_redaction_preserves_ordinary_commands() {
        for command in [
            "ls /tmp/ordinary-path/file.txt",
            "git show 0123456789abcdef0123456789abcdef01234567",
            "echo 01234567-89ab-cdef-0123-456789abcdef",
            "echo abcd-efgh-ijkl-mnop ordinary-hyphenated-words",
            "login --password-file /tmp/password-file",
            "login --password-file=/tmp/password-file",
            "printf '%s' 'ordinary text' | cat",
            "{ \"ordinary\": \"text\" }",
        ] {
            assert_eq!(redact_for_log(command), command);
        }
    }

    #[test]
    fn log_redaction_debug_escaped_commands() {
        for command in [
            r#"printf '%s' "fake-value" | gh secret set FAKE"#,
            r#"echo "abcd-efgh-ijkl-mnop""#,
            r#"login --token "fake-value""#,
        ] {
            let line = format!("tool_name=\"Bash\" command={command:?}");
            let redacted = redact_for_log(&line);
            assert!(!redacted.contains("fake-value"));
            assert!(!redacted.contains("abcd-efgh-ijkl-mnop"));
            assert!(redacted.contains("[REDACTED:"));
        }
    }

    #[test]
    fn log_redaction_structured_event() {
        let input = serde_json::json!({"command": "printf '%s' 'fake-value' | gh secret set FAKE"});
        let redacted = redact_for_log(&input.to_string());
        assert!(!redacted.contains("fake-value"));
        assert!(redacted.contains("[REDACTED:gh-secret]"));
        assert!(serde_json::from_str::<serde_json::Value>(&redacted).is_ok());
    }
}
