//! `MOCK_AGENT_BOARD=1`: act as a bulletin-board sub-agent for one turn.
//!
//! The scripted half of clud's a2a demo (docs/architecture/agent-board.md):
//! the mock reads its board id (`CLUD_BOARD_ID`) and parent
//! (`CLUD_BOARD_PARENT`) from the environment the daemon gave the turn, finds
//! the daemon API through `$CLUD_DAEMON_STATE_DIR/daemon.json` (what `clud daemon
//! api-info` reads), fills its "working on" slot, tells its parent it is ready,
//! waits for one instruction from the parent, and acknowledges it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const WAIT_FOR_INSTRUCTION: Duration = Duration::from_secs(60);

struct Api {
    port: u16,
    token: String,
}

impl Api {
    fn discover() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os("CLUD_DAEMON_STATE_DIR")?);
        let info: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("daemon.json")).ok()?).ok()?;
        Some(Self {
            port: u16::try_from(info["dashboard_port"].as_u64()?).ok()?,
            token: info["api_token"].as_str()?.to_string(),
        })
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Option<serde_json::Value> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let body = body.map(ToString::to_string).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.token,
            body.len(),
            port = self.port,
        );
        stream.write_all(request.as_bytes()).ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        let (_, payload) = response.split_once("\r\n\r\n")?;
        serde_json::from_str(payload).ok()
    }

    fn status(&self, id: &str, working_on: &str) {
        let card = serde_json::json!({"id": id, "working_on": working_on});
        let _ = self.call("POST", "/v1/board/agents", Some(&card));
    }

    fn post(&self, from: &str, to: &str, body: &str) {
        let message = serde_json::json!({"from": from, "to": to, "body": body});
        let _ = self.call("POST", "/v1/board/messages", Some(&message));
    }

    /// The first message to `id` from `parent`, waiting up to `budget`.
    fn instruction(&self, id: &str, parent: &str, budget: Duration) -> Option<String> {
        let started = Instant::now();
        let mut cursor = 0_u64;
        while started.elapsed() < budget {
            let page = self.call(
                "GET",
                &format!("/v1/board/messages?to={id}&after={cursor}"),
                None,
            )?;
            for message in page["messages"].as_array().into_iter().flatten() {
                if message["to"] == id && message["from"] == parent {
                    return message["body"].as_str().map(str::to_string);
                }
            }
            cursor = page["next_cursor"].as_u64().unwrap_or(cursor);
            std::thread::sleep(Duration::from_millis(200));
        }
        None
    }
}

/// Run the board script when `MOCK_AGENT_BOARD=1`; prints one summary line.
pub fn run_if_enabled() {
    if std::env::var("MOCK_AGENT_BOARD").ok().as_deref() != Some("1") {
        return;
    }
    let Some(id) = std::env::var("CLUD_BOARD_ID")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        return;
    };
    let parent = std::env::var("CLUD_BOARD_PARENT").unwrap_or_default();
    let Some(api) = Api::discover() else {
        println!("{}", serde_json::json!({"mock_board": "no daemon api"}));
        return;
    };
    // The daemon pre-filed this agent's card; read it back to prove it.
    let card = api
        .call("GET", &format!("/v1/board/agents/{id}"), None)
        .unwrap_or_default();
    api.status(&id, "mock: waiting for instructions");
    api.post(
        &id,
        &parent,
        &format!("ready: {id} in {}", card["cwd"].as_str().unwrap_or("?")),
    );
    let instruction = api.instruction(&id, &parent, WAIT_FOR_INSTRUCTION);
    match &instruction {
        Some(body) => {
            api.status(&id, &format!("mock: done with {body}"));
            api.post(&id, &parent, &format!("ack: {body}"));
        }
        None => api.status(&id, "mock: gave up waiting"),
    }
    println!(
        "{}",
        serde_json::json!({"mock_board": {
            "id": id,
            "parent": parent,
            "state_dir": std::env::var("CLUD_DAEMON_STATE_DIR").ok(),
            "port": api.port,
            "card": card,
            "instruction": instruction,
        }})
    );
}
