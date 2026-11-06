//! A hook payload that reaches normal `clud` must never launch a harness.
//!
//! Harness hooks run `clud-cmd-scan` and [`crate::multicall`] dispatches on
//! argv\[0\]. A launcher script in front of the alias loses that name, so the
//! hook runs as plain `clud` with its JSON on piped stdin. As a prompt, that
//! JSON starts a harness whose own tool calls fire the same hook: a fork bomb
//! that stops only at the session cap. See
//! `docs/architecture/shim-dispatch.md`, "A hook that lost its alias".

use std::io::{ErrorKind, Read};

use serde_json::Value;

const CHUNK_BYTES: usize = 64 * 1024;

/// What piped stdin turned out to hold.
#[derive(Debug, PartialEq, Eq)]
pub enum PipedStdin {
    HookPayload(String),
    Prompt(String),
    /// A read error or non-UTF-8 input, which has never been a prompt.
    Unreadable,
}

/// Whether `text` is one JSON object with a string `hook_event_name` at the
/// top level. A prompt that merely mentions the key is not.
pub fn is_hook_payload(text: &str) -> bool {
    let text = text.trim();
    text.starts_with('{')
        && text.ends_with('}')
        && serde_json::from_str::<Value>(text)
            .is_ok_and(|value| value.get("hook_event_name").is_some_and(Value::is_string))
}

/// Read piped stdin to EOF, or only until it holds a complete hook payload: a
/// hook writer may keep the pipe open after sending one (DD-057), and waiting
/// for its EOF would wedge the tool call.
pub fn read_piped_stdin(input: &mut impl Read) -> PipedStdin {
    let mut bytes = Vec::new();
    let mut chunk = vec![0u8; CHUNK_BYTES];
    loop {
        match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(text) = complete_payload(&bytes) {
                    return PipedStdin::HookPayload(text.to_string());
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return PipedStdin::Unreadable,
        }
    }
    String::from_utf8(bytes).map_or(PipedStdin::Unreadable, PipedStdin::Prompt)
}

fn complete_payload(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes)
        .ok()
        .filter(|text| is_hook_payload(text))
}

/// Give the payload the verdict its hook wanted, and say what broke.
pub fn run_scanner<I: IntoIterator<Item = String>>(payload: String, args: I) -> i32 {
    eprintln!(
        "[clud] a hook payload reached plain `clud`, so its clud-cmd-scan alias lost its name; \
         scanned it without launching. Run `clud doctor`."
    );
    crate::block_bad_cmd::run_for_payload(payload, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_top_level_string_key_in_an_object_is_a_payload() {
        for text in [
            r#"{"hook_event_name":"PreToolUse","session_id":"s"}"#,
            " {\"hook_event_name\":\"Stop\"}\n",
        ] {
            assert!(is_hook_payload(text), "{text}");
        }
        for text in [
            "",
            "   ",
            "explain the hook_event_name field",
            r#"what does {"hook_event_name":"PreToolUse"} mean?"#,
            r#"[{"hook_event_name":"PreToolUse"}]"#,
            r#"{"hook_event_name":"PreToolUse""#,
            r#"{"hook_event_name":7}"#,
            r#"{"hook_event_name":null}"#,
            r#"{"nested":{"hook_event_name":"PreToolUse"}}"#,
            r#""hook_event_name""#,
        ] {
            assert!(!is_hook_payload(text), "{text}");
        }
    }

    /// Hands out its chunks, then fails the test if read again: a stand-in
    /// for a writer that holds the pipe open.
    struct OpenPipe(Vec<&'static [u8]>);

    impl Read for OpenPipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            assert!(!self.0.is_empty(), "read past a complete payload");
            let chunk = self.0.remove(0);
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn a_complete_payload_returns_without_waiting_for_eof() {
        let mut pipe = OpenPipe(vec![br#"{"hook_event_"#, br#"name":"PreToolUse"}"#]);
        assert_eq!(
            read_piped_stdin(&mut pipe),
            PipedStdin::HookPayload(r#"{"hook_event_name":"PreToolUse"}"#.to_string())
        );
    }

    #[test]
    fn other_input_is_read_to_eof_unchanged() {
        assert_eq!(
            read_piped_stdin(&mut &b"  mention hook_event_name\n"[..]),
            PipedStdin::Prompt("  mention hook_event_name\n".to_string())
        );
        assert_eq!(
            read_piped_stdin(&mut &b""[..]),
            PipedStdin::Prompt(String::new())
        );
        assert_eq!(
            read_piped_stdin(&mut &[0xff, 0xfe][..]),
            PipedStdin::Unreadable
        );
    }
}
