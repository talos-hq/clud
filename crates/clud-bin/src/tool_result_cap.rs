//! Shell text budget at the Claude-to-Codex subscription bridge boundary.
//! Complete edge lines survive; the recovery marker is additional to the budget.
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static SPILLS: OnceLock<Mutex<HashMap<[u8; 32], PathBuf>>> = OnceLock::new();

const DEFAULT_LIMIT: usize = 4096;

pub(crate) fn configured_limit() -> usize {
    parse_limit(std::env::var("CLUD_TOOL_RESULT_MAX_BYTES").ok().as_deref())
}

fn parse_limit(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_LIMIT)
}

/// Persist before clipping; a failed spill fails the request rather than losing data.
pub(crate) fn cap(text: &str, limit: usize) -> io::Result<String> {
    if limit == 0 || text.len() <= limit {
        return Ok(text.to_owned());
    }
    // Stable pointers keep replayed history byte-identical across turns.
    let mut spills = SPILLS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let key: [u8; 32] = Sha256::digest(text.as_bytes()).into();
    if let Some(path) = spills.get(&key) {
        if std::fs::read(path).is_ok_and(|bytes| bytes == text.as_bytes()) {
            return Ok(render(text, limit, path));
        }
    }
    let mut artifact = tempfile::Builder::new()
        .prefix("clud-tool-result-")
        .suffix(".txt")
        .tempfile()?;
    artifact.write_all(text.as_bytes())?;
    artifact.flush()?;
    let (_, path) = artifact.keep().map_err(|error| error.error)?;
    let emitted = render(text, limit, &path);
    spills.insert(key, path);
    Ok(emitted)
}

fn render(text: &str, limit: usize, path: &std::path::Path) -> String {
    let data = text.as_bytes();
    let edge = limit / 2;
    let head_end = data[..edge]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |i| i + 1);
    let mut tail_start = data.len() - edge;
    if data.get(tail_start.wrapping_sub(1)) != Some(&b'\n') {
        tail_start = data[tail_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(data.len(), |i| tail_start + i + 1);
    }
    let omitted = &data[head_end..tail_start];
    let lines = omitted.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(!omitted.is_empty() && !omitted.ends_with(b"\n"));
    let start = data[..head_end]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    let end = start + lines - 1;
    let path = shell_words::quote(&path.to_string_lossy()).into_owned();
    let marker = format!("[{lines} lines / {} bytes omitted; re-read with clud tool run code/slice.py {path} --lines {start}-{end}; {limit}-byte head/tail budget plus marker (may exceed cap)]\n", omitted.len());
    format!("{}{marker}{}", &text[..head_end], &text[tail_start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_and_disabled() {
        for limit in [0, 4096, 8192] {
            let text = "é".repeat(2048);
            assert_eq!(cap(&text, limit).unwrap(), text);
        }
        assert_eq!(parse_limit(None), 4096);
        assert_eq!(parse_limit(Some("invalid")), 4096);
        assert_eq!(parse_limit(Some("0")), 0);
        assert_eq!(parse_limit(Some("128")), 128);
    }

    #[test]
    fn complete_edges_and_recoverable_counts() {
        let text = (0..1000)
            .map(|i| format!("line {i:04} é\n"))
            .collect::<String>();
        let emitted = cap(&text, 4096).unwrap();
        assert_eq!(emitted, cap(&text, 4096).unwrap());
        let marker_start = emitted.find('[').unwrap();
        let marker_end = emitted[marker_start..].find(']').unwrap() + marker_start + 2;
        let head = &emitted[..marker_start];
        let tail = &emitted[marker_end..];
        assert!(text.starts_with(head) && text.ends_with(tail));
        assert!(head.len() <= 2048 && tail.len() <= 2048);
        assert!(head.ends_with('\n') && tail.starts_with("line "));
        assert_eq!(head.len(), 2041);
        assert_eq!(tail.len(), 2041);
        let marker = &emitted[marker_start..marker_end];
        assert!(marker.contains("686 lines / 8918 bytes omitted"));
        assert!(marker.contains("--lines 158-843"));
        let path = marker
            .split("code/slice.py ")
            .nth(1)
            .unwrap()
            .split(" --lines")
            .next()
            .unwrap();
        let paths = shell_words::split(path).unwrap();
        assert_eq!(paths.len(), 1);
        let path = &paths[0];
        assert_eq!(std::fs::read(path).unwrap(), text.as_bytes());
        // TempPath cleanup keeps tests from leaving persistent production spills.
        drop(tempfile::TempPath::try_from_path(path).unwrap());
    }

    #[test]
    fn oversized_lines_and_marker_larger_than_cap() {
        let text = format!("{}\nTAIL", "é".repeat(3000));
        let path = std::path::Path::new("/tmp/output.txt");
        let emitted = render(&text, 4096, path);
        assert!(emitted.starts_with("[1 lines / 6001 bytes omitted;"));
        assert!(emitted.ends_with("\nTAIL"));
        let tiny = cap("abc", 1).unwrap();
        assert!(tiny.len() > 1);
        assert!(tiny.contains("1 lines / 3 bytes omitted"));
        assert!(tiny.contains("1-byte head/tail budget plus marker (may exceed cap)"));
        assert!(!tiny.ends_with("abc"));
    }
}
