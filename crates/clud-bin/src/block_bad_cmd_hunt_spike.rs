//! Throwaway literal-grammar experiment, not a general shell parser.
use super::{plausible_path, Range};
use std::path::Path;

fn words(command: &str) -> Option<Vec<Vec<String>>> {
    // Do not interpret pipelines, expansions, redirections or nested shell syntax.
    // `>` inside a quoted awk program is handled after tokenization.
    if command.contains(['|', '$', '`', '\n']) {
        return None;
    }
    command
        .split(';')
        .map(|part| shell_words::split(part).ok())
        .collect()
}

pub(super) fn has_extra_idiom(command: &str) -> bool {
    words(command).is_some_and(|parts| {
        parts.iter().any(|p| {
            p.first()
                .is_some_and(|s| matches!(s.as_str(), "awk" | "cat" | "grep" | "head" | "tail"))
        })
    })
}

fn line_count(cwd: &Path, path: &str) -> Option<u32> {
    u32::try_from(
        std::fs::read_to_string(cwd.join(path))
            .ok()?
            .lines()
            .count(),
    )
    .ok()
}

fn awk_range(program: &str) -> Option<(u32, u32)> {
    let compact: String = program.chars().filter(|c| !c.is_whitespace()).collect();
    let (start, end) = compact.strip_prefix("NR>=")?.split_once("&&NR<=")?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn extra_range(parts: &[String], cwd: &Path) -> Option<Range> {
    let path = parts.last()?;
    if !plausible_path(path) {
        return None;
    }
    let (start, end) = match parts
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["awk", program, _] => awk_range(program)?,
        ["cat", "-n", _] => (1, line_count(cwd, path)?),
        ["head", "-n", n, _] => (1, n.parse().ok()?),
        ["tail", "-n", n, _] => {
            let end = line_count(cwd, path)?;
            (
                end.saturating_sub(n.parse::<u32>().ok()?).saturating_add(1),
                end,
            )
        }
        _ => return None,
    };
    (start > 0 && end >= start).then(|| Range {
        path: path.clone(),
        start,
        end,
    })
}

pub(super) fn ranges(command: &str, cwd: &Path) -> Option<Vec<Range>> {
    if !has_extra_idiom(command) {
        return Some(super::hunt_ranges(command));
    }
    let parts = words(command)?;
    let has_head = parts.iter().any(|p| p.first().is_some_and(|s| s == "head"));
    let has_tail = parts.iter().any(|p| p.first().is_some_and(|s| s == "tail"));
    let mut ranges = Vec::new();
    for part in parts {
        let name = part.first()?.as_str();
        if matches!(name, "head" | "tail") && !(has_head && has_tail) {
            return None;
        }
        if name == "grep" {
            if part.len() != 4 || !plausible_path(&part[3]) {
                return None;
            }
            let flag = &part[1];
            if !flag.starts_with("-A") && !flag.starts_with("-B") {
                return None;
            }
            let context: u32 = flag[2..].parse().ok()?;
            let pattern = &part[2];
            if context < 20
                || pattern
                    .chars()
                    .any(|c| !c.is_ascii_alphanumeric() && c != '_')
            {
                return None;
            }
            let source = std::fs::read_to_string(cwd.join(&part[3])).ok()?;
            for (i, line) in source.lines().enumerate() {
                if !line.contains(pattern) {
                    continue;
                }
                let hit = u32::try_from(i + 1).ok()?;
                let (start, end) = if flag.starts_with("-A") {
                    (hit, hit.saturating_add(context))
                } else {
                    (hit.saturating_sub(context).max(1), hit)
                };
                ranges.push(Range {
                    path: part[3].clone(),
                    start,
                    end,
                });
            }
        } else {
            ranges.push(extra_range(&part, cwd)?);
        }
    }
    Some(ranges)
}

/// Narrow the next read to its first 40 requested lines, with a 4 KiB body cap.
/// This intentionally changes coverage: it is advice, not equivalent output.
pub(super) fn next_read(command: &str, cwd: &Path) -> Option<String> {
    let ranges = ranges(command, cwd)?;
    let range = ranges.first()?;
    let path = shell_words::quote(&range.path);
    Some(format!(
        "clud tool run code/slice.py {path} --lines {}-{} --max-bytes 4096",
        range.start,
        range.end.min(range.start.saturating_add(39))
    ))
}

pub(super) fn note(
    event: &str,
    raw: &serde_json::Value,
    payload: &crate::block_bad_cmd::HookPayloadView,
) -> Option<String> {
    if event != "PostToolUse"
        || !super::shell_tool(&payload.tool_name)
        || super::effective_limit() == 0
        || super::opts_out(&payload.command)
    {
        return None;
    }
    let response = raw.get("tool_response")?;
    let stdout = response.get("stdout")?.as_str()?;
    let stderr = response
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let bytes = stdout.len().checked_add(stderr.len())?;
    if bytes <= 4096 {
        return None;
    }
    let command = next_read(&payload.command, &payload.cwd)?;
    Some(format!(
        "[clud byte-budget] {bytes} bytes returned; next narrower read: `{command}`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idioms_and_fail_open() {
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        let file = "src/block_bad_cmd.rs";
        for command in [
            format!("awk 'NR >= 40 && NR <= 200' {file}"),
            format!("cat -n {file}"),
            format!("grep -A100 raw_stdin_bytes {file}"),
            format!("head -n 100 {file}; tail -n 100 {file}"),
        ] {
            assert!(super::super::detect(&command, cwd).is_some(), "{command}");
            let next = next_read(&command, cwd).unwrap();
            assert!(next.contains("--max-bytes 4096"));
        }
        for command in [
            format!("cat -n {file} > out"),
            format!("head -n 100 {file}"),
            format!("awk 'unknown' {file}"),
            format!("grep -A20 'x.*' {file}"),
        ] {
            assert!(super::super::detect(&command, cwd).is_none(), "{command}");
        }
    }
    #[test]
    fn post_output_threshold_and_no_block_contract() {
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        let payload = crate::block_bad_cmd::parse_payload_value(
            &serde_json::json!({
                "tool_name": "Bash", "tool_input": {"command": "cat -n src/block_bad_cmd.rs"}
            }),
            cwd,
        )
        .unwrap();
        for (event, bytes, expected) in [
            ("PreToolUse", 4097, false),
            ("PostToolUse", 4096, false),
            ("PostToolUse", 4097, true),
        ] {
            let raw =
                serde_json::json!({"tool_response": {"stdout": "x".repeat(bytes), "stderr": ""}});
            let result = note(event, &raw, &payload);
            assert_eq!(result.is_some(), expected);
            if let Some(note) = result {
                assert!(note.contains("4097 bytes"));
                assert_eq!(note.lines().count(), 1);
            }
        }
        assert!(note("PostToolUse", &serde_json::json!({}), &payload).is_none());
    }
}
