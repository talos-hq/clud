//! Codex behind the OpenAI-shaped doors (`/v1/chat/completions`,
//! `/v1/responses`, `/v1/completions`, Gemini).
//!
//! The Codex pipeline writes Anthropic Messages frames straight to a socket,
//! while every other route hands the gateway an upstream body it can translate.
//! Rather than teach the pipeline a second output protocol, the turn is served
//! into a loopback socket pair, the captured Messages response is read back,
//! and it goes through the same `relay_translated_chat` translator every other
//! route uses. That translator buffers the whole body anyway, so capturing it
//! first costs the client nothing it was not already paying.

use super::*;

/// One captured Messages response: status, content type and decoded body.
struct Captured {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

/// Serve one OpenAI-shaped turn on the Codex route.
pub(super) fn serve_codex_translated(
    stream: &mut TcpStream,
    turn: &UnifiedTurn<'_>,
    payload: &[u8],
    chat: ChatTranslation<'_>,
) -> ProxyOutcome {
    let idle_timeout = turn.config.stream_idle_timeout;
    let captured = match capture_messages(turn, payload) {
        Ok(captured) => captured,
        Err(error) => {
            let message = format!("the Codex route produced no readable answer: {error}");
            return write_client_error(stream, chat.dialect, 502, &message, idle_timeout);
        }
    };
    let head = ProxyResponseHead {
        status: captured.status,
        content_type: &captured.content_type,
        retry_after: None,
        request_id: None,
    };
    // The Codex pipeline records its own usage inside `serve_messages`; this
    // accumulator only satisfies the translator's signature and is dropped.
    let mut usage = StreamingUsage::new(crate::anthropic_usage::Provider::Claude);
    let mut body = io::Cursor::new(captured.body);
    // Secret restoration already ran inside `serve_messages`.
    relay_translated_chat(
        stream,
        chat,
        &head,
        Vec::new(),
        &mut body,
        None,
        idle_timeout,
        turn.shutdown,
        &mut usage,
    )
}

/// Run the Codex Messages turn into a loopback socket and read its response.
fn capture_messages(turn: &UnifiedTurn<'_>, payload: &[u8]) -> io::Result<Captured> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let mut reader = TcpStream::connect(listener.local_addr()?)?;
    let (mut writer, _) = listener.accept()?;
    drop(listener);
    // Nothing is ever sent to the server end, so its post-response drain sees
    // EOF at once instead of waiting out its timeout.
    reader.shutdown(Shutdown::Write)?;
    let (config, shutdown, conversations) = (turn.config, turn.shutdown, turn.conversations);
    let (streaming, conversation_key, route) =
        (turn.streaming, turn.conversation_key, turn.selected_route);
    let (log, cache_health, redaction) = (turn.log, turn.cache_health, turn.redaction);
    let mut raw = Vec::new();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            serve_messages(
                &mut writer,
                config,
                shutdown,
                conversations,
                payload,
                streaming,
                conversation_key,
                Some(route),
                log,
                cache_health,
                redaction,
            );
        });
        reader.read_to_end(&mut raw)
    })?;
    parse_captured(&raw)
}

/// Split one HTTP/1.1 response into status, content type and decoded body.
fn parse_captured(raw: &[u8]) -> io::Result<Captured> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| invalid("response head is incomplete"))?;
    let head =
        std::str::from_utf8(&raw[..split]).map_err(|_| invalid("response head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| invalid("response status line is malformed"))?;
    let mut content_type = String::from("application/json");
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-type") {
            content_type = value.to_string();
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
        }
    }
    let rest = &raw[split + 4..];
    let body = if chunked {
        decode_chunked(rest).ok_or_else(|| invalid("chunked body is malformed"))?
    } else {
        rest.to_vec()
    };
    Ok(Captured {
        status,
        content_type,
        body,
    })
}

/// Decode an HTTP/1.1 chunked body. A stream cut before the terminating
/// zero-length chunk keeps what arrived, so the translator can still see a
/// truncated turn and answer it as one.
fn decode_chunked(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") else {
            return Some(body);
        };
        let size_text = std::str::from_utf8(&rest[..line_end]).ok()?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(body);
        }
        let take = size.min(rest.len());
        body.extend_from_slice(&rest[..take]);
        if take < size {
            return Some(body);
        }
        rest = rest.get(size + 2..).unwrap_or(&[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_length_response_is_split_into_status_type_and_body() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let captured = parse_captured(raw).unwrap();
        assert_eq!(captured.status, 200);
        assert_eq!(captured.content_type, "application/json");
        assert_eq!(captured.body, b"{}");
    }

    #[test]
    fn a_chunked_event_stream_is_decoded() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let captured = parse_captured(raw).unwrap();
        assert_eq!(captured.content_type, "text/event-stream");
        assert_eq!(captured.body, b"hello world");
    }

    #[test]
    fn a_truncated_chunked_body_keeps_what_arrived() {
        assert_eq!(decode_chunked(b"5\r\nhel").unwrap(), b"hel");
        assert_eq!(decode_chunked(b"3\r\nabc\r\n").unwrap(), b"abc");
    }

    #[test]
    fn an_error_status_is_preserved_for_the_translator() {
        let raw = b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n\r\n{\"type\":\"error\"}";
        let captured = parse_captured(raw).unwrap();
        assert_eq!(captured.status, 429);
        assert_eq!(captured.body, b"{\"type\":\"error\"}");
    }

    #[test]
    fn a_response_without_a_head_is_refused() {
        assert!(parse_captured(b"HTTP/1.1 200 OK\r\n").is_err());
        assert!(parse_captured(b"garbage\r\n\r\n").is_err());
    }
}
