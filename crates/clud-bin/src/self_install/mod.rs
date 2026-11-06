//! Native self-installer components shared by the CLI and install transaction.

pub mod activation;
pub mod catalog;
pub mod entry;
pub mod picker;
mod result;
pub mod setup;
pub mod transaction;


/// Diagnostic only: append `<epoch ms> <pid> <phase>` to `CLUD_INSTALLER_TRACE`.
pub fn trace(phase: &str) {
    use std::io::Write;
    let Some(path) = std::env::var_os("CLUD_INSTALLER_TRACE") else { return };
    let ms = std::time::UNIX_EPOCH.elapsed().unwrap_or_default().as_millis();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{ms} {} {phase}", std::process::id());
    }
}
