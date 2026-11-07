//! Unix half of `crate::console_title`.
//!
//! POSIX terminals have no API to read the title back, so there is no
//! drift-detecting keeper here. The one-shot stamp is an OSC 2 written to
//! stdout: it lands after the shell's preexec hook (kitty, zsh/bash
//! integrations title the window with the command line, e.g. `clud -c`),
//! so it replaces that command line with `clud <cwd-name>`.

use std::io::Write;

/// Never spawns: returns `false`. Without a way to read the live title
/// a keeper could only re-stamp blindly, fighting any child that sets a
/// meaningful title of its own.
pub fn spawn_keeper_thread() -> bool {
    false
}

pub fn set_title(title: &str) {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1 {
        // Piped or redirected: escape bytes would corrupt the output.
        return;
    }
    let bytes = crate::console_title::osc2_title_bytes(title);
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&bytes);
    let _ = out.flush();
}
