//! Opt-in references to external context, resolved once per harness launch.
use std::path::Path;

use serde_json::Value;

use crate::{args::Args, backend::Backend, clud_settings};

pub fn prepare(args: &mut Args, backend: Backend) -> Result<(), String> {
    args.optional_context = None;
    if !matches!(backend, Backend::Claude | Backend::Codex) {
        return Ok(());
    }
    let home = clud_settings::home_dir_path().map_err(|error| error.to_string())?;
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    if enabled_at(&home, &cwd)? {
        args.optional_context = Some(crate::setup_command::hermes_context_reference(&home)?);
    }
    Ok(())
}

fn optional_document(path: &Path) -> Result<Value, String> {
    if !path
        .try_exists()
        .map_err(|error| format!("{}: {error}", path.display()))?
    {
        return Ok(serde_json::json!({}));
    }
    clud_settings::read_settings_json_file(path).map_err(|error| error.to_string())
}

fn enabled_at(home: &Path, cwd: &Path) -> Result<bool, String> {
    let global = optional_document(&clud_settings::settings_path_at(home))?;
    let local = crate::config::repo_settings_path_for_cwd(cwd)
        .map(|path| optional_document(&path))
        .transpose()?;
    let merged = clud_settings::merged_settings_document(&global, local.as_ref());
    match merged.pointer("/context/hermes/enabled") {
        None => Ok(false),
        Some(Value::Bool(enabled)) => Ok(*enabled),
        Some(_) => Err("context.hermes.enabled must be true or false".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_is_disabled_by_default_and_requires_a_boolean() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        assert!(!enabled_at(home.path(), cwd.path()).unwrap());
        std::fs::create_dir(home.path().join(".clud")).unwrap();
        let path = clud_settings::settings_path_at(home.path());
        std::fs::write(&path, r#"{"context":{"hermes":{"enabled":true}}}"#).unwrap();
        assert!(enabled_at(home.path(), cwd.path()).unwrap());
        std::fs::write(&path, r#"{"context":{"hermes":{"enabled":"true"}}}"#).unwrap();
        assert!(enabled_at(home.path(), cwd.path()).is_err());
    }

    #[test]
    fn repository_setting_can_disable_the_global_reference() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".clud")).unwrap();
        std::fs::write(
            clud_settings::settings_path_at(home.path()),
            r#"{"context":{"hermes":{"enabled":true}}}"#,
        )
        .unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        std::fs::create_dir(repo.path().join(".clud")).unwrap();
        std::fs::write(
            repo.path().join(".clud/settings.json"),
            r#"{"context":{"hermes":{"enabled":false}}}"#,
        )
        .unwrap();
        assert!(!enabled_at(home.path(), repo.path()).unwrap());
    }
}
