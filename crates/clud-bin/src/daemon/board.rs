//! Agent message bulletin board: `/v1/board/messages`.
//!
//! clud, Hermes and other agents post short messages addressed to an agent
//! (by its `CLUD_SESSION_ID`) or to `all`. An agent connected through the
//! `clud` MCP bridge reads its mail with a cursor, so each read returns only
//! what arrived since the last one. The board lives in daemon memory and is
//! bounded: a daemon restart forgets it, and the oldest messages fall off once
//! `MAX_MESSAGES` is reached.
//!
//! Alongside the messages the board keeps an **agent directory**
//! (`/v1/board/agents`): one card per agent with its id, the id of the agent
//! that spawned it, its working directory and a free-text "what I'm working on
//! now" slot. The daemon writes a card when `session_start` creates a
//! sub-agent session; agents (main or sub) upsert their own card through the
//! bridge's `board_status` tool.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tiny_http::{Method, Request};

use super::http::http_response::{read_body, respond_json};

/// Recipient that every agent's read includes.
pub(crate) const BROADCAST: &str = "all";
const MAX_MESSAGES: usize = 1024;
const MAX_BODY_CHARS: usize = 16 * 1024;
const MAX_NAME_CHARS: usize = 256;
const MAX_PAGE: usize = 128;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct BoardMessage {
    pub cursor: u64,
    pub at_ms: u64,
    pub from: String,
    pub to: String,
    pub body: String,
}

#[derive(Debug, Deserialize)]
struct PostDto {
    to: String,
    #[serde(default)]
    from: String,
    body: String,
}

#[derive(Debug, Serialize)]
struct PageDto {
    messages: Vec<BoardMessage>,
    next_cursor: u64,
}

/// Most agent cards kept; the least recently updated card is dropped first.
const MAX_AGENTS: usize = 512;
const MAX_STATUS_CHARS: usize = 2048;
const MAX_CWD_CHARS: usize = 4096;

/// One agent's entry in the board's directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub(crate) struct AgentCard {
    pub id: String,
    /// Board id of the agent that spawned this one; empty for a root agent.
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub cwd: String,
    /// `claude`, `codex`, a Claude Code subagent type, ...; informational.
    #[serde(default)]
    pub kind: String,
    /// GitHub `owner/repo` of the agent's checkout, when it has one.
    #[serde(default)]
    pub repo: String,
    /// Branch checked out in the agent's cwd.
    #[serde(default)]
    pub branch: String,
    /// The "what I'm working on now" slot.
    #[serde(default)]
    pub working_on: String,
    #[serde(default)]
    pub updated_ms: u64,
}

/// A partial card: absent fields keep their stored value.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct AgentUpdate {
    pub id: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub working_on: Option<String>,
}

#[derive(Default)]
pub(crate) struct Agents {
    cards: BTreeMap<String, AgentCard>,
}

fn too_long(value: Option<&String>, max: usize) -> bool {
    value.is_some_and(|v| v.chars().count() > max)
}

