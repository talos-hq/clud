//! `clud inject <session> "<message>"`: the human/CLI face of
//! `POST /v1/sessions/{id}/inject` (docs/architecture/session-inject.md).
//! Never prompts. Blocks until the injection is delivered or failed unless
//! `--detach` is given.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Parsed `clud inject` options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectOptions {
    pub session: String,
    pub message: String,
    pub interrupt: bool,
    pub client_msg_id: Option<String>,
    pub from: Option<String>,
    pub ticket: Option<String>,
    pub detach: bool,
    pub timeout_secs: u64,
    pub json: bool,
}

/// The stable id a retry of the same CLI call reuses when none is given.
pub fn default_client_msg_id(session: &str, message: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{session}\n{message}").as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("cli-{hex}")
}

/// The JSON body sent to the daemon.
pub fn request_body(options: &InjectOptions) -> Value {
    let from = options
        .from
        .clone()
        .or_else(|| std::env::var("CLUD_BOARD_ID").ok())
        .or_else(|| std::env::var("CLUD_SESSION_ID").ok())
        .unwrap_or_else(|| "cli".to_string());
    serde_json::json!({
        "message": options.message,
        "client_msg_id": options
            .client_msg_id
            .clone()
            .unwrap_or_else(|| default_client_msg_id(&options.session, &options.message)),
        "interrupt": options.interrupt,
        "from": from,
        "ticket": options.ticket,
    })
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
}

fn call(request: ureq::Request, body: Option<&Value>) -> Result<(u16, Value), String> {
    let response = match body {
        Some(body) => request
            .set("Content-Type", "application/json")
            .send_string(&body.to_string()),
        None => request.call(),
    };
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => response,
        Err(error) => return Err(format!("daemon API unreachable: {error}")),
    };
    let status = response.status();
    let value = response
        .into_string()
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null);
    Ok((status, value))
}

/// Run `clud inject`; returns the process exit code.
pub fn run(state_dir: &Path, options: &InjectOptions) -> i32 {
    if let Err(error) = super::ensure_daemon(state_dir) {
        eprintln!("[clud] daemon unavailable: {error}");
        return 1;
    }
    let (port, token) = match super::http::read_api_info(state_dir) {
        Ok((_, Some(port), Some(token), _)) => (port, token),
        _ => {
            eprintln!("[clud] daemon API listener unavailable; restart the daemon");
            return 1;
        }
    };
    match inject(port, &token, options) {
        Ok(result) => report(&result, options.json),
        Err(message) => {
            if options.json {
                println!(
                    "{}",
                    serde_json::json!({"accepted": false, "error": message})
                );
            } else {
                eprintln!("[clud] inject failed: {message}");
            }
            1
        }
    }
}

/// POST the injection, then (unless detached) wait for it to settle.
pub fn inject(port: u16, token: &str, options: &InjectOptions) -> Result<Value, String> {
    let base = format!("http://127.0.0.1:{port}/v1/sessions/{}", options.session);
    let agent = agent();
    let auth = format!("Bearer {token}");
    let (status, reply) = call(
        agent
            .post(&format!("{base}/inject"))
            .set("Authorization", &auth),
        Some(&request_body(options)),
    )?;
    if status != 202 {
        let code = reply["code"].as_str().unwrap_or("error");
        let message = reply["message"].as_str().unwrap_or("request rejected");
        return Err(format!("{code}: {message} (HTTP {status})"));
    }
    if options.detach || reply["state"] != "queued" {
        return Ok(reply);
    }
    let seq = reply["seq"].as_u64().unwrap_or(0);
    let deadline = Instant::now() + Duration::from_secs(options.timeout_secs);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        let (status, record) = call(agent.get(&base).set("Authorization", &auth), None)?;
        if status != 200 {
            return Err(format!("session lookup failed (HTTP {status})"));
        }
        let entry = record["injections"]
            .as_array()
            .and_then(|entries| entries.iter().find(|entry| entry["seq"] == seq));
        if let Some(entry) = entry.filter(|entry| entry["state"] != "queued") {
            let mut settled = reply.clone();
            settled["state"] = entry["state"].clone();
            settled["turn_id"] = entry["turn_id"].clone();
            settled["error"] = entry["error"].clone();
            settled["accepted"] = Value::Bool(entry["state"] != "failed");
            return Ok(settled);
        }
    }
    let mut pending = reply;
    pending["note"] = Value::String(format!(
        "still queued after {}s; the running turn has not ended. It stays queued and is delivered at the next turn boundary",
        options.timeout_secs
    ));
    Ok(pending)
}

fn report(result: &Value, json: bool) -> i32 {
    let failed = result["state"] == "failed";
    if json {
        println!("{result}");
    } else {
        let state = result["state"].as_str().unwrap_or("?");
        let seq = result["seq"].as_u64().unwrap_or(0);
        let mut line = format!("[clud] injection seq {seq}: {state}");
        if let Some(turn) = result["turn_id"].as_str() {
            line.push_str(&format!(" (turn {turn})"));
        }
        if result["duplicate"] == true {
            line.push_str(" [duplicate client_msg_id; not delivered again]");
        }
        if let Some(error) = result["error"].as_str() {
            line.push_str(&format!(": {error}"));
        }
        if let Some(note) = result["note"].as_str() {
            line.push_str(&format!(" -- {note}"));
        }
        println!("{line}");
    }
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> InjectOptions {
        InjectOptions {
            session: "sess-1".to_string(),
            message: "do X".to_string(),
            interrupt: true,
            client_msg_id: None,
            from: Some("op".to_string()),
            ticket: Some("T-2".to_string()),
            detach: true,
            timeout_secs: 5,
            json: true,
        }
    }

    #[test]
    fn retries_of_the_same_cli_call_share_a_client_msg_id() {
        let a = request_body(&options());
        let b = request_body(&options());
        assert_eq!(a["client_msg_id"], b["client_msg_id"]);
        let mut other = options();
        other.message = "do Y".to_string();
        assert_ne!(a["client_msg_id"], request_body(&other)["client_msg_id"]);
    }

    #[test]
    fn body_carries_interrupt_from_and_ticket() {
        let body = request_body(&options());
        assert_eq!(body["interrupt"], true);
        assert_eq!(body["from"], "op");
        assert_eq!(body["ticket"], "T-2");
        assert_eq!(body["message"], "do X");
    }
}
