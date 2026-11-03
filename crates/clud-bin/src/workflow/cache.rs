//! Resume support for `clud workflow --resume` (#2471).
//!
//! Every successful `agent()` result is appended to
//! `.clud/workflow/cache/<script>.jsonl`, keyed by a hash of the call's
//! `(prompt, label, phase, model, schema)`. The request id is excluded on
//! purpose: it depends on issue order, while the inputs identify the work.
//! With `--resume`, a call whose key is cached returns the stored value
//! instead of launching an agent; an edited prompt or option misses.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::engine::AgentRequest;

/// Stable cache key for one call's inputs.
pub fn key(req: &AgentRequest) -> String {
    let canonical = json!({
        "prompt": req.prompt,
        "label": req.label,
        "phase": req.phase,
        "model": req.model,
        "schema": req.schema,
    });
    let digest = Sha256::digest(canonical.to_string().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// On-disk result cache for one workflow script.
pub struct ResultCache {
    path: PathBuf,
    entries: HashMap<String, Value>,
}

impl ResultCache {
    /// Cache file for `script` under `git_root`.
    pub fn path_for(git_root: &Path, script: &Path) -> PathBuf {
        let stem = script
            .file_stem()
            .map_or_else(|| "workflow".into(), |s| s.to_string_lossy().into_owned());
        git_root
            .join(".clud")
            .join("workflow")
            .join("cache")
            .join(format!("{stem}.jsonl"))
    }

    /// Open the cache. With `resume`, previously stored results are loaded;
    /// otherwise the file is truncated so a fresh run starts clean.
    pub fn open(path: PathBuf, resume: bool) -> Self {
        let mut entries = HashMap::new();
        if resume {
            if let Ok(text) = std::fs::read_to_string(&path) {
                for line in text.lines() {
                    if let Ok(v) = serde_json::from_str::<Value>(line) {
                        if let (Some(k), Some(ok)) =
                            (v.get("key").and_then(Value::as_str), v.get("ok"))
                        {
                            entries.insert(k.to_string(), ok.clone());
                        }
                    }
                }
            }
        } else if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::write(&path, "");
        }
        Self { path, entries }
    }

    pub fn get(&self, req: &AgentRequest) -> Option<&Value> {
        self.entries.get(&key(req))
    }

    /// Store a successful result (failures are never cached).
    pub fn record(&mut self, req: &AgentRequest, ok: &Value) {
        let k = key(req);
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{}", json!({"key": k, "ok": ok}));
        }
        self.entries.insert(k, ok.clone());
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
