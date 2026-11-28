//! `clud tool info [<ref>]` formatter. Slice 3 of #427.
//!
//! Resolves the supplied reference (or "most recent" when omitted) to a
//! single invocation, then prints the state block per #427's spec:
//! id / long-form / session info / tool / pid / state / elapsed / last N
//! lines of stdout + stderr from the per-invocation JSONL.

use std::fs;
use std::io;
use std::path::Path;

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;

use crate::session_index::SessionContext;
use crate::tool_query::{read_invocations, resolve_ref, Invocation, ResolveError};

const DEFAULT_LAST_LINES: usize = 20;

/// Run `clud tool info`. Returns the desired process exit code.
pub fn run(reference: Option<&str>, pid: Option<u32>, lines: usize, json: bool) -> io::Result<i32> {
    let Some(ctx) = SessionContext::from_env() else {
        eprintln!("[clud] tool info: no clud session active (CLUD_SESSION_PID unset)");
        return Ok(2);
    };
    let invocations = read_invocations(&ctx)?;
    let tool_id = match resolve_ref(&invocations, ctx.session_pid, reference, pid) {
        Ok(id) => id,
        Err(err) => {
            print_resolve_error(&err, &invocations);
            return Ok(2);
        }
    };
    let Some(inv) = invocations.iter().find(|i| i.tool_id == tool_id) else {
        eprintln!("[clud] tool info: resolved tool_id {tool_id} disappeared from index");
        return Ok(2);
    };

    let last_lines = if lines == 0 {
        DEFAULT_LAST_LINES
    } else {
        lines
    };

    if json {
        print_json(&ctx, inv, last_lines)?;
    } else {
        print_human(&ctx, inv, last_lines)?;
    }
    Ok(0)
}

fn print_resolve_error(err: &ResolveError, invocations: &[Invocation]) {
    eprintln!("[clud] tool info: {err}");
    if let ResolveError::Ambiguous { matches, .. } = err {
        for &id in matches {
            if let Some(inv) = invocations.iter().find(|i| i.tool_id == id) {
                eprintln!(
                    "  - id {} → tool {}, pid {}",
                    inv.tool_id, inv.tool, inv.pid
                );
            }
        }
    }
}

fn print_human(ctx: &SessionContext, inv: &Invocation, last_lines: usize) -> io::Result<()> {
    let long = inv.long_form(ctx.session_pid);
    println!("id:            {} (long-form {long})", inv.tool_id);
    println!(
        "session:       daemon pid {} (start_epoch {})",
        ctx.session_pid, ctx.session_start_epoch
    );
    println!("tool:          {}", inv.tool);
    if !inv.args.is_empty() {
        println!("args:          {}", inv.args.join(" "));
    }
    println!(
        "pid:           {} (start_time {})",
        inv.pid, inv.pid_start_time
    );
    println!("state:         {}", inv.state.label());
    if let Some(code) = inv.exit_code {
        println!("exit_code:     {code}");
    }
    if let Some(reason) = &inv.reason {
        println!("reason:        {reason}");
    }
    println!("started_at_ms: {}", inv.started_at_ms);
    if let Some(end) = inv.ended_at_ms {
        println!(
            "ended_at_ms:   {}  (elapsed {} ms)",
            end,
            end.saturating_sub(inv.started_at_ms)
        );
    } else {
        println!("ended_at_ms:   <still running>");
    }

    let log_dir = ctx.tool_log_dir(inv.tool_id);
    println!("last stdout ({} lines):", last_lines);
    print_last_lines(&log_dir.join("stdout.jsonl"), last_lines)?;
    println!("last stderr ({} lines):", last_lines);
    print_last_lines(&log_dir.join("stderr.jsonl"), last_lines)?;
    Ok(())
}

fn print_json(ctx: &SessionContext, inv: &Invocation, last_lines: usize) -> io::Result<()> {
    use std::io::Write;
    let log_dir = ctx.tool_log_dir(inv.tool_id);
    let value = serde_json::json!({
        "tool_id": inv.tool_id,
        "long_id": inv.long_form(ctx.session_pid),
        "session_pid": ctx.session_pid,
        "session_start_epoch": ctx.session_start_epoch,
        "tool": inv.tool,
        "args": inv.args,
        "pid": inv.pid,
        "pid_start_time": inv.pid_start_time,
        "state": inv.state.label(),
        "exit_code": inv.exit_code,
        "reason": inv.reason,
        "started_at_ms": inv.started_at_ms as u64,
        "ended_at_ms": inv.ended_at_ms.map(|v| v as u64),
        "last_stdout_lines": read_last_lines(&log_dir.join("stdout.jsonl"), last_lines)?,
        "last_stderr_lines": read_last_lines(&log_dir.join("stderr.jsonl"), last_lines)?,
    });
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &value)?;
    out.write_all(b"\n")
}

fn print_last_lines(path: &Path, n: usize) -> io::Result<()> {
    let decoded = read_last_lines(path, n)?;
    for line in decoded {
        // The JSONL "bytes" are arbitrary binary; lossily render for the
        // human-readable view. The --json path returns base64 instead.
        println!("  {}", line);
    }
    Ok(())
}

fn read_last_lines(path: &Path, n: usize) -> io::Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)?;
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value): Result<serde_json::Value, _> = serde_json::from_str(trimmed) else {
            continue;
        };
        let Some(b64) = value.get("bytes").and_then(|v| v.as_str()) else {
            continue;
        };
        let decoded = STANDARD_NO_PAD.decode(b64).unwrap_or_default();
        // Split on newlines so each chunk decoded line shows separately.
        for sub in String::from_utf8_lossy(&decoded).lines() {
            lines.push(sub.to_string());
        }
    }
    if lines.len() > n {
        let cut = lines.len() - n;
        lines.drain(..cut);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use base64::Engine;
    use kernal_api::platform::fs::TemporaryDirectory;
    use std::io::Write;

    fn write_jsonl_chunk(path: &Path, bytes: &[u8]) {
        let encoded = STANDARD_NO_PAD.encode(bytes);
        let line = format!(r#"{{"v":1,"ts_ms":0,"stream":"stdout","bytes":"{encoded}"}}"#);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(line.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
    }

    #[test]
    fn read_last_lines_returns_empty_for_missing_file() {
        let tmp = TemporaryDirectory::new().unwrap();
        let missing = tmp.path().join("absent.jsonl");
        assert!(read_last_lines(&missing, 10).unwrap().is_empty());
    }

    #[test]
    fn read_last_lines_decodes_and_splits_chunks() {
        let tmp = TemporaryDirectory::new().unwrap();
        let path = tmp.path().join("stdout.jsonl");
        write_jsonl_chunk(&path, b"line1\nline2\n");
        write_jsonl_chunk(&path, b"line3\n");
        let lines = read_last_lines(&path, 10).unwrap();
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn read_last_lines_caps_at_n() {
        let tmp = TemporaryDirectory::new().unwrap();
        let path = tmp.path().join("stdout.jsonl");
        for i in 0..50 {
            write_jsonl_chunk(&path, format!("line{i}\n").as_bytes());
        }
        let lines = read_last_lines(&path, 5).unwrap();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "line45");
        assert_eq!(lines[4], "line49");
    }
}
