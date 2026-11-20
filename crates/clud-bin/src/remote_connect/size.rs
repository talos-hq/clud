//! Whose terminal sets the remote shell's size (DD-229 in
//! `docs/DESIGN_DECISIONS.md`).
//!
//! The consenting user's terminal shows the shell's output byte for byte
//! (`console.rs`), so it draws correctly only when the PTY has exactly its
//! grid: a full-screen program positions the cursor and wraps lines for the
//! PTY's size, and at any other size its redraws overlap and leave stale rows
//! behind. The operator's browser console can render any grid; the user's
//! terminal cannot be resized by this client. So when this client has a
//! terminal and the server says it follows (`hello.pty_sizing == "client"`),
//! the local terminal's size is the session's size: the PTY opens at it,
//! follows the local window, ignores the operator's resizes, and every change
//! is announced as `pty_size` for the console to render. Without a local
//! terminal, or with a server that does not follow, the operator's size is
//! used as before.
use serde_json::Value;
use std::io::IsTerminal;
use std::sync::mpsc;

/// The `hello.pty_sizing` value of a server that follows the client's size.
pub(super) const CLIENT_SIZING: &str = "client";

#[derive(Default)]
pub(super) struct SessionSize {
    /// `(rows, cols)` of the local terminal, when stdout is one.
    local: Option<(u16, u16)>,
    /// The server renders the size this client announces.
    follows: bool,
    /// Local terminal resizes, `(rows, cols)`.
    changes: Option<mpsc::Receiver<(u16, u16)>>,
}

impl SessionSize {
    /// The local terminal's size, watched for resizes, when stdout is a terminal.
    pub fn watch_stdout() -> Self {
        if !std::io::stdout().is_terminal() {
            return Self::default();
        }
        let Ok((cols, rows)) = crossterm::terminal::size() else {
            return Self::default();
        };
        let (tx, rx) = mpsc::channel();
        crate::platform::session::spawn_os_resize_watcher(tx);
        Self {
            local: Some((rows, cols)),
            follows: false,
            changes: Some(rx),
        }
    }

    #[cfg(test)]
    pub fn with_local(local: Option<(u16, u16)>, changes: mpsc::Receiver<(u16, u16)>) -> Self {
        Self {
            local,
            follows: false,
            changes: Some(changes),
        }
    }

    /// Every `hello` says whether the server follows the client's size.
    pub fn hello(&mut self, hello: &Value) {
        self.follows = hello["pty_sizing"] == CLIENT_SIZING;
    }

    /// `(rows, cols)` this client sets for the session, when it sets it.
    pub fn governing(&self) -> Option<(u16, u16)> {
        self.local.filter(|_| self.follows)
    }

    /// An operator's `pty_open` or `pty_resize`, with the session's size.
    pub fn applied(&self, event: &Value) -> Value {
        let mut event = event.clone();
        if let Some((rows, cols)) = self.governing() {
            event["rows"] = rows.into();
            event["cols"] = cols.into();
        }
        event
    }

    /// The local terminal's newest size, if it changed and this client sets
    /// the session's size. Changes are recorded either way.
    pub fn changed(&mut self) -> Option<(u16, u16)> {
        let latest = self.changes.as_ref()?.try_iter().last()?;
        let changed = self.local != Some(latest);
        self.local = Some(latest);
        self.governing().filter(|_| changed)
    }
}

#[cfg(test)]
#[path = "size_tests.rs"]
mod tests;
