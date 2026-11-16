//! Stub replies to a child's terminal queries when no real terminal can
//! answer them (#1310, #1347, #1366, #1702).
//!
//! ConPTY sends `ESC[6n` at startup and holds the child until a terminal
//! replies, and a child TUI may block on its own capability probes. Two
//! places answer for a missing terminal: the local PTY pump when stdin is not
//! an interactive console, and the daemon worker while no client is attached.
//! Both use [`TerminalQueryScanner`], so they answer the same set and both
//! survive a query split across two PTY reads. Before #1702 the pump matched
//! within one chunk only and the worker answered only `ESC[6n`.

/// Each query and the reply a fresh terminal would give. A pattern must match
/// exactly at an ESC, so replies such as `ESC[?1;2c` are never mistaken for
/// queries. No pattern is a proper prefix of another.
const QUERY_REPLIES: &[(&[u8], &[u8])] = &[
    // Cursor position: row 1, column 1, as a fresh terminal reports.
    (b"\x1b[6n", b"\x1b[1;1R"),
    // DA1 / DA2.
    (b"\x1b[c", b"\x1b[?1;2c"),
    (b"\x1b[0c", b"\x1b[?1;2c"),
    (b"\x1b[>c", b"\x1b[>0;0;0c"),
    (b"\x1b[>0c", b"\x1b[>0;0;0c"),
    // Kitty keyboard protocol flags.
    (b"\x1b[?u", b"\x1b[?0u"),
    // OSC 10 / 11 foreground and background colour, BEL- or ST-terminated.
    (b"\x1b]10;?\x07", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
    (b"\x1b]10;?\x1b\\", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
    (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
    (b"\x1b]11;?\x1b\\", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
];

/// Finds queries in a child's output stream, including one split across two
/// reads, and builds the replies. One scanner per output stream.
#[derive(Debug, Default)]
pub(crate) struct TerminalQueryScanner {
    /// The previous chunk's tail when it was a proper prefix of a query.
    carry: Vec<u8>,
}

impl TerminalQueryScanner {
    /// The replies for every query `chunk` completes, in stream order. Each
    /// query is answered exactly once, however the stream was split.
    pub(crate) fn replies(&mut self, chunk: &[u8]) -> Vec<u8> {
        let carried = self.carry.len();
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let mut replies = Vec::new();
        for (start, &byte) in buf.iter().enumerate() {
            if byte != 0x1b {
                continue;
            }
            let rest = &buf[start..];
            if let Some((query, reply)) = QUERY_REPLIES.iter().find(|(q, _)| rest.starts_with(q)) {
                // A match wholly inside the carried bytes was answered with
                // the chunk that held it.
                if start + query.len() > carried {
                    replies.extend_from_slice(reply);
                }
            }
        }
        self.carry = open_query_tail(&buf).to_vec();
        replies
    }
}

/// The longest tail of `buf` that starts at an ESC and is a proper prefix of
/// some query: the part a following chunk may still complete.
fn open_query_tail(buf: &[u8]) -> &[u8] {
    let longest = QUERY_REPLIES
        .iter()
        .map(|(q, _)| q.len())
        .max()
        .unwrap_or(0);
    let from = buf.len().saturating_sub(longest - 1);
    (from..buf.len())
        .filter(|&start| buf[start] == 0x1b)
        .map(|start| &buf[start..])
        .find(|tail| {
            QUERY_REPLIES
                .iter()
                .any(|(q, _)| q.len() > tail.len() && q.starts_with(tail))
        })
        .unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replies(chunk: &[u8]) -> Vec<u8> {
        TerminalQueryScanner::default().replies(chunk)
    }

    #[test]
    fn every_query_gets_its_stub_reply() {
        assert_eq!(replies(b"\x1b[6n"), b"\x1b[1;1R");
        assert_eq!(replies(b"\x1b[c"), b"\x1b[?1;2c");
        assert_eq!(replies(b"\x1b[0c"), b"\x1b[?1;2c");
        assert_eq!(replies(b"\x1b[>c"), b"\x1b[>0;0;0c");
        assert_eq!(replies(b"\x1b[>0c"), b"\x1b[>0;0;0c");
        assert_eq!(replies(b"\x1b[?u"), b"\x1b[?0u");
        assert_eq!(
            replies(b"\x1b]10;?\x07"),
            b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"
        );
        assert_eq!(
            replies(b"\x1b]11;?\x1b\\"),
            b"\x1b]11;rgb:0000/0000/0000\x1b\\"
        );
        assert_eq!(
            replies(b"\x1b[?9001h\x1b[6n\x1b[c\x1b[?u"),
            b"\x1b[1;1R\x1b[?1;2c\x1b[?0u"
        );
    }

    #[test]
    fn replies_and_other_sequences_are_not_queries() {
        for not_a_query in [
            &b"\x1b[?1;2c"[..],
            b"\x1b[?0u",
            b"\x1b[1;1R",
            b"\x1b[?1004h",
            b"",
            b"\x1b[",
            b"\x1b[6",
        ] {
            assert!(replies(not_a_query).is_empty(), "{not_a_query:?}");
        }
    }

    /// #1702: every query split at every byte offset across two reads is
    /// answered exactly once.
    #[test]
    fn a_query_split_at_every_offset_is_answered_exactly_once() {
        for (query, reply) in QUERY_REPLIES {
            for split in 1..query.len() {
                let mut scanner = TerminalQueryScanner::default();
                let mut stream = b"out".to_vec();
                stream.extend_from_slice(&query[..split]);
                let mut got = scanner.replies(&stream);
                let mut rest = query[split..].to_vec();
                rest.extend_from_slice(b"more");
                got.extend(scanner.replies(&rest));
                got.extend(scanner.replies(b"tail"));
                assert_eq!(&got, reply, "{query:?} split at {split}");
            }
        }
    }

    #[test]
    fn a_query_spread_one_byte_per_read_is_answered_once() {
        let mut scanner = TerminalQueryScanner::default();
        let got: Vec<u8> = b"\x1b]10;?\x1b\\"
            .iter()
            .flat_map(|byte| scanner.replies(std::slice::from_ref(byte)))
            .collect();
        assert_eq!(got, b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
    }

    #[test]
    fn repeated_queries_are_each_answered_and_never_twice() {
        let mut scanner = TerminalQueryScanner::default();
        assert_eq!(
            scanner.replies(b"\x1b[6n\x1b[5n\x1b[6n"),
            b"\x1b[1;1R\x1b[1;1R"
        );
        assert!(scanner.replies(b"x").is_empty());
    }

    #[test]
    fn the_carry_is_dropped_when_the_next_read_breaks_the_prefix() {
        let mut scanner = TerminalQueryScanner::default();
        assert!(scanner.replies(b"\x1b[").is_empty());
        assert!(scanner.replies(b"1;1R").is_empty());
        assert!(scanner.carry.is_empty());
    }
}
