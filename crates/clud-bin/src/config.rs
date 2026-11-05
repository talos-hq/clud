//! `clud config` settings inspection and editing.

use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use running_process::{NativeProcess, ProcessConfig, StderrMode, StdinMode};
use serde_json::{json, Value};

use crate::args::{parser_command, Args, ConfigSubcommand};
use crate::{clud_settings, loop_spec, subprocess};

const REPO_SETTINGS_DIR: &str = ".clud";
const REPO_SETTINGS_FILE: &str = "settings.json";
const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "backend",
    "codex",
    "context",
    "foreground",
    "hook_health",
    "launch_setup",
    "optimize",
    "shell",
];

static UNKNOWN_KEYS_WARNED: AtomicBool = AtomicBool::new(false);

pub fn run(args: &Args, subcommand: Option<ConfigSubcommand>) -> i32 {
    let Some(subcommand) = subcommand else {
        return print_help_and_exit_zero();
    };

    match run_inner(args, subcommand) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

fn run_inner(args: &Args, subcommand: ConfigSubcommand) -> Result<i32, String> {
    let home = clud_settings::home_dir_path().map_err(|error| error.to_string())?;
    let cwd = env::current_dir().map_err(|error| format!("current directory: {error}"))?;
    match subcommand {
        ConfigSubcommand::Set { assignment } => {
            let change = assignment_change(&assignment)?;
            if args.dry_run {
                println!("[clud] dry-run: would propose {assignment}");
                return Ok(0);
            }
            Ok(clud_settings::authorized::run(&change))
        }
        ConfigSubcommand::Show { json } => {
            let mut stdout = io::stdout().lock();
            show_to_writer(&home, &cwd, json, &mut stdout)?;
            Ok(0)
        }
        ConfigSubcommand::Edit { local, editor } => {
            let path = ensure_edit_target(&home, &cwd, local)?;
            if args.dry_run {
                println!("[clud] dry-run: would open {}", path.display());
                return Ok(0);
            }
            let argv = editor_command_argv(editor.as_deref(), &path)?;
            run_editor(argv)?;
            Ok(0)
        }
    }
}

fn assignment_change(assignment: &str) -> Result<clud_settings::authorized::Mutation, String> {
    let (key, raw) = assignment
        .split_once('=')
        .ok_or("expected KEY=VALUE, for example context.hermes.enabled=true")?;
    if key.is_empty()
        || key.split('.').any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    {
        return Err("settings keys must be dot-separated names".into());
    }
    let value: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
    if key == "context.hermes.enabled" && !value.is_boolean() {
        return Err("context.hermes.enabled must be true or false".into());
    }
    Ok(clud_settings::authorized::Mutation::Set {
        pointer: format!("/{}", key.replace('.', "/")),
        value: value.to_string(),
        reason: format!("Set {key} using clud config set"),
    })
}

pub(crate) fn show_to_writer(
    home: &Path,
    cwd: &Path,
    json_output: bool,
    writer: &mut dyn Write,
) -> Result<(), String> {
    let global_path = clud_settings::settings_path_at(home);
    let global =
        clud_settings::load_or_init_global_settings_at(home).map_err(|error| error.to_string())?;
    warn_unknown_top_level_keys("global", &global);

    let local_path = repo_settings_path_for_cwd(cwd);
    let local = match &local_path {
        Some(path) if path.is_file() => {
            let document =
                clud_settings::read_settings_json_file(path).map_err(|error| error.to_string())?;
            warn_unknown_top_level_keys("local", &document);
            Some(document)
        }
        Some(_) | None => None,
    };

    let merged = clud_settings::merged_settings_document(&global, local.as_ref());

    if json_output {
        let payload = json!({
            "paths": {
                "global": path_to_json_string(&global_path),
                "local": local_path.as_ref().map(|path| path_to_json_string(path)),
            },
            "global": global,
            "local": local.clone().unwrap_or(Value::Null),
            "merged": merged,
        });
        write_json_value(writer, &payload)?;
        return Ok(());
    }

    writeln!(writer, "global: {}", global_path.display()).map_err(write_error)?;
    match &local_path {
        Some(path) if local.is_some() => {
            writeln!(writer, "local: {}", path.display()).map_err(write_error)?;
        }
        Some(path) => {
            writeln!(writer, "local: {} (not found)", path.display()).map_err(write_error)?;
        }
        None => {
            writeln!(writer, "local: (not inside a git repository)").map_err(write_error)?;
        }
    }
    writeln!(writer).map_err(write_error)?;
    write_named_json_value(writer, "merged", &merged)?;
    if let Some(local) = &local {
        write_named_json_value(writer, "local", local)?;
    }
    write_named_json_value(writer, "global", &global)?;
    Ok(())
}

pub(crate) fn repo_settings_path_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let root = loop_spec::git_root_from(cwd);
    if root.join(".git").exists() {
        Some(root.join(REPO_SETTINGS_DIR).join(REPO_SETTINGS_FILE))
    } else {
        None
    }
}

