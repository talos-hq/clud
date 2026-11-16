//! What of the remote shell's output the consenting user's terminal may see.
//!
//! The local view is watch-only: the operator's terminal answers queries and
//! reports input on the server side. Passed through, a query makes the local
//! terminal type its reply into the client's stdin (`ESC P >|Konsole 26.08.1`),
//! and an input-reporting mode (mouse, focus, bracketed paste) makes it send
//! reports there for every pointer move, which outlive the session as junk in
//! the user's shell. So this filter drops every sequence that asks the terminal
//! to answer, clipboard writes, and the input-reporting modes, and keeps all
//! drawing (text, colour, cursor, alternate screen) unchanged.
//!
//! The parser is streaming: a sequence split across PTY reads is held until it
//! completes, and an unterminated one is released unchanged past
//! [`MAX_SEQUENCE`] so a broken stream cannot stall the view.

/// Longest escape sequence held back while waiting for its end.
const MAX_SEQUENCE: usize = 4096;

/// DEC private modes that make the terminal report input: X10/normal/
/// highlight/button/any-event mouse, focus events, UTF-8/SGR/urxvt/SGR-pixel
/// mouse encodings, and bracketed paste.
const INPUT_MODES: &[u32] = &[
    9, 1000, 1001, 1002, 1003, 1004, 1005, 1006, 1015, 1016, 2004,
];

/// Turns every [`INPUT_MODES`] mode off, for a terminal the filter did not
/// always guard (an older client, or a session that ended mid-sequence).
pub(super) const RESET_INPUT_MODES: &[u8] = b"\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l";

#[derive(Debug, Default)]
pub(super) struct MirrorFilter {
    /// An escape sequence begun in an earlier chunk.
    held: Vec<u8>,
}

enum Scan {
    /// A whole sequence of this many bytes.
    Complete(usize),
    /// More bytes are needed.
    Incomplete,
}

impl MirrorFilter {
    /// `chunk` as the local terminal may show it.
    pub(super) fn filter(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(buf.len());
        let mut at = 0;
        while at < buf.len() {
            let Some(offset) = buf[at..].iter().position(|&b| b == 0x1b) else {
                out.extend_from_slice(&buf[at..]);
                break;
            };
            out.extend_from_slice(&buf[at..at + offset]);
            at += offset;
            match scan(&buf[at..]) {
                Scan::Complete(len) => {
                    keep(&buf[at..at + len], &mut out);
                    at += len;
                }
                Scan::Incomplete if buf.len() - at > MAX_SEQUENCE => {
                    out.push(0x1b);
                    at += 1;
                }
                Scan::Incomplete => {
                    self.held = buf[at..].to_vec();
                    break;
                }
            }
        }
        out
    }
}

/// The length of the escape sequence at the start of `seq` (which begins ESC).
fn scan(seq: &[u8]) -> Scan {
    let Some(&kind) = seq.get(1) else {
        return Scan::Incomplete;
    };
    match kind {
        b'[' => seq[2..]
            .iter()
            .position(|b| (0x40..=0x7e).contains(b))
            .map_or(Scan::Incomplete, |end| Scan::Complete(end + 3)),
        // OSC, DCS, SOS, PM, APC: a string ended by BEL (OSC) or ST.
        b']' | b'P' | b'X' | b'^' | b'_' => string_end(seq, kind == b']'),
        // Character-set and similar: ESC, intermediates, one final byte.
        _ => seq[1..]
            .iter()
            .position(|b| !(0x20..=0x2f).contains(b))
            .map_or(Scan::Incomplete, |end| Scan::Complete(end + 2)),
    }
}

fn string_end(seq: &[u8], bel_ends: bool) -> Scan {
    for index in 2..seq.len() {
        if bel_ends && seq[index] == 0x07 {
            return Scan::Complete(index + 1);
        }
        if seq[index] == 0x1b && seq.get(index + 1) == Some(&b'\\') {
            return Scan::Complete(index + 2);
        }
    }
    Scan::Incomplete
}

/// Append `seq` to `out` unless the local terminal must not act on it.
fn keep(seq: &[u8], out: &mut Vec<u8>) {
    match seq[1] {
        b'[' => keep_csi(seq, out),
        b']' if is_osc_request(seq) => {}
        b'P' if is_dcs_request(seq) => {}
        _ => out.extend_from_slice(seq),
    }
}

fn keep_csi(seq: &[u8], out: &mut Vec<u8>) {
    let body = &seq[2..seq.len() - 1];
    let fin = seq[seq.len() - 1];
    let prefix = body.first().copied().filter(|b| b"<=>?".contains(b));
    let params = if prefix.is_some() { &body[1..] } else { body };
    let first: Option<u32> = params
        .split(|&b| b == b';')
        .next()
        .and_then(|p| std::str::from_utf8(p).ok()?.parse().ok());
    let query = match (prefix, fin) {
        // Device status / cursor position reports, device attributes.
        (_, b'n' | b'c') => true,
        // XTVERSION, and xterm modifyOtherKeys (a keyboard reporting mode).
        (Some(b'>'), b'q' | b'm') => true,
        // Kitty keyboard protocol: query, push, pop and set. Local stdin
        // carries Ctrl+C and the consent answer and must keep plain keys.
        (Some(_), b'u') => true,
        // DECRQM: report whether a mode is set.
        (_, b'p') if body.contains(&b'$') => true,
        // Window reports (size, position, title): XTWINOPS 11-21.
        (None, b't') => first.is_some_and(|ps| (11..=21).contains(&ps)),
        _ => false,
    };
    if query {
        return;
    }
    if prefix == Some(b'?') && matches!(fin, b'h' | b'l') {
        let kept: Vec<&[u8]> = params
            .split(|&b| b == b';')
            .filter(|p| {
                std::str::from_utf8(p)
                    .ok()
                    .and_then(|p| p.parse::<u32>().ok())
                    .is_none_or(|mode| !INPUT_MODES.contains(&mode))
            })
            .collect();
        if kept.is_empty() {
            return;
        }
        out.extend_from_slice(b"\x1b[?");
        out.extend_from_slice(&kept.join(&b';'));
        out.push(fin);
        return;
    }
    out.extend_from_slice(seq);
}

