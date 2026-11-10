//! All credential UI belongs to lokr, including caller-checked askpass acquisition.
use super::{Failure, Options, Secret};

pub(super) fn prompt(options: &Options) -> Result<Secret, Failure> {
    if super::real_ui_disabled() {
        return Err(Failure::Unavailable);
    }
    let dir = tempfile::tempdir().map_err(|_| Failure::Sink)?;
    let path = dir.path().join("credential");
    let mut request = options.clone();
    request.to_file = Some(path.clone());
    request.vault = None;
    request.to_keyring.clear();
    request.to_command.clear();
    super::lokr::request(&request)?;
    std::fs::read_to_string(path)
        .map(zeroize::Zeroizing::new)
        .map_err(|_| Failure::Sink)
}

pub(super) fn destination_label(options: &Options) -> String {
    if let Some(path) = &options.to_file {
        format!("Store in {}", path.display())
    } else if let Some(command) = options.to_command.first() {
        format!("Send to {command}")
    } else if options.vault.is_some() || !options.to_keyring.is_empty() {
        "Store in the native credential vault".to_string()
    } else {
        "Return to the requesting authentication tool".to_string()
    }
}