impl Agents {
    /// Create or merge a card; returns the stored result.
    pub(crate) fn upsert(
        &mut self,
        update: AgentUpdate,
        at_ms: u64,
    ) -> Result<AgentCard, &'static str> {
        let id = update.id.trim();
        if id.is_empty() || id.chars().count() > MAX_NAME_CHARS || id == BROADCAST {
            return Err("`id` must be a non-empty agent id other than \"all\"");
        }
        if too_long(update.parent.as_ref(), MAX_NAME_CHARS)
            || too_long(update.kind.as_ref(), MAX_NAME_CHARS)
            || too_long(update.repo.as_ref(), MAX_NAME_CHARS)
            || too_long(update.branch.as_ref(), MAX_NAME_CHARS)
            || too_long(update.cwd.as_ref(), MAX_CWD_CHARS)
            || too_long(update.working_on.as_ref(), MAX_STATUS_CHARS)
        {
            return Err("a card field is too long");
        }
        if !self.cards.contains_key(id) && self.cards.len() >= MAX_AGENTS {
            if let Some(stalest) = self
                .cards
                .values()
                .min_by_key(|card| card.updated_ms)
                .map(|card| card.id.clone())
            {
                self.cards.remove(&stalest);
            }
        }
        let card = self
            .cards
            .entry(id.to_string())
            .or_insert_with(|| AgentCard {
                id: id.to_string(),
                ..AgentCard::default()
            });
        let fields = [
            (&mut card.parent, update.parent),
            (&mut card.cwd, update.cwd),
            (&mut card.kind, update.kind),
            (&mut card.repo, update.repo),
            (&mut card.branch, update.branch),
            (&mut card.working_on, update.working_on),
        ];
        for (slot, value) in fields {
            if let Some(value) = value {
                *slot = value.trim().to_string();
            }
        }
        card.updated_ms = at_ms;
        Ok(card.clone())
    }

    pub(crate) fn get(&self, id: &str) -> Option<AgentCard> {
        self.cards.get(id).cloned()
    }

    /// Every card, or only the direct children of `parent` when it is set.
    pub(crate) fn list(&self, parent: Option<&str>) -> Vec<AgentCard> {
        self.cards
            .values()
            .filter(|card| parent.is_none_or(|p| card.parent == p))
            .cloned()
            .collect()
    }
}

fn agents() -> &'static Mutex<Agents> {
    static AGENTS: OnceLock<Mutex<Agents>> = OnceLock::new();
    AGENTS.get_or_init(|| Mutex::new(Agents::default()))
}

/// Record (or merge) an agent's card. Used by the daemon when it creates a
/// sub-agent session, so the card exists before the agent's first turn.
pub(crate) fn register_agent(update: AgentUpdate) -> Result<AgentCard, &'static str> {
    agents()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .upsert(update, now_ms())
}

/// The stored card for `id`, if any.
pub(crate) fn agent_card(id: &str) -> Option<AgentCard> {
    agents()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(id)
}

#[derive(Default)]
pub(crate) struct Board {
    messages: VecDeque<BoardMessage>,
    last_cursor: u64,
}

impl Board {
    /// Append a message and return it with its assigned cursor.
    pub(crate) fn post(
        &mut self,
        from: &str,
        to: &str,
        body: &str,
        at_ms: u64,
    ) -> Result<BoardMessage, &'static str> {
        let to = to.trim();
        if to.is_empty() || to.chars().count() > MAX_NAME_CHARS {
            return Err("`to` must be a non-empty session id or \"all\"");
        }
        if from.chars().count() > MAX_NAME_CHARS {
            return Err("`from` is too long");
        }
        if body.trim().is_empty() || body.chars().count() > MAX_BODY_CHARS {
            return Err("`body` must be non-empty and at most 16384 characters");
        }
        self.last_cursor += 1;
        let message = BoardMessage {
            cursor: self.last_cursor,
            at_ms,
            from: from.trim().to_string(),
            to: to.to_string(),
            body: body.to_string(),
        };
        if self.messages.len() == MAX_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back(message.clone());
        Ok(message)
    }

    /// Messages after `after` addressed to `to` (plus broadcasts), oldest
    /// first, and the cursor to pass next time. An empty `to` reads them all.
    pub(crate) fn read(&self, to: &str, after: u64, limit: usize) -> (Vec<BoardMessage>, u64) {
        let limit = limit.clamp(1, MAX_PAGE);
        let page: Vec<BoardMessage> = self
            .messages
            .iter()
            .filter(|m| m.cursor > after)
            .filter(|m| to.is_empty() || m.to == to || m.to == BROADCAST)
            .take(limit)
            .cloned()
            .collect();
        // A short page means every newer message was examined, so the caller
        // can skip past messages addressed to someone else.
        let next = match page.last() {
            Some(last) if page.len() == limit => last.cursor,
            _ => after.max(self.last_cursor),
        };
        (page, next)
    }
}

fn board() -> &'static Mutex<Board> {
    static BOARD: OnceLock<Mutex<Board>> = OnceLock::new();
    BOARD.get_or_init(|| Mutex::new(Board::default()))
}