/// OSC 52 clipboard access, or any OSC asking for a value (`;?`).
fn is_osc_request(seq: &[u8]) -> bool {
    let body = osc_body(seq);
    body.starts_with(b"52;") || body.split(|&b| b == b';').any(|part| part == b"?")
}

fn osc_body(seq: &[u8]) -> &[u8] {
    let end = if seq.ends_with(b"\x07") { 1 } else { 2 };
    &seq[2..seq.len() - end]
}

/// XTGETTCAP (`DCS + q`) and DECRQSS (`DCS $ q`) ask the terminal to answer.
fn is_dcs_request(seq: &[u8]) -> bool {
    seq[2..].starts_with(b"+q") || seq[2..].starts_with(b"$q")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = MirrorFilter::default();
        chunks.iter().flat_map(|c| filter.filter(c)).collect()
    }

    #[test]
    fn drawing_passes_through_byte_for_byte() {
        let screen: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[1;1H\x1b[38;5;196mred\x1b[0m \xe2\x98\x83\r\n\x1b]0;title\x07\x1b(B\x1b[?2026h\x1b[ q";
        assert_eq!(shown(&[screen]), screen);
    }

    #[test]
    fn queries_that_make_the_terminal_answer_are_dropped() {
        // The reported spam: XTVERSION, then DA, DSR, kitty flags, DECRQM,
        // window size, colour queries, XTGETTCAP and DECRQSS.
        let queries: &[&[u8]] = &[
            b"\x1b[>q",
            b"\x1b[>0q",
            b"\x1b[c",
            b"\x1b[>c",
            b"\x1b[=c",
            b"\x1b[6n",
            b"\x1b[?6n",
            b"\x1b[5n",
            b"\x1b[?u",
            b"\x1b[>1u",
            b"\x1b[<u",
            b"\x1b[=5;1u",
            b"\x1b[?2026$p",
            b"\x1b[14t",
            b"\x1b[18t",
            b"\x1b]10;?\x07",
            b"\x1b]11;?\x1b\\",
            b"\x1b]4;1;?\x07",
            b"\x1bP+q544e\x1b\\",
            b"\x1bP$qm\x1b\\",
        ];
        for query in queries {
            assert_eq!(shown(&[b"a", query, b"b"]), b"ab", "{query:?}");
        }
    }

    #[test]
    fn clipboard_writes_never_reach_the_local_terminal() {
        assert_eq!(shown(&[b"\x1b]52;c;aGVsbG8=\x07x"]), b"x");
    }

    #[test]
    fn input_reporting_modes_are_removed_and_display_modes_kept() {
        assert_eq!(shown(&[b"\x1b[?1000;1006h"]), b"");
        assert_eq!(shown(&[b"\x1b[?1003h\x1b[?1004h\x1b[?2004h"]), b"");
        assert_eq!(shown(&[b"\x1b[?1049;1006;25h"]), b"\x1b[?1049;25h");
        assert_eq!(shown(&[b"\x1b[?1000l\x1b[?25h"]), b"\x1b[?25h");
        assert_eq!(shown(&[b"\x1b[>4;1m\x1b[1m"]), b"\x1b[1m");
    }

    #[test]
    fn a_sequence_split_across_reads_is_held_until_complete() {
        assert_eq!(shown(&[b"a\x1b", b"[>", b"0q", b"b"]), b"ab");
        assert_eq!(shown(&[b"\x1b[?10", b"03h", b"x"]), b"x");
        assert_eq!(shown(&[b"\x1b]10;", b"?\x1b", b"\\y"]), b"y");
        assert_eq!(shown(&[b"\x1b[3", b"1mz"]), b"\x1b[31mz");
    }

    #[test]
    fn an_unterminated_string_is_released_rather_than_stalling() {
        let mut filter = MirrorFilter::default();
        assert!(filter.filter(b"\x1b]0;").is_empty());
        let long = vec![b'x'; MAX_SEQUENCE + 10];
        let out = filter.filter(&long);
        assert_eq!(out.len(), 4 + long.len());
    }

    #[test]
    fn reset_turns_every_input_mode_off() {
        for mode in INPUT_MODES {
            let off = format!("\x1b[?{mode}l");
            assert!(
                RESET_INPUT_MODES
                    .windows(off.len())
                    .any(|w| w == off.as_bytes()),
                "{mode}"
            );
        }
    }
}
