//! `clud session new`: the CLI entry point that creates a durable API
//! session. It declares `origin: cli` in the create body, so the stored record
//! says the CLI made it (docs/architecture/session-origin.md).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::session_origin::SessionOrigin;

/// What the CLI asks the daemon to create.
#[derive(Debug, Clone)]
pub struct NewSessionRequest {
    pub backend: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub model: Option<String>,
    pub safe: bool,
}

/// The `POST /v1/sessions` body. The CLI always declares `origin: cli`.
pub fn create_body(request: &NewSessionRequest) -> Value {
    let mut body = json!({
        "backend": request.backend,
        "cwd": request.cwd,
        "safe": request.safe,
        "origin": SessionOrigin::Cli,
    });
    if let Some(name) = &request.name {
        body["name"] = json!(name);
    }
    if let Some(model) = &request.model {
        body["model"] = json!(model);
    }
    body
}

/// POST the create request; returns the created session record.
pub fn create(port: u16, token: &str, request: &NewSessionRequest) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{port}/v1/sessions");
    let post = super::inject_client::agent()
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"));
    let (status, value) = super::inject_client::call(post, Some(&create_body(request)))?;
    if status == 201 {
        Ok(value)
    } else {
        Err(format!("create session failed ({status}): {value}"))
    }
}

/// Run `clud session new`; returns the process exit code.
pub fn run(state_dir: &Path, request: &NewSessionRequest, json: bool) -> i32 {
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
    match create(port, &token, request) {
        Ok(record) if json => {
            println!("{record}");
            0
        }
        Ok(record) => {
            println!("{}", record["id"].as_str().unwrap_or("-"));
            0
        }
        Err(message) => {
            eprintln!("[clud] {message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_body_declares_cli_origin() {
        let body = create_body(&NewSessionRequest {
            backend: "claude".to_string(),
            cwd: PathBuf::from("/"),
            name: Some("n".to_string()),
            model: None,
            safe: false,
        });
        assert_eq!(body["origin"], "cli");
        assert_eq!(body["name"], "n");
        assert!(body.get("model").is_none());
    }
}