/// Best-effort system notice to one board address, never a broadcast.
pub(super) fn notify(to: &str, body: &str) {
    if to.trim() != "all" {
        let _ = board()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .post("clud", to, body, now_ms());
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn query_value<'a>(url: &'a str, name: &str) -> Option<&'a str> {
    url.split_once('?')?.1.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then_some(value)
    })
}

fn error(request: Request, status: u16, code: &str, message: &str) {
    let body = serde_json::json!({"code": code, "message": message});
    respond_json(request, status, body.to_string().as_bytes());
}

/// Route `/v1/board/messages`; hands the request back when the path is not
/// the board's.
pub(super) fn handle(
    mut request: Request,
    method: &Method,
    path: &str,
    url: &str,
) -> Option<Request> {
    if path == "/v1/board/agents" || path.starts_with("/v1/board/agents/") {
        handle_agents(request, method, path, url);
        return None;
    }
    if path != "/v1/board/messages" {
        return Some(request);
    }
    match method {
        Method::Post => {
            let Some(dto) = read_body(&mut request)
                .ok()
                .and_then(|body| serde_json::from_slice::<PostDto>(&body).ok())
            else {
                error(
                    request,
                    400,
                    "invalid_request",
                    "expected {to, body, from?}",
                );
                return None;
            };
            let posted = board()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .post(&dto.from, &dto.to, &dto.body, now_ms());
            match posted {
                Ok(message) => respond_json(
                    request,
                    201,
                    &serde_json::to_vec(&message).unwrap_or_else(|_| b"{}".to_vec()),
                ),
                Err(message) => error(request, 400, "invalid_request", message),
            }
        }
        Method::Get => {
            let to = query_value(url, "to")
                .map(percent_decode)
                .unwrap_or_default();
            let after = query_value(url, "after")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let limit = query_value(url, "limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_PAGE);
            let (messages, next_cursor) = board()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read(&to, after, limit);
            let page = PageDto {
                messages,
                next_cursor,
            };
            respond_json(
                request,
                200,
                &serde_json::to_vec(&page).unwrap_or_else(|_| b"{}".to_vec()),
            );
        }
        _ => error(request, 405, "method_not_allowed", "use GET or POST"),
    }
    None
}

fn respond_card(request: Request, status: u16, card: &AgentCard) {
    respond_json(
        request,
        status,
        &serde_json::to_vec(card).unwrap_or_else(|_| b"{}".to_vec()),
    );
}

/// `/v1/board/agents`: `GET` lists cards (`?parent=<id>` narrows to one
/// agent's children), `POST` upserts one; `GET /v1/board/agents/<id>` reads one.
fn handle_agents(mut request: Request, method: &Method, path: &str, url: &str) {
    let one = path
        .strip_prefix("/v1/board/agents/")
        .map(percent_decode)
        .filter(|id| !id.is_empty());
    match (method, one) {
        (Method::Get, Some(id)) => match agent_card(&id) {
            Some(card) => respond_card(request, 200, &card),
            None => error(request, 404, "not_found", "no such agent"),
        },
        (Method::Get, None) => {
            let parent = query_value(url, "parent").map(percent_decode);
            let cards = agents()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .list(parent.as_deref());
            let body = serde_json::json!({ "agents": cards });
            respond_json(request, 200, body.to_string().as_bytes());
        }
        (Method::Post, None) => {
            let Some(update) = read_body(&mut request)
                .ok()
                .and_then(|body| serde_json::from_slice::<AgentUpdate>(&body).ok())
            else {
                error(
                    request,
                    400,
                    "invalid_request",
                    "expected {id, parent?, cwd?, kind?, repo?, branch?, working_on?}",
                );
                return;
            };
            match register_agent(update) {
                Ok(card) => respond_card(request, 200, &card),
                Err(message) => error(request, 400, "invalid_request", message),
            }
        }
        _ => error(request, 405, "method_not_allowed", "use GET or POST"),
    }
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(id: &str) -> AgentUpdate {
        AgentUpdate {
            id: id.to_string(),
            ..AgentUpdate::default()
        }
    }

    #[test]
    fn agent_cards_merge_partial_updates() {
        let mut agents = Agents::default();
        agents
            .upsert(
                AgentUpdate {
                    parent: Some("main".into()),
                    cwd: Some("/repo".into()),
                    kind: Some("codex".into()),
                    repo: Some("zackees/clud".into()),
                    branch: Some("feat/x".into()),
                    ..update("sub-1")
                },
                1,
            )
            .unwrap();
        let card = agents
            .upsert(
                AgentUpdate {
                    working_on: Some("fixing the parser".into()),
                    ..update("sub-1")
                },
                2,
            )
            .unwrap();
        assert_eq!(card.parent, "main");
        assert_eq!(card.cwd, "/repo");
        assert_eq!(card.working_on, "fixing the parser");
        assert_eq!(card.repo, "zackees/clud");
        assert_eq!(card.branch, "feat/x");
        assert_eq!(card.updated_ms, 2);
        agents.upsert(update("other"), 3).unwrap();
        let children: Vec<_> = agents
            .list(Some("main"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(children, ["sub-1"]);
        assert_eq!(agents.list(None).len(), 2);
    }

    #[test]
    fn agent_cards_reject_bad_ids_and_evict_the_stalest() {
        let mut agents = Agents::default();
        assert!(agents.upsert(update(" "), 0).is_err());
        assert!(agents.upsert(update(BROADCAST), 0).is_err());
        for i in 0..MAX_AGENTS {
            agents
                .upsert(update(&format!("a{i}")), i as u64 + 10)
                .unwrap();
        }
        agents.upsert(update("newest"), 10_000).unwrap();
        assert_eq!(agents.list(None).len(), MAX_AGENTS);
        assert!(agents.get("a0").is_none());
        assert!(agents.get("newest").is_some());
    }

    #[test]
    fn read_returns_own_and_broadcast_messages_after_the_cursor() {
        let mut board = Board::default();
        board.post("hermes", "a", "for a", 1).unwrap();
        board.post("hermes", "b", "for b", 2).unwrap();
        board.post("clud", BROADCAST, "everyone", 3).unwrap();
        let (page, next) = board.read("a", 0, 10);
        let bodies: Vec<_> = page.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, ["for a", "everyone"]);
        assert_eq!(next, 3);
        let (page, next) = board.read("a", next, 10);
        assert!(page.is_empty());
        assert_eq!(next, 3);
    }

    #[test]
    fn a_full_page_does_not_skip_unread_messages() {
        let mut board = Board::default();
        for i in 0..3 {
            board.post("x", "a", &format!("m{i}"), 0).unwrap();
        }
        board.post("x", "b", "other", 0).unwrap();
        let (page, next) = board.read("a", 0, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(next, 2);
        let (page, next) = board.read("a", next, 2);
        assert_eq!(
            page.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
            ["m2"]
        );
        assert_eq!(next, 4);
    }

    #[test]
    fn invalid_posts_are_rejected() {
        let mut board = Board::default();
        assert!(board.post("x", " ", "hi", 0).is_err());
        assert!(board.post("x", "a", "  ", 0).is_err());
        assert!(board
            .post("x", "a", &"y".repeat(MAX_BODY_CHARS + 1), 0)
            .is_err());
    }

    #[test]
    fn the_board_is_bounded() {
        let mut board = Board::default();
        for _ in 0..MAX_MESSAGES + 5 {
            board.post("x", "a", "m", 0).unwrap();
        }
        assert_eq!(board.messages.len(), MAX_MESSAGES);
        assert_eq!(board.messages.front().unwrap().cursor, 6);
    }

    #[test]
    fn query_values_decode() {
        let url = "/v1/board/messages?to=sess%2D1&after=4";
        assert_eq!(
            query_value(url, "to").map(percent_decode).unwrap(),
            "sess-1"
        );
        assert_eq!(query_value(url, "after"), Some("4"));
        assert_eq!(query_value(url, "limit"), None);
    }
}
