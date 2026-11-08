//! Prefix-anchored credential patterns. No entropy or personal-data heuristics.

use std::sync::OnceLock;

pub struct Match {
    pub start: usize,
    pub end: usize,
    pub kind: &'static str,
    rank: usize,
}

const PATTERNS: &[(&str, &str)] = &[
    ("openrouter", r"sk-or-v1-[0-9a-f]{64}"),
    ("anthropic", r"sk-ant-[A-Za-z0-9_-]{20,}"),
    ("openai", r"sk-(?:proj-)?[A-Za-z0-9_-]{20,}"),
    (
        "github",
        r"(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})",
    ),
    ("aws", r"(?:AKIA|ASIA)[0-9A-Z]{16}"),
    ("slack", r"xox[abpors]-[A-Za-z0-9-]{10,}"),
    ("google", r"AIza[0-9A-Za-z_-]{35}"),
    ("stripe", r"(?:sk|rk)_(?:live|test)_[0-9a-zA-Z]{24,}"),
    ("jwt", r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*"),
];

fn patterns() -> &'static Vec<(&'static str, regex::Regex)> {
    static COMPILED: OnceLock<Vec<(&str, regex::Regex)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        PATTERNS
            .iter()
            .map(|(kind, pattern)| {
                (
                    *kind,
                    regex::Regex::new(pattern).expect("static secret pattern"),
                )
            })
            .collect()
    })
}

fn identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn boundary(text: &str, start: usize, end: usize) -> bool {
    let bytes = text.as_bytes();
    (start == 0 || !identifier(bytes[start - 1])) && (end == bytes.len() || !identifier(bytes[end]))
}

fn prefixed(text: &str, found: &mut Vec<Match>) {
    for (rank, (kind, pattern)) in patterns().iter().enumerate() {
        for candidate in pattern.find_iter(text) {
            let generic_reserved = *kind == "openai"
                && (candidate.as_str().starts_with("sk-or-")
                    || candidate.as_str().starts_with("sk-ant-"));
            if !generic_reserved && boundary(text, candidate.start(), candidate.end()) {
                found.push(Match {
                    start: candidate.start(),
                    end: candidate.end(),
                    kind,
                    rank,
                });
            }
        }
    }
}

fn private_keys(text: &str, found: &mut Vec<Match>) {
    static BEGIN: OnceLock<regex::Regex> = OnceLock::new();
    let begin = BEGIN.get_or_init(|| {
        regex::Regex::new(r"-----BEGIN ([A-Z ]*PRIVATE KEY)-----").expect("static PEM pattern")
    });
    for capture in begin.captures_iter(text) {
        let header = capture.get(0).expect("whole PEM header");
        let end = format!("-----END {}-----", &capture[1]);
        if let Some(offset) = text[header.end()..].find(&end) {
            found.push(Match {
                start: header.start(),
                end: header.end() + offset + end.len(),
                kind: "pem",
                rank: PATTERNS.len(),
            });
        }
    }
}

/// Overlapping detections form one interval so no portion of a vault key can
/// escape because another pattern began earlier inside the same interval.
pub fn detect<'a>(text: &str, known: impl Iterator<Item = &'a str>) -> Vec<Match> {
    let mut found = Vec::new();
    prefixed(text, &mut found);
    private_keys(text, &mut found);
    for secret in known.filter(|secret| !secret.is_empty()) {
        for (start, _) in text.match_indices(secret) {
            found.push(Match {
                start,
                end: start + secret.len(),
                kind: "vault",
                rank: PATTERNS.len() + 1,
            });
        }
    }
    found.sort_by_key(|matched| (matched.start, matched.rank));
    let mut merged: Vec<Match> = Vec::new();
    for matched in found {
        if let Some(previous) = merged
            .last_mut()
            .filter(|previous| matched.start < previous.end)
        {
            previous.end = previous.end.max(matched.end);
            if matched.rank < previous.rank {
                previous.kind = matched.kind;
                previous.rank = matched.rank;
            }
        } else {
            merged.push(matched);
        }
    }
    merged
}