pub(crate) fn ensure_edit_target(home: &Path, cwd: &Path, local: bool) -> Result<PathBuf, String> {
    if local {
        let path = repo_settings_path_for_cwd(cwd).ok_or_else(|| {
            "not inside a git repository; `clud config edit --local` writes repo .clud/settings.json"
                .to_string()
        })?;
        load_or_init_settings_file(&path)?;
        return Ok(path);
    }

    let _ =
        clud_settings::load_or_init_global_settings_at(home).map_err(|error| error.to_string())?;
    Ok(clud_settings::settings_path_at(home))
}

pub(crate) fn editor_command_argv(
    editor_override: Option<&str>,
    path: &Path,
) -> Result<Vec<String>, String> {
    let mut argv = if let Some(command) = editor_override
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(editor_from_env)
    {
        split_editor_command(&command)?
    } else {
        default_editor_command()?
    };
    argv.push(path.to_string_lossy().to_string());
    Ok(argv)
}

fn load_or_init_settings_file(path: &Path) -> Result<Value, String> {
    let mut document = match clud_settings::read_settings_json_file(path) {
        Ok(document) => document,
        Err(clud_settings::SettingsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            json!({})
        }
        Err(error) => return Err(error.to_string()),
    };
    let original = document.clone();
    clud_settings::seed_global_settings_defaults(&mut document);
    if document != original || !path.is_file() {
        clud_settings::write_settings_json_file(path, &document)
            .map_err(|error| error.to_string())?;
    }
    Ok(document)
}

fn editor_from_env() -> Option<String> {
    env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}

fn split_editor_command(command: &str) -> Result<Vec<String>, String> {
    let argv = shell_words::split(command)
        .map_err(|error| format!("failed to parse editor command `{command}`: {error}"))?;
    if argv.is_empty() {
        Err("editor command is empty".to_string())
    } else {
        Ok(argv)
    }
}

use crate::platform::config::default_editor_command;

fn run_editor(argv: Vec<String>) -> Result<(), String> {
    let process = NativeProcess::new(ProcessConfig {
        command: subprocess::command_spec_for_subprocess(argv),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    process
        .start()
        .map_err(|error| format!("failed to start editor: {error}"))?;
    let exit_code = process
        .wait(None)
        .map_err(|error| format!("failed to wait for editor: {error}"))?;
    if exit_code == 0 {
        Ok(())
    } else {
        Err(format!("editor exited with {exit_code}"))
    }
}

fn print_help_and_exit_zero() -> i32 {
    let mut command = parser_command();
    if let Some(config) = command.find_subcommand_mut("config") {
        let _ = config.print_help();
        println!();
    }
    0
}

fn warn_unknown_top_level_keys(scope: &str, document: &Value) {
    let Some(object) = document.as_object() else {
        return;
    };
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !KNOWN_TOP_LEVEL_KEYS.contains(key))
        .collect();
    if unknown.is_empty() || UNKNOWN_KEYS_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "[clud] warning: {scope} settings contain unknown top-level keys ({}); preserving them",
        unknown.join(", ")
    );
}

