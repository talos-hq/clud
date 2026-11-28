//! Daemon-side `ResolveInterpreter` handler. Slice 2 of #406 / #410.
//!
//! Reads the JSON request the shim sent (`{"want": "python", "argv":
//! [...]}`), walks `$PATH` for a usable Python 3 interpreter, returns
//! `{"path": "<abs path>"}` on success or `{"status":
//! "not_available", "reason": "..."}` when none is found.
//!
//! Slice 2 ships the **fast path only** — if a system Python 3 exists
//! on `$PATH`, return it. The `uv`-install fallback lands in slice 3
//! (#411). For slice 2 the no-Python case returns:
//!
//! ```json
//! {"status": "not_available",
//!  "reason": "no Python 3 found on PATH and uv install not yet implemented"}
//! ```
//!
//! Protocol envelope (request and response):
//! - JSON line, UTF-8.
//! - Request carries a `v` field (currently 1). Mismatched versions
//!   return `NotAvailable` with `reason: "clud daemon protocol version
//!   mismatch; upgrade clud"` — slice 3 will firm this up when it
//!   adds real version negotiation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Wire protocol version the shim and daemon must agree on. Bumped
/// only by intentional schema changes; the daemon SHOULD accept the
/// last N versions, but slice 2 ships with a strict version match.
pub const SHIM_PROTOCOL_VERSION: u32 = 1;

/// Request the shim sends to the daemon. `v` field added by the
/// envelope wrapper so the daemon can reject mismatched protocol
/// versions before doing any work.
#[derive(Debug, Clone, Deserialize)]
pub struct ResolveRequest {
    #[serde(default = "default_protocol_version")]
    pub v: u32,
    /// `want` is the basename of the executable the caller invoked
    /// (`python`, `python3`, `pip`, …). The daemon uses it to pick the
    /// matching interpreter family.
    pub want: String,
    /// Argv after argv[0]; the daemon uses it for diagnostics and the
    /// session index — slice 2 doesn't otherwise look at it.
    #[serde(default)]
    pub argv: Vec<String>,
}

fn default_protocol_version() -> u32 {
    0 // Triggers a clear version-mismatch error rather than silently
      // accepting a request that omitted the field.
}

/// Response on success — daemon resolved a usable interpreter.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedPath {
    pub path: String,
}

/// Response when no usable interpreter exists. `reason` is a stable
/// stderr string the shim renders verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct NotAvailable {
    pub status: &'static str,
    pub reason: String,
}

impl NotAvailable {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            status: "not_available",
            reason: reason.into(),
        }
    }
}

/// Handle one resolve request and return the JSON response line.
///
/// `path_env` is the value of the child process's `PATH` env var.
/// `which_python` is a callback that scans `path_env` and returns the
/// first matching interpreter — exposed so unit tests can supply
/// deterministic fakes without touching the host's actual `PATH`.
pub fn handle_resolve_request<F>(raw_line: &str, path_env: &str, which_python: F) -> String
where
    F: FnOnce(&str, &str) -> Option<PathBuf>,
{
    let req: ResolveRequest = match serde_json::from_str(raw_line.trim()) {
        Ok(r) => r,
        Err(_) => {
            return render_not_available("protocol error: request not valid JSON; upgrade clud");
        }
    };
    if req.v != SHIM_PROTOCOL_VERSION {
        return render_not_available("clud daemon protocol version mismatch; upgrade clud");
    }

    // Fast path: look up a system interpreter by basename. Slice 3
    // (#411) extends this with `uv` install when the fast path returns
    // None.
    let want = if req.want.is_empty() {
        "python3"
    } else {
        req.want.as_str()
    };
    match which_python(want, path_env) {
        Some(p) => render_resolved(&p),
        None => {
            render_not_available("no Python 3 found on PATH and uv install not yet implemented")
        }
    }
}

