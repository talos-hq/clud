//! Agent message bulletin board: `/v1/board/messages`.
//!
//! clud, Hermes and other agents post short messages addressed to an agent
//! (by its `CLUD_SESSION_ID`) or to `all`. An agent connected through the
//! `clud` MCP bridge reads its mail with a cursor, so each read returns only
//! what arrived since the last one. The board lives in daemon memory and is
//! bounded: a daemon restart forgets it, and the oldest messages fall off once
//! `MAX_MESSAGES` is reached.

use std::collections::VecDeque;
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
