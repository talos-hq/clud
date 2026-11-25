use std::io::Write;
use std::path::Path;

const PROMPT_PREFIX: &str = "CLUD LOOP COMPLETION VERIFIER\n";

pub fn should_write_marker(threshold: u32, iteration: u32) -> bool {
    threshold > 0
        && iteration >= threshold
        && (iteration == threshold || std::env::var_os("CLUD_MOCK_LOOP_CLAIM_ONCE").is_none())
}

pub fn run(args: &[String]) -> bool {
    let Some(prompt) = args.last().filter(|arg| arg.starts_with(PROMPT_PREFIX)) else {
        if let Some(path) = std::env::var_os("CLUD_MOCK_LOOP_TASK_EVIDENCE") {
            let feedback = args.iter().any(|arg| {
                arg.contains("Completion verification rejected the previous claim")
                    && arg.contains("Group B remains open")
            });
            std::fs::write(path, if feedback { "A B" } else { "A" })
                .expect("write mock task evidence");
        }
        return false;
    };
    let verdict = match std::env::var("CLUD_MOCK_LOOP_VERDICTS") {
        Ok(script) => scripted_verdict(prompt, &script),
        Err(_) => serde_json::json!({"status": "complete", "reason": "mock verification"}),
    };
    let text = format!("CLUD_LOOP_VERDICT {verdict}");
    if args.iter().any(|arg| arg == "--json") {
        let event = serde_json::json!({"type": "item.completed", "item": {"type": "agent_message", "text": text}});
        if std::env::var_os("CLUD_MOCK_LOOP_VERDICT_STDERR").is_some() {
            eprintln!("{event}");
        } else {
            println!("{event}");
        }
    } else if args.iter().any(|arg| arg == "stream-json") {
        println!("{}", serde_json::json!({"type": "result", "result": text}));
    } else {
        println!("{text}");
    }
    true
}

fn scripted_verdict(prompt: &str, script: &str) -> serde_json::Value {
    let log = Path::new(".clud/loop/mock-verifier.jsonl");
    std::fs::create_dir_all(log.parent().unwrap()).expect("create mock loop log directory");
    let previous = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .count();
    let verdicts: Vec<serde_json::Value> =
        serde_json::from_str(script).expect("parse mock loop verdicts");
    let verdict = verdicts
        .get(previous)
        .expect("unexpected verifier invocation");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .expect("open mock verifier log");
    let evidence = std::env::var_os("CLUD_MOCK_LOOP_TASK_EVIDENCE")
        .and_then(|path| std::fs::read_to_string(path).ok());
    writeln!(
        file,
        "{}",
        serde_json::json!({"prompt": prompt, "evidence": evidence})
    )
    .expect("write mock verifier log");
    verdict.clone()
}
