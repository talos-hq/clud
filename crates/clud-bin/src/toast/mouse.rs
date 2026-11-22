//! Click-to-dismiss: filter SGR mouse reports aimed at the toast (#1189).
//!
//! clud never turns mouse tracking on itself — doing so steals text selection
//! and wheel scrollback from inline TUIs. The filter is only *armed* when the
//! child already enabled SGR mouse reporting (Claude Code's fullscreen mode
//! does) and a toast is visible. Then a left-button press inside the close
//! button, and its matching release, are swallowed; every other byte reaches
//! the child unchanged.

use super::text_tier::CellRect;

/// Longest SGR mouse report we will hold across a chunk boundary.
const MAX_PENDING: usize = 32;

#[derive(Debug, Default)]
pub struct MouseFilter {
    pending: Vec<u8>,
    swallowing_release: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MouseResult {
    pub bytes: Vec<u8>,
    pub dismissed: bool,
    pub open_processes: bool,
    /// `Some` only when the child has enabled any-motion SGR reporting.
    pub cpu_hover: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parse {
    /// Bytes at the front are not an SGR report; this many can pass through.
    NotReport(usize),
    /// A complete report of this length.
    Report {
        len: usize,
        button: u16,
        x: u16,
        y: u16,
        press: bool,
    },
    /// A prefix of a report; need more bytes.
    Incomplete,
}

impl MouseFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while a partial report is held for its continuation; the PTY pump
    /// shortens its wait so [`Self::flush_pending`] releases it promptly.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Filter one stdin chunk. clud consumes only clicks on its own visible
    /// controls. CPU-panel hover is observed without consuming mouse reports;
    /// all other bytes reach the child unchanged.
    #[cfg(test)]
    pub fn process(
        &mut self,
        chunk: &[u8],
        close: Option<CellRect>,
        cpu: Option<CellRect>,
        hover_armed: bool,
    ) -> MouseResult {
        self.process_with_gear(chunk, close, cpu, hover_armed, None)
    }

    pub fn process_with_gear(
        &mut self,
        chunk: &[u8],
        close: Option<CellRect>,
        cpu: Option<CellRect>,
        hover_armed: bool,
        gear: Option<CellRect>,
    ) -> MouseResult {
        if close.is_none()
            && cpu.is_none()
            && gear.is_none()
            && self.pending.is_empty()
            && !self.swallowing_release
        {
            return MouseResult {
                bytes: chunk.to_vec(),
                ..MouseResult::default()
            };
        }
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(input.len());
        let mut result = MouseResult::default();
        let mut i = 0;
        while i < input.len() {
            if input[i] != 0x1b {
                out.push(input[i]);
                i += 1;
                continue;
            }
            match parse(&input[i..]) {
                Parse::NotReport(n) => {
                    out.extend_from_slice(&input[i..i + n]);
                    i += n;
                }
                Parse::Incomplete => {
                    if input.len() - i <= MAX_PENDING {
                        self.pending = input[i..].to_vec();
                    } else {
                        out.extend_from_slice(&input[i..]);
                    }
                    result.bytes = out;
                    return result;
                }
                Parse::Report {
                    len,
                    button,
                    x,
                    y,
                    press,
                } => {
                    let left_press = press && button & 0b11 == 0 && button & (32 | 64) == 0;
                    let hit_close = close.is_some_and(|rect| rect.contains_1based(x, y));
                    if hover_armed {
                        result.cpu_hover = Some(cpu.is_some_and(|rect| rect.contains_1based(x, y)));
                    }
                    if left_press && gear.is_some_and(|rect| rect.contains_1based(x, y)) {
                        result.open_processes = true;
                        self.swallowing_release = true;
                    } else if left_press && hit_close {
                        result.dismissed = true;
                        self.swallowing_release = true;
                    } else if !press && self.swallowing_release {
                        self.swallowing_release = false;
                    } else {
                        out.extend_from_slice(&input[i..i + len]);
                    }
                    i += len;
                }
            }
        }
        result.bytes = out;
        result
    }

    /// Release any held partial report (called when stdin goes idle so a lone
    /// ESC keypress is never delayed for more than one idle poll).
    pub fn flush_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

/// Parse an SGR mouse report `ESC [ < b ; x ; y (M|m)` at the start of `bytes`.
fn parse(bytes: &[u8]) -> Parse {
    const PREFIX: &[u8] = b"\x1b[<";
    let prefix_len = PREFIX.len().min(bytes.len());
    if bytes[..prefix_len] != PREFIX[..prefix_len] {
        // ESC followed by something else: forward ESC alone; the rest is
        // re-examined byte by byte.
        return Parse::NotReport(1);
    }
    if bytes.len() < PREFIX.len() {
        return Parse::Incomplete;
    }
    let mut fields = [0u32; 3];
    let mut field = 0;
    let mut digits = 0;
    for (offset, &b) in bytes[PREFIX.len()..].iter().enumerate() {
        match b {
            b'0'..=b'9' => {
                fields[field] = fields[field]
                    .saturating_mul(10)
                    .saturating_add(u32::from(b - b'0'));
                digits += 1;
            }
            b';' if field < 2 && digits > 0 => {
                field += 1;
                digits = 0;
            }
            b'M' | b'm' if field == 2 && digits > 0 => {
                let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                return Parse::Report {
                    len: PREFIX.len() + offset + 1,
                    button: clamp(fields[0]),
                    x: clamp(fields[1]),
                    y: clamp(fields[2]),
                    press: b == b'M',
                };
            }
            _ => return Parse::NotReport(PREFIX.len() + offset),
        }
    }
    Parse::Incomplete
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLOSE: CellRect = CellRect {
        row: 0,
        col: 70,
        width: 3,
        height: 1,
    };
    const CPU: CellRect = CellRect {
        row: 0,
        col: 20,
        width: 30,
        height: 3,
    };

    #[test]
    fn unarmed_filter_is_a_byte_exact_pass_through() {
        let mut f = MouseFilter::new();
        let input = b"typed\x1b[<0;71;1Mtext\x1b";
        assert_eq!(f.process(input, None, None, false).bytes, input);
    }

    #[test]
    fn a_click_on_the_close_button_is_swallowed_with_its_release() {
        let mut f = MouseFilter::new();
        let result = f.process(b"a\x1b[<0;71;1Mb\x1b[<0;71;1mc", Some(CLOSE), None, false);
        assert!(result.dismissed);
        assert_eq!(result.bytes, b"abc");
    }

    #[test]
    fn clicks_elsewhere_and_other_buttons_reach_the_child() {
        let mut f = MouseFilter::new();
        let input = b"\x1b[<0;10;5M\x1b[<0;10;5m\x1b[<2;71;1M\x1b[<64;71;1M\x1b[<32;71;1M";
        let result = f.process(input, Some(CLOSE), None, false);
        assert!(!result.dismissed);
        assert_eq!(result.bytes, input);
    }

    #[test]
    fn a_report_split_across_chunks_is_reassembled() {
        let mut f = MouseFilter::new();
        let result = f.process(b"x\x1b[<0;7", Some(CLOSE), None, false);
        assert_eq!(result.bytes, b"x");
        assert!(!result.dismissed);
        let result = f.process(b"1;1M", Some(CLOSE), None, false);
        assert!(result.dismissed);
        assert!(result.bytes.is_empty());
        let result = f.process(b"\x1b[<0;71;1m", None, None, false);
        assert!(
            result.bytes.is_empty(),
            "release after dismiss is swallowed even if disarmed"
        );
    }

    #[test]
    fn keyboard_escape_sequences_pass_through_unchanged() {
        let mut f = MouseFilter::new();
        let input = b"\x1b[A\x1bOP\x1b[99;5u\x1b[200~paste\x1b[201~";
        let result = f.process(input, Some(CLOSE), None, false);
        assert!(!result.dismissed);
        assert_eq!(result.bytes, input);
    }

    #[test]
    fn a_lone_trailing_escape_is_held_then_flushed_on_idle() {
        let mut f = MouseFilter::new();
        let result = f.process(b"q\x1b", Some(CLOSE), None, false);
        assert_eq!(result.bytes, b"q");
        assert_eq!(f.flush_pending(), b"\x1b");
        assert!(f.flush_pending().is_empty());
    }

    #[test]
    fn malformed_reports_are_forwarded() {
        let mut f = MouseFilter::new();
        let input = b"\x1b[<0;;1M\x1b[<a";
        let result = f.process(input, Some(CLOSE), None, false);
        assert!(!result.dismissed);
        assert_eq!(result.bytes, input);
    }

    #[test]
    fn cpu_hover_observes_motion_without_swallowing_input() {
        let mut f = MouseFilter::new();
        let click_bytes = b"\x1b[<0;21;1M\x1b[<0;21;1m";
        let click = f.process(click_bytes, None, Some(CPU), false);
        assert_eq!(click.bytes, click_bytes);
        assert_eq!(click.cpu_hover, None, "no hover without any-motion");

        let enter_bytes = b"\x1b[<35;21;1M";
        let enter = f.process(enter_bytes, None, Some(CPU), true);
        assert_eq!(enter.cpu_hover, Some(true));
        assert_eq!(enter.bytes, enter_bytes);
        let leave = f.process(b"\x1b[<35;5;8M", None, Some(CPU), true);
        assert_eq!(leave.cpu_hover, Some(false));
        assert_eq!(leave.bytes, b"\x1b[<35;5;8M");

        let drag = b"\x1b[<32;21;1M";
        let dragging = f.process(drag, None, Some(CPU), true);
        assert_eq!(dragging.cpu_hover, Some(true));
        assert_eq!(dragging.bytes, drag);
    }

    #[test]
    fn split_cpu_motion_report_is_forwarded_byte_exact_when_complete() {
        let mut f = MouseFilter::new();
        assert!(f
            .process(b"\x1b[<35;2", None, Some(CPU), true)
            .bytes
            .is_empty());
        let result = f.process(b"1;1M", None, Some(CPU), true);
        assert_eq!(result.bytes, b"\x1b[<35;21;1M");
        assert_eq!(result.cpu_hover, Some(true));
    }
}