fn write_named_json_value(
    writer: &mut dyn Write,
    label: &str,
    value: &Value,
) -> Result<(), String> {
    writeln!(writer, "{label}:").map_err(write_error)?;
    write_json_value(writer, value)?;
    writeln!(writer).map_err(write_error)?;
    Ok(())
}

fn write_json_value(writer: &mut dyn Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer_pretty(&mut *writer, value)
        .map_err(|error| format!("serialize settings: {error}"))?;
    writeln!(writer).map_err(write_error)?;
    Ok(())
}

fn path_to_json_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn write_error(error: io::Error) -> String {
    format!("write output: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn dotted_assignment_uses_the_authorized_settings_mutation() {
        let clud_settings::authorized::Mutation::Set { pointer, value, .. } =
            assignment_change("context.hermes.enabled=true").unwrap()
        else {
            panic!("expected set")
        };
        assert_eq!(pointer, "/context/hermes/enabled");
        assert_eq!(value, "true");
        for invalid in [
            "use-hermes",
            "context..enabled=true",
            "context.hermes.enabled=yes",
            "context/hermes=true",
            "=true",
        ] {
            assert!(assignment_change(invalid).is_err(), "{invalid}");
        }
        let clud_settings::authorized::Mutation::Set { value, .. } =
            assignment_change("context.hermes.enabled=false").unwrap()
        else {
            panic!("expected set")
        };
        assert_eq!(value, "false");
    }

    #[test]
    fn repo_settings_path_requires_git_root() {
        let repo = tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        let child = repo.path().join("child");
        fs::create_dir(&child).unwrap();

        assert_eq!(
            repo_settings_path_for_cwd(&child).unwrap(),
            repo.path().join(".clud").join("settings.json")
        );

        let outside = tempdir().unwrap();
        assert!(repo_settings_path_for_cwd(outside.path()).is_none());
    }

    #[test]
    fn show_json_seeds_global_defaults_and_merges_local_settings() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        let local_path = repo.path().join(".clud").join("settings.json");
        fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        fs::write(
            &local_path,
            r#"{"shell":{"disable_powershell":true},"codex":{"config_overrides":["local"]}}"#,
        )
        .unwrap();

        let mut output = Vec::new();
        show_to_writer(home.path(), repo.path(), true, &mut output).unwrap();
        let payload: Value = serde_json::from_slice(&output).unwrap();

        assert_eq!(
            payload["paths"]["global"],
            path_to_json_string(&clud_settings::settings_path_at(home.path()))
        );
        assert_eq!(payload["paths"]["local"], path_to_json_string(&local_path));
        assert_eq!(payload["global"]["hook_health"]["auto_fix_hooks"], true);
        assert_eq!(payload["local"]["shell"]["disable_powershell"], true);
        assert_eq!(payload["merged"]["shell"]["disable_powershell"], true);
        assert_eq!(
            payload["merged"]["codex"]["config_overrides"],
            json!(["local"])
        );
        assert!(clud_settings::settings_path_at(home.path()).is_file());
    }

    #[test]
    fn ensure_edit_target_local_seeds_repo_settings() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();

        let path = ensure_edit_target(home.path(), repo.path(), true).unwrap();
        let document = clud_settings::read_settings_json_file(&path).unwrap();

        assert_eq!(path, repo.path().join(".clud").join("settings.json"));
        assert_eq!(document["shell"]["disable_powershell"], false);
        assert_eq!(document["hook_health"]["auto_fix_hooks"], true);
        assert_eq!(
            document["codex"]["config_overrides"][0],
            clud_settings::DEFAULT_CODEX_GITHUB_PLUGIN_CONFIG_OVERRIDE
        );
    }

    #[test]
    fn ensure_edit_target_local_requires_repo() {
        let home = tempdir().unwrap();
        let outside = tempdir().unwrap();

        let error = ensure_edit_target(home.path(), outside.path(), true).unwrap_err();

        assert!(error.contains("not inside a git repository"), "{error}");
    }

    #[test]
    fn editor_command_parses_override_and_appends_path() {
        let argv = editor_command_argv(Some("code --wait"), Path::new("settings.json")).unwrap();

        assert_eq!(argv, vec!["code", "--wait", "settings.json"]);
    }
}
