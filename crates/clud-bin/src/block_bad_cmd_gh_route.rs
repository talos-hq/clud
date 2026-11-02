//! #2460: with `gh.backend = "ghd"`, every gh call goes through the session's
//! `gh` shim, which runs `ghd gh ARGS`. A call to the real binary by path
//! (`/usr/bin/gh`, `$CLUD_GH_SHIM_TARGET`, `$GHD_REAL_GH`) would skip ghd's
//! cache and budget, so the hook rewrites that program word to `gh` and says
//! so in one line. Arguments are untouched.

use super::block_bad_cmd_shell::{parse_command_with_home, program_name};
use super::ShellDialect;

/// Variables that name the real gh inside a clud session.
const REAL_GH_VARS: &[&str] = &["CLUD_GH_SHIM_TARGET", "GHD_REAL_GH"];

/// Is `raw` (a program word as typed) the real gh rather than the shim?
fn names_real_gh(raw: &str) -> bool {
    let word = raw.trim_matches(&['\'', '"'][..]);
    if let Some(var) = word.strip_prefix('$') {
        let var = var.trim_start_matches('{').trim_end_matches('}');
        return REAL_GH_VARS.contains(&var);
    }
    word.contains(['/', '\\']) && program_name(word) == "gh"
}

fn is_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|b| b.is_ascii_whitespace() || b"(;&|".contains(&b))
}

/// Byte offsets of `token` in `text` where it stands as a whole word.
fn token_offsets(text: &str, token: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    text.match_indices(token)
        .map(|(at, _)| at)
        .filter(|&at| {
            let before = at.checked_sub(1).map(|i| bytes[i]);
            is_boundary(before) && is_boundary(bytes.get(at + token.len()).copied())
        })
        .collect()
}

/// `command` with each real-gh program word replaced by `gh`, or `None` when
/// there is none or the rewrite is not provably faithful: a non-POSIX dialect,
/// a parse error, or a real-gh path that also appears as an argument.
pub(super) fn ghd_substitute(command: &str, dialect: ShellDialect) -> Option<String> {
    if dialect != ShellDialect::Posix {
        return None;
    }
    let script = parse_command_with_home(command, dialect, None, None);
    if script.has_error || script.depth_capped {
        return None;
    }
    let mut programs: Vec<&str> = script
        .invocations
        .iter()
        .filter_map(|inv| inv.raw.first().map(String::as_str))
        .filter(|raw| names_real_gh(raw))
        .collect();
    if programs.is_empty() {
        return None;
    }
    programs.sort_unstable();
    programs.dedup();
    let mut rewritten = command.to_string();
    for token in programs {
        let uses = script
            .invocations
            .iter()
            .filter(|inv| inv.raw.first().map(String::as_str) == Some(token))
            .count();
        let offsets = token_offsets(&rewritten, token);
        if offsets.len() != uses {
            return None;
        }
        for at in offsets.into_iter().rev() {
            rewritten.replace_range(at..at + token.len(), "gh");
        }
    }
    Some(rewritten)
}

/// The one line the agent sees after [`ghd_substitute`] applied.
pub(super) fn ghd_substitution_notice() -> &'static str {
    "[clud] ran `gh` (routed through ghd, gh.backend) instead of the real gh binary"
}

#[cfg(test)]
#[path = "block_bad_cmd_gh_route_tests.rs"]
mod tests;
