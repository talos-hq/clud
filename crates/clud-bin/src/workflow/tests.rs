use super::meta::wrap_source;
use super::*;
use serde_json::json;

const REVIEW: &str = r#"export const meta = {
  name: 'review-changes',
  description: 'Review changed files, verify each finding { not a brace }',
  phases: [{ title: 'Review' }, { title: 'Verify' }],
}
const DIMENSIONS = [{key: 'bugs'}, {key: 'perf'}]
const results = await pipeline(
  DIMENSIONS,
  d => agent(`review ${d.key}`, {label: `review:${d.key}`, phase: 'Review', schema: {type: 'object'}}),
  review => parallel(review.findings.map(f => () =>
    agent(`verify ${f}`, {label: `verify:${f}`, phase: 'Verify'}).then(v => ({f, v}))
  ))
)
return { confirmed: results.flat().filter(x => x.v === 'real').map(x => x.f), mode: args.mode }
"#;

fn fake(
    log: &mut Vec<Vec<String>>,
) -> impl FnMut(&[AgentRequest]) -> Vec<Result<serde_json::Value, String>> + '_ {
    move |batch: &[AgentRequest]| {
        log.push(
            batch
                .iter()
                .map(|r| r.label.clone().unwrap_or_default())
                .collect(),
        );
        batch
            .iter()
            .map(|r| {
                Ok(match r.prompt.as_str() {
                    "review bugs" => json!({"findings": ["a", "b"]}),
                    "review perf" => json!({"findings": ["c"]}),
                    "verify b" => json!("noise"),
                    _ => json!("real"),
                })
            })
            .collect()
    }
}

#[test]
fn runs_pipeline_parallel_workflow_in_concurrent_batches() {
    let mut log = Vec::new();
    let mut runner = fake(&mut log);
    let out = run_script(REVIEW, &json!({"mode": "fast"}), &mut runner).unwrap();
    drop(runner);
    assert_eq!(out.meta.name, "review-changes");
    assert_eq!(out.meta.phases.len(), 2);
    assert_eq!(out.result, json!({"confirmed": ["a", "c"], "mode": "fast"}));
    assert_eq!(out.agents.len(), 5);
    // Both reviews go out together, then all three verifies together.
    assert_eq!(log[0], vec!["review:bugs", "review:perf"]);
    let mut second = log[1].clone();
    second.sort();
    assert_eq!(second, vec!["verify:a", "verify:b", "verify:c"]);
    assert_eq!(log.len(), 2);
    assert_eq!(out.agents[0].0.phase.as_deref(), Some("Review"));
    assert_eq!(out.agents[0].0.schema, Some(json!({"type": "object"})));
}

#[test]
fn agent_failure_rejects_and_can_be_caught() {
    let src = "export const meta = { name: 'x', description: 'y' }\n\
               try { await agent('boom') } catch (e) { return 'caught: ' + e.message }";
    let mut runner =
        |b: &[AgentRequest]| b.iter().map(|_| Err("step failed".to_string())).collect();
    let out = run_script(src, &json!({}), &mut runner).unwrap();
    assert_eq!(out.result, json!("caught: step failed"));
}

#[test]
fn uncaught_agent_failure_fails_the_run() {
    let src = "export const meta = { name: 'x', description: 'y' }\nawait agent('boom')";
    let mut runner =
        |b: &[AgentRequest]| b.iter().map(|_| Err("step failed".to_string())).collect();
    let err = run_script(src, &json!({}), &mut runner).unwrap_err();
    assert!(err.contains("step failed"), "{err}");
}

#[test]
fn phase_sets_default_phase_and_unknown_phase_is_rejected() {
    let src = "export const meta = { name: 'x', description: 'y', phases: [{title: 'A'}] }\n\
               phase('A'); await agent('p'); phase('B')";
    let mut seen = Vec::new();
    let mut runner = |b: &[AgentRequest]| {
        seen.extend(b.iter().map(|r| r.phase.clone()));
        b.iter().map(|_| Ok(json!("ok"))).collect()
    };
    let err = run_script(src, &json!({}), &mut runner).unwrap_err();
    assert!(err.contains("phase(\"B\") is not declared"), "{err}");
    assert_eq!(seen, vec![Some("A".to_string())]);
}

