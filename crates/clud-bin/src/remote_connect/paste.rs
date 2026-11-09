//! Bracketed-paste gate for operator input.
//!
//! A terminal may wrap a paste in `ESC[200~` … `ESC[201~` only while the
//! program has asked for it with `ESC[?2004h`. The operator's browser terminal
//! decides from a replayed copy of the shell's output, so its idea of the mode
//! can be stale; a shell that is not expecting the markers then prints them as
//! typed text (`^[[200~curl … | sh~`). The client sees the real output stream,
//! so it tracks the mode here and drops the markers while the shell has not
//! enabled it.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const ENABLE: &[u8] = b"\x1b[?2004h";
const DISABLE: &[u8] = b"\x1b[?2004l";
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// The operator console's input frame size (clud.bot `console.ts`).
pub(super) const FRAME: usize = 4096;

/// Whether the shell currently accepts bracketed paste; shared by both pumps.
#[derive(Clone, Default)]
pub(super) struct PasteMode(Arc<AtomicBool>);

impl PasteMode {
    pub(super) fn enabled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Follows `ESC[?2004h` / `ESC[?2004l` in the shell's output, across chunks.
pub(super) struct OutputTracker {
    mode: PasteMode,
    tail: Vec<u8>,
}

impl OutputTracker {
    pub(super) fn new(mode: PasteMode) -> Self {
        Self {
            mode,
            tail: Vec::new(),
        }
    }

    pub(super) fn observe(&mut self, chunk: &[u8]) {
        let mut seen = std::mem::take(&mut self.tail);
        seen.extend_from_slice(chunk);
        let last = |needle: &[u8]| seen.windows(needle.len()).rposition(|w| w == needle);
        match (last(ENABLE), last(DISABLE)) {
            (Some(on), Some(off)) => self.mode.0.store(on > off, Ordering::Relaxed),
            (Some(_), None) => self.mode.0.store(true, Ordering::Relaxed),
            (None, Some(_)) => self.mode.0.store(false, Ordering::Relaxed),
            (None, None) => {}
        }
        let keep = seen.len().min(ENABLE.len() - 1);
        self.tail = seen[seen.len() - keep..].to_vec();
    }
}

/// Removes paste markers from operator input while the shell has not asked for
/// them. The console splits a long paste into frames of [`FRAME`] bytes; a
/// marker cut by such a split is held until the next frame completes it. A
/// shorter frame is never held, so a lone Esc keypress is delivered at once.
pub(super) struct InputFilter {
    mode: PasteMode,
    pending: Vec<u8>,
}

impl InputFilter {
    pub(super) fn new(mode: PasteMode) -> Self {
        Self {
            mode,
            pending: Vec::new(),
        }
    }

    pub(super) fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(data);
        if self.mode.enabled() {
            return bytes;
        }
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            if rest.starts_with(PASTE_START) || rest.starts_with(PASTE_END) {
                i += PASTE_START.len();
                continue;
            }
            if data.len() == FRAME && rest.len() < PASTE_START.len() && is_marker_prefix(rest) {
                self.pending = rest.to_vec();
                break;
            }
            out.push(rest[0]);
            i += 1;
        }
        out
    }
}

fn is_marker_prefix(bytes: &[u8]) -> bool {
    PASTE_START.starts_with(bytes) || PASTE_END.starts_with(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (OutputTracker, InputFilter) {
        let mode = PasteMode::default();
        (OutputTracker::new(mode.clone()), InputFilter::new(mode))
    }

    const PASTE: &[u8] = b"\x1b[200~curl -fsSL https://clud.bot/install.sh | sh\x1b[201~";
    const CLEAN: &[u8] = b"curl -fsSL https://clud.bot/install.sh | sh";

    #[test]
    fn markers_are_dropped_before_the_shell_enables_bracketed_paste() {
        let (_, mut input) = pair();
        assert_eq!(input.filter(PASTE), CLEAN);
    }

    #[test]
    fn markers_pass_through_while_the_shell_enabled_bracketed_paste() {
        let (mut output, mut input) = pair();
        output.observe(b"\x1b[?2004h[user@host ~]$ ");
        assert_eq!(input.filter(PASTE), PASTE);
    }

    #[test]
    fn markers_are_dropped_after_the_shell_disables_it_to_run_a_command() {
        let (mut output, mut input) = pair();
        output.observe(b"\x1b[?2004h$ ");
        output.observe(b"sudo systemctl stop tailscaled\r\n\x1b[?2004l\r");
        assert_eq!(input.filter(PASTE), CLEAN);
    }

    #[test]
    fn the_last_mode_change_in_a_chunk_wins() {
        let (mut output, mut input) = pair();
        output.observe(b"\x1b[?2004l\r\nout\r\n\x1b[?2004h$ ");
        assert_eq!(input.filter(PASTE), PASTE);
        output.observe(b"\x1b[?2004h$ \x1b[?2004l");
        assert_eq!(input.filter(PASTE), CLEAN);
    }

    #[test]
    fn a_mode_change_split_across_output_chunks_is_seen() {
        let (mut output, mut input) = pair();
        output.observe(b"prompt \x1b[?20");
        output.observe(b"04h$ ");
        assert_eq!(input.filter(PASTE), PASTE);
        output.observe(b"\x1b[?2004");
        output.observe(b"l");
        assert_eq!(input.filter(PASTE), CLEAN);
    }

    #[test]
    fn a_marker_split_across_full_input_frames_is_still_dropped() {
        let (_, mut input) = pair();
        let mut paste = b"\x1b[200~".to_vec();
        paste.extend(std::iter::repeat_n(b'a', 2 * FRAME));
        paste.extend_from_slice(b"\x1b[201~");
        let mut got = Vec::new();
        // Cut so that both markers straddle a frame boundary.
        let mut shifted = vec![b'a'; FRAME - 3];
        shifted.extend_from_slice(&paste);
        for frame in shifted.chunks(FRAME) {
            got.extend(input.filter(frame));
        }
        assert_eq!(got.len(), FRAME - 3 + 2 * FRAME);
        assert!(got.iter().all(|b| *b == b'a'));
    }

    #[test]
    fn a_lone_escape_keypress_is_delivered_immediately() {
        let (_, mut input) = pair();
        assert_eq!(input.filter(b"\x1b"), b"\x1b");
        assert_eq!(input.filter(b"\x1b["), b"\x1b[");
    }

    #[test]
    fn other_escape_sequences_are_untouched() {
        let (_, mut input) = pair();
        let keys = b"\x1b[A\x1b[B\x1b[1;5C\x1bOP\x03\r";
        assert_eq!(input.filter(keys), keys);
        assert_eq!(input.filter(b"\x1b[2~x"), b"\x1b[2~x");
    }
}
