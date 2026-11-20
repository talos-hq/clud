//! `Install clud.app` exists only on macOS (#2863); Windows has no bundle
//! launch, so these dialogs are never shown there.

use std::path::Path;

use crate::self_install::app_bundle::Dialogs;

/// Never called on Windows: `app_bundle::run` returns before it.
pub fn run_helper(_helper: &Path, _flag: &str) -> Result<i32, String> {
    Err("Install clud.app is macOS-only".into())
}

/// Never constructed on Windows: `app_bundle::run` returns before it.
pub struct NativeDialogs;

impl Dialogs for NativeDialogs {
    fn choose(&mut self, _message: &str, _buttons: &[&str]) -> Option<String> {
        None
    }

    fn open_terminal(&mut self, _script: &str) -> Result<(), String> {
        Err("Install clud.app is macOS-only".into())
    }
}