#[test]
fn rejects_missing_or_invalid_meta() {
    let mut runner = |b: &[AgentRequest]| b.iter().map(|_| Ok(json!(1))).collect();
    let err = run_script("return 1", &json!({}), &mut runner).unwrap_err();
    assert!(err.contains("must begin with"), "{err}");
    let err = run_script(
        "export const meta = { name: 'x' }\nreturn 1",
        &json!({}),
        &mut runner,
    )
    .unwrap_err();
    assert!(err.contains("invalid workflow meta"), "{err}");
}

#[test]
fn wrap_source_skips_braces_in_strings_and_comments() {
    let w =
        wrap_source("export const meta = { name: '}', /* } */ description: \"{\" } // }\nreturn 1")
            .unwrap();
    assert!(
        w.contains("const meta = { name: '}', /* } */ description: \"{\" };"),
        "{w}"
    );
    assert!(
        w.contains("__clud_meta(JSON.stringify(meta));\n // }\nreturn 1"),
        "{w}"
    );
}

mod runner_tests {
    use super::super::runner::{agent_argv, parse_agent_result};
    use super::*;
    use std::path::Path;

    fn req(schema: Option<serde_json::Value>) -> AgentRequest {
        AgentRequest {
            id: 1,
            prompt: "do it".into(),
            label: None,
            phase: None,
            model: Some("claude-haiku-5-5".into()),
            schema,
        }
    }

    #[test]
    fn argv_is_headless_stream_json_with_model_and_schema() {
        let argv = agent_argv(Path::new("clud"), &req(Some(json!({"type": "object"}))));
        assert_eq!(
            argv,
            vec![
                "clud",
                "-p",
                "do it",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-haiku-5-5",
                "--json-schema",
                r#"{"type":"object"}"#,
            ]
        );
        assert!(!agent_argv(Path::new("clud"), &req(None)).contains(&"--json-schema".to_string()));
    }

    #[test]
    fn parses_text_structured_error_and_missing_results() {
        let text = "{\"type\":\"assistant\"}\n{\"type\":\"result\",\"result\":\"all good\"}\n";
        assert_eq!(
            parse_agent_result(text, false, 0).unwrap(),
            json!("all good")
        );
        let structured = r#"{"type":"result","result":"x","structured_output":{"ok":true}}"#;
        assert_eq!(
            parse_agent_result(structured, true, 0).unwrap(),
            json!({"ok": true})
        );
        assert!(
            parse_agent_result(r#"{"type":"result","result":"x"}"#, true, 0)
                .unwrap_err()
                .contains("no structured_output")
        );
        let err = r#"{"type":"result","is_error":true,"result":"rate limited"}"#;
        assert_eq!(
            parse_agent_result(err, false, 1).unwrap_err(),
            "rate limited"
        );
        assert!(parse_agent_result("garbage", false, 7)
            .unwrap_err()
            .contains("exited 7 without a result"));
    }
}

mod board_tests {
    use super::super::cli::{render_board, render_done, Row, RowStatus};

    fn rows() -> Vec<Row> {
        vec![
            Row {
                id: 1,
                label: "review:bugs".into(),
                phase: "Review".into(),
                status: RowStatus::Done,
                detail: "3 turns $0.10".into(),
            },
            Row {
                id: 2,
                label: "a-very-long-label-that-must-be-truncated".into(),
                phase: "Verify".into(),
                status: RowStatus::Failed,
                detail: "rate limited".into(),
            },
            Row {
                id: 3,
                label: "verify:c".into(),
                phase: String::new(),
                status: RowStatus::Running,
                detail: String::new(),
            },
        ]
    }

    #[test]
    fn board_rows_have_uniform_width_and_status_words() {
        let board = render_board("running", &rows());
        let lines: Vec<&str> = board.lines().skip(1).collect();
        let w = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == w), "{board}");
        assert!(board.contains("| done    |"));
        assert!(board.contains("| FAILED  |"));
        assert!(board.contains("| running |"));
        assert!(board.contains("a-very-long-label-that-~"), "{board}");
    }

    #[test]
    fn done_file_points_at_artifacts_and_reports_failure() {
        let text = render_done(&Err("boom".into()), &rows(), ".clud/workflow/x-1");
        assert!(text.starts_with("# WORKFLOW_DONE"));
        assert!(text.contains("Outcome: FAILED: boom"));
        assert!(text.contains("`.clud/workflow/x-1/result.json`"));
        assert!(text.contains("Agents: 3"));
    }
}
