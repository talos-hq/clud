//! `clud workflow <script.js> [--args JSON]`: run a workflow script with
//! the real `clud -p` runner, render an ASCII status board per batch, and
//! leave artifacts under `.clud/workflow/<run>/` plus a visible
//! `WORKFLOW_DONE.md` at the project root.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::engine::{run_script, AgentRequest, WorkflowOutcome};
use super::runner::{agent_argv, run_batch};
use crate::stream_json::IterationSummary;

/// Status of one agent row on the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Running,
    Done,
    Failed,
}

/// One board row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: u32,
    pub label: String,
    pub phase: String,
    pub status: RowStatus,
    pub detail: String,
}

/// Render the ASCII status board (pure; no ANSI escapes).
pub fn render_board(title: &str, rows: &[Row]) -> String {
    const LABEL: usize = 24;
    const PHASE: usize = 12;
    const DETAIL: usize = 28;
    let border = format!(
        "+-----+-{}-+-{}-+---------+-{}-+",
        "-".repeat(LABEL),
        "-".repeat(PHASE),
        "-".repeat(DETAIL)
    );
    let cut = |s: &str, n: usize| -> String {
        if s.chars().count() <= n {
            s.to_string()
        } else {
            let mut t: String = s.chars().take(n - 1).collect();
            t.push('~');
            t
        }
    };
    let mut out = format!("[workflow] {title}\n{border}\n");
    out.push_str(&format!(
        "| {:>3} | {:<LABEL$} | {:<PHASE$} | {:<7} | {:<DETAIL$} |\n{border}\n",
        "#", "agent", "phase", "status", "detail"
    ));
    for row in rows {
        let status = match row.status {
            RowStatus::Running => "running",
            RowStatus::Done => "done",
            RowStatus::Failed => "FAILED",
        };
        out.push_str(&format!(
            "| {:>3} | {:<LABEL$} | {:<PHASE$} | {:<7} | {:<DETAIL$} |\n",
            row.id,
            cut(&row.label, LABEL),
            cut(&row.phase, PHASE),
            status,
            cut(&row.detail, DETAIL)
        ));
    }
    out.push_str(&border);
    out
}

fn row_for(req: &AgentRequest) -> Row {
    Row {
        id: req.id,
        label: req
            .label
            .clone()
            .unwrap_or_else(|| req.prompt.lines().next().unwrap_or_default().to_string()),
        phase: req.phase.clone().unwrap_or_default(),
        status: RowStatus::Running,
        detail: String::new(),
    }
}

/// Render the root `WORKFLOW_DONE.md`.
pub fn render_done(
    outcome: &Result<WorkflowOutcome, String>,
    rows: &[Row],
    run_dir_rel: &str,
) -> String {
    let mut out = String::from("# WORKFLOW_DONE\n\n");
    match outcome {
        Ok(o) => {
            out.push_str(&format!(
                "- Workflow: {} — {}\n",
                o.meta.name, o.meta.description
            ));
            out.push_str("- Outcome: ok\n");
        }
        Err(e) => out.push_str(&format!("- Outcome: FAILED: {e}\n")),
    }
    out.push_str(&format!("- Agents: {}\n\n```\n", rows.len()));
    out.push_str(&render_board("final", rows));
    out.push_str("\n```\n\nArtifacts (hidden directory):\n\n");
    for name in ["result.json", "agents.json"] {
        out.push_str(&format!("- `{run_dir_rel}/{name}`\n"));
    }
    out
}

/// Entry point for `clud workflow`. Returns the process exit code.
pub fn run(script: &Path, args_json: Option<&str>, dry_run: bool) -> i32 {
    let source = match std::fs::read_to_string(script) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[clud] cannot read workflow {}: {e}", script.display());
            return 2;
        }
    };
    let args: Value = match args_json.map(serde_json::from_str).transpose() {
        Ok(v) => v.unwrap_or(Value::Null),
        Err(e) => {
            eprintln!("[clud] --args is not valid JSON: {e}");
            return 2;
        }
    };
    let clud_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("clud"));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let git_root = crate::loop_spec::git_root_from(&cwd);

    let mut rows: Vec<Row> = Vec::new();
    let outcome = {
        let mut runner = |batch: &[AgentRequest]| -> Vec<Result<Value, String>> {
            let start = rows.len();
            rows.extend(batch.iter().map(row_for));
            if dry_run {
                for req in batch {
                    println!(
                        "{}",
                        serde_json::json!({"agent": req, "argv": agent_argv(&clud_exe, req)})
                    );
                }
                for row in &mut rows[start..] {
                    row.status = RowStatus::Done;
                    row.detail = "dry-run".into();
                }
                return batch.iter().map(|r| Ok(dry_run_value(r))).collect();
            }
            eprintln!("{}", render_board("running", &rows));
            let mut summaries = vec![IterationSummary::default(); batch.len()];
            let first_id = batch.first().map_or(0, |r| r.id);
            let results = run_batch(&clud_exe, Some(cwd.clone()), batch, &mut |id, line| {
                if let Some(s) = summaries.get_mut((id - first_id) as usize) {
                    s.observe(line);
                }
            });
            for ((row, res), summary) in rows[start..].iter_mut().zip(&results).zip(&summaries) {
                row.status = if res.is_ok() {
                    RowStatus::Done
                } else {
                    RowStatus::Failed
                };
                row.detail = match (res, summary.cost_usd, summary.turns) {
                    (Err(e), _, _) => e.clone(),
                    (Ok(_), Some(c), Some(t)) => format!("{t} turns ${c:.2}"),
                    (Ok(_), _, _) => "ok".into(),
                };
            }
            eprintln!("{}", render_board("batch finished", &rows));
            results
        };
        run_script(&source, &args, &mut runner)
    };

    if !dry_run {
        write_artifacts(&git_root, script, &outcome, &rows);
    }
    match &outcome {
        Ok(o) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&o.result).unwrap_or_default()
            );
            0
        }
        Err(e) => {
            eprintln!("[clud] workflow failed: {e}");
            1
        }
    }
}

/// Dry runs resolve schema calls with `{}` and text calls with `""`.
fn dry_run_value(req: &AgentRequest) -> Value {
    if req.schema.is_some() {
        Value::Object(serde_json::Map::new())
    } else {
        Value::String(String::new())
    }
}

fn write_artifacts(
    git_root: &Path,
    script: &Path,
    outcome: &Result<WorkflowOutcome, String>,
    rows: &[Row],
) {
    let stem = script
        .file_stem()
        .map_or_else(|| "workflow".into(), |s| s.to_string_lossy().into_owned());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let rel = format!(".clud/workflow/{stem}-{stamp}");
    let dir = git_root.join(&rel);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let result = match outcome {
        Ok(o) => {
            serde_json::json!({"ok": true, "meta": o.meta, "result": o.result, "phases": o.phases})
        }
        Err(e) => serde_json::json!({"ok": false, "error": e}),
    };
    let _ = std::fs::write(dir.join("result.json"), format!("{result:#}\n"));
    if let Ok(o) = outcome {
        let agents: Vec<Value> = o
            .agents
            .iter()
            .map(|(req, res)| match res {
                Ok(v) => serde_json::json!({"request": req, "ok": v}),
                Err(e) => serde_json::json!({"request": req, "error": e}),
            })
            .collect();
        let _ = std::fs::write(
            dir.join("agents.json"),
            format!("{:#}\n", Value::Array(agents)),
        );
    }
    let _ = std::fs::write(
        git_root.join("WORKFLOW_DONE.md"),
        render_done(outcome, rows, &rel),
    );
}
