//! Executing a downloaded bootstrap script (#2609).
//!
//! Windows has no shebang line to honour, so the script runs under bash.

use std::io;
use std::path::Path;

/// Return the argv that runs `script` under bash.
pub fn script_argv(script: &Path) -> io::Result<Vec<String>> {
    Ok(vec![
        "bash".to_string(),
        script.to_string_lossy().into_owned(),
    ])
}
