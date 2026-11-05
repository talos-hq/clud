//! Executing a downloaded bootstrap script through its own shebang (#2609).
//!
//! hermes-slim ships `#!/bin/sh` and talos `#!/usr/bin/env bash`, so the script
//! must pick its own interpreter: naming one would break the other.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Make `script` executable and return the argv that runs it.
pub fn script_argv(script: &Path) -> io::Result<Vec<String>> {
    std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o700))?;
    Ok(vec![script.to_string_lossy().into_owned()])
}