fn render_resolved(path: &Path) -> String {
    let resp = ResolvedPath {
        path: path.to_string_lossy().into_owned(),
    };
    serde_json::to_string(&resp).unwrap_or_else(|_| {
        String::from(r#"{"status":"not_available","reason":"internal serialize error"}"#)
    })
}

fn render_not_available(reason: &str) -> String {
    let resp = NotAvailable::new(reason);
    serde_json::to_string(&resp).unwrap_or_else(|_| {
        String::from(r#"{"status":"not_available","reason":"internal serialize error"}"#)
    })
}

/// Walk `path_env` for the first interpreter whose basename matches
/// `want`. On Windows we also try `.exe` and `.cmd` extensions; on
/// Unix the file must have executable permission.
///
/// Matches the family of `want` to the platform Python convention:
///
/// - `python` → tries `python3`, `python3.X` for X in 13..=8, then
///   `python` (legacy).
/// - `python3` / `python3.X` → tries only that name.
/// - Anything else is treated as a literal basename.
pub fn which_python_default(want: &str, path_env: &str) -> Option<PathBuf> {
    for dir in path_env.split(path_sep()).filter(|p| !p.is_empty()) {
        for name in candidate_names(want) {
            if let Some(path) = which(&name, dir) {
                return Some(path);
            }
        }
    }
    None
}

/// Resolve the first executable basename on an explicitly supplied PATH.
/// Empty PATH means not found; this never falls back to the process PATH.
/// Guards must reject relative/empty entries when the shell cwd is unproven.
pub fn which(name: &str, path_env: &str) -> Option<PathBuf> {
    if path_env.is_empty() || name.is_empty() || Path::new(name).components().count() != 1 {
        return None;
    }
    which::which_in(name, Some(path_env), std::env::current_dir().ok()?).ok()
}

/// Candidate basenames to probe, in priority order, for a given
/// `want`. Exposed publicly so the daemon-side server (#411 slice 3+)
/// can reuse the family-resolution rule without copying it.
pub fn candidate_names(want: &str) -> Vec<String> {
    if want == "python" {
        // Prefer python3 family over legacy "python" entirely.
        let mut v: Vec<String> = vec!["python3".to_string()];
        for minor in (8..=13).rev() {
            v.push(format!("python3.{minor}"));
        }
        v.push("python".to_string());
        v
    } else {
        vec![want.to_string()]
    }
}

fn path_sep() -> char {
    if crate::platform::host::WINDOWS {
        ';'
    } else {
        ':'
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;
    use std::fs::File;
    use std::path::PathBuf;

    #[test]
    fn which_explicit_path_contract() {
        let tmp = TemporaryDirectory::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let name = if crate::platform::host::WINDOWS {
            "rm.exe"
        } else {
            "rm"
        };
        for dir in [&a, &b] {
            let file = dir.join(name);
            std::fs::write(&file, b"shim").unwrap();
            crate::platform::shim_fs::set_mode(file, 0o755).unwrap();
        }
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(which("rm", path.to_str().unwrap()), Some(a.join(name)));
        assert_eq!(which("rm", b.to_str().unwrap()), Some(b.join(name)));
        assert_eq!(which("missing", path.to_str().unwrap()), None);
        assert_eq!(which("rm", ""), None);
    }

    fn empty_which(_: &str, _: &str) -> Option<PathBuf> {
        None
    }

    fn fixed_which(path: &'static str) -> impl FnOnce(&str, &str) -> Option<PathBuf> {
        move |_, _| Some(PathBuf::from(path))
    }

    #[test]
    fn rejects_request_with_wrong_protocol_version() {
        let req = r#"{"v": 0, "want": "python", "argv": []}"#;
        let out = handle_resolve_request(req, "", empty_which);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "not_available");
        assert!(v["reason"]
            .as_str()
            .unwrap()
            .contains("protocol version mismatch"));
    }

    #[test]
    fn rejects_request_with_invalid_json() {
        let out = handle_resolve_request("not json", "", empty_which);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "not_available");
        assert!(v["reason"].as_str().unwrap().contains("not valid JSON"));
    }

    #[test]
    fn returns_resolved_path_when_fast_path_hits() {
        let req = r#"{"v": 1, "want": "python", "argv": []}"#;
        let out = handle_resolve_request(req, "", fixed_which("/usr/bin/python3"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["path"], "/usr/bin/python3");
        assert!(
            v.get("status").is_none(),
            "successful response has no status"
        );
    }

    #[test]
    fn returns_not_available_when_fast_path_misses() {
        let req = r#"{"v": 1, "want": "python", "argv": []}"#;
        let out = handle_resolve_request(req, "", empty_which);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "not_available");
        assert!(v["reason"]
            .as_str()
            .unwrap()
            .contains("no Python 3 found on PATH"));
    }

    #[test]
    fn empty_want_defaults_to_python3() {
        let req = r#"{"v": 1, "want": "", "argv": []}"#;
        let want_captured = std::cell::RefCell::new(String::new());
        let recorder = |w: &str, _: &str| -> Option<PathBuf> {
            *want_captured.borrow_mut() = w.to_string();
            None
        };
        let _ = handle_resolve_request(req, "", recorder);
        assert_eq!(*want_captured.borrow(), "python3");
    }

    #[test]
    fn candidate_names_for_python_prefers_python3_family() {
        let names = candidate_names("python");
        assert_eq!(names[0], "python3");
        assert!(names.contains(&"python3.13".to_string()));
        assert!(names.contains(&"python".to_string()));
        // Legacy "python" must come after python3.X
        let py3_idx = names.iter().position(|n| n == "python3").unwrap();
        let legacy_idx = names.iter().position(|n| n == "python").unwrap();
        assert!(
            py3_idx < legacy_idx,
            "python3 must be tried before legacy python: {names:?}"
        );
    }

    #[test]
    fn candidate_names_for_specific_version_passes_through() {
        assert_eq!(
            candidate_names("python3.11"),
            vec!["python3.11".to_string()]
        );
        assert_eq!(candidate_names("pip"), vec!["pip".to_string()]);
    }

    #[test]
    fn which_python_finds_first_match_in_path() {
        let tmp = TemporaryDirectory::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // Put a fake python3 in dir b, none in dir a.
        let exe_name = if crate::platform::host::WINDOWS {
            "python3.exe"
        } else {
            "python3"
        };
        let py_in_b = b.join(exe_name);
        File::create(&py_in_b).unwrap();
        crate::platform::shim_fs::set_mode(&py_in_b, 0o755).unwrap();

        let path_env = format!(
            "{}{}{}",
            a.to_string_lossy(),
            path_sep(),
            b.to_string_lossy()
        );
        let found = which_python_default("python3", &path_env).unwrap();
        assert_eq!(found, py_in_b);
    }

    #[test]
    fn which_python_returns_none_when_path_empty() {
        let found = which_python_default("python3", "");
        assert!(found.is_none());
    }

    #[test]
    fn which_python_returns_none_when_no_match() {
        let tmp = TemporaryDirectory::new().unwrap();
        // Empty dir, no python3 anywhere.
        let found = which_python_default("python3", &tmp.path().to_string_lossy());
        assert!(found.is_none());
    }
}
