//! DeepSeek Harness (`dsh`) as a first-class install target (#1829).
//!
//! Upstream ships `dsh` only as an npm developer preview. clud owns a
//! versioned private install under `~/.clud/harnesses/dsh/<version>/`: the
//! reviewed `@deepseek-ai/dsh` release plus a private Node.js runtime from the
//! `node` npm package, so neither global npm state nor the system Node is
//! touched. Node 26 fails dsh's native addon at boot even though upstream's
//! `engines` range admits it, so the private runtime is pinned to a Node
//! version dsh is known to boot under.
//!
//! The child environment carries the provider key from clud's vault, and an
//! OpenRouter launch adds a clud-owned `--patch` overlay that selects dsh's
//! built-in `openrouter` route and the resolved model. The user's own dsh
//! profiles and `$DSH_HOME/.credentials.yaml` are never written.

use std::path::{Path, PathBuf};
use std::time::Duration;

use running_process::ReadStatus;

use crate::backend::ModelProvider;
use crate::subprocess;
use crate::win_creation_flags::invisible_helper_creationflags;

/// Reviewed upstream release. Bump only after checking the release still
/// boots, honors `--patch`, and resolves `DEEPSEEK_API_KEY`/`OPENROUTER_API_KEY`.
pub const DSH_VERSION: &str = "0.2.0-rc.2";
pub const DSH_PACKAGE: &str = "@deepseek-ai/dsh";
/// Private runtime installed beside dsh. 24.x is inside upstream's `engines`
/// range and boots the native addon that Node 26 rejects.
pub const NODE_VERSION: &str = "24.21.0";
pub const UPDATE_COMMAND: &str = "clud dsh-update";

/// Root of every managed dsh version.
pub fn managed_root(home: &Path) -> PathBuf {
    home.join(".clud").join("harnesses").join("dsh")
}

/// The managed prefix for the reviewed version.
pub fn managed_prefix(home: &Path) -> PathBuf {
    managed_root(home).join(DSH_VERSION)
}

/// npm's per-prefix bin directory: holds both `dsh` and the private `node`.
pub fn bin_dir(prefix: &Path) -> PathBuf {
    prefix.join("node_modules").join(".bin")
}

pub fn executable_name(windows: bool) -> &'static str {
    if windows {
        "dsh.cmd"
    } else {
        "dsh"
    }
}

/// The managed `dsh` launcher for `home`, whether or not it exists.
pub fn managed_executable(home: &Path, windows: bool) -> PathBuf {
    bin_dir(&managed_prefix(home)).join(executable_name(windows))
}

/// The home every managed-dsh path hangs off: clud's one home resolver, so
/// the install and backend discovery always agree (#1829, #1836).
pub fn managed_home() -> Option<PathBuf> {
    crate::home::user_home()
}

fn home_dir() -> Result<PathBuf, String> {
    managed_home().ok_or_else(|| "home directory unavailable".to_string())
}

/// The bin directory to put first on the child's PATH when `executable` is a
/// clud-managed dsh, so its `#!/usr/bin/env node` (or `.cmd` lookup) finds the
/// private Node instead of whatever the system has.
pub fn managed_bin_for(executable: &Path, home: &Path) -> Option<PathBuf> {
    let root = managed_root(home);
    let parent = executable.parent()?;
    (parent.starts_with(&root) && parent.ends_with(Path::new("node_modules").join(".bin")))
        .then(|| parent.to_path_buf())
}

/// The npm argv that installs the reviewed release into `prefix`.
pub fn npm_install_argv(npm: &str, prefix: &Path) -> Vec<String> {
    vec![
        npm.to_string(),
        "install".to_string(),
        "--prefix".to_string(),
        prefix.to_string_lossy().into_owned(),
        "--no-fund".to_string(),
        "--no-audit".to_string(),
        "--no-save".to_string(),
        "--loglevel=error".to_string(),
        format!("{DSH_PACKAGE}@{DSH_VERSION}"),
        format!("node@{NODE_VERSION}"),
    ]
}

/// Install the reviewed dsh into the managed prefix. A present prefix is a
/// no-op. The install runs in a sibling staging directory and is renamed into
/// place only after `dsh --version` reports the pinned version, so an
/// interrupted or failed install never leaves a half-written prefix and any
/// other managed version stays usable.
pub fn install(home: &Path) -> Result<PathBuf, String> {
    let windows = crate::platform::host::WINDOWS;
    let prefix = managed_prefix(home);
    let executable = bin_dir(&prefix).join(executable_name(windows));
    if executable.is_file() {
        return Ok(executable);
    }
    let npm = which::which("npm").map_err(|_| {
        "npm was not found on PATH; install Node.js (any version that ships npm) and retry"
            .to_string()
    })?;
    let root = managed_root(home);
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let staging = root.join(format!("{DSH_VERSION}.staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = install_into(&npm.to_string_lossy(), &staging, windows).and_then(|()| {
        // The version probe's private Node/addon can still be unmapping on Windows.
        crate::self_install::transaction::rename_when_released(&staging, &prefix)
            .map_err(|e| format!("cannot move the install into {}: {e}", prefix.display()))
    });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result.map(|()| executable)
}

fn install_into(npm: &str, staging: &Path, windows: bool) -> Result<(), String> {
    std::fs::create_dir_all(staging)
        .map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    let (code, output) = run_captured(npm_install_argv(npm, staging), None)?;
    if code != 0 {
        return Err(format!("npm install exited with {code}: {}", tail(&output)));
    }
    let staged = bin_dir(staging).join(executable_name(windows));
    verify(&staged)
}

/// Run `dsh --version` under the managed PATH and require the pinned version.
pub fn verify(executable: &Path) -> Result<(), String> {
    let bin = executable
        .parent()
        .ok_or("dsh executable has no parent directory")?;
    let env = vec![(
        "PATH".to_string(),
        prepend_path(bin, std::env::var("PATH").ok().as_deref()),
    )];
    let argv = vec![
        executable.to_string_lossy().into_owned(),
        "--version".to_string(),
    ];
    let (code, output) = run_captured(argv, Some(env))?;
    if code != 0 {
        return Err(format!(
            "dsh --version exited with {code}: {}",
            tail(&output)
        ));
    }
    if !output.contains(DSH_VERSION) {
        return Err(format!(
            "dsh --version reported {:?}, expected {DSH_VERSION}",
            output.trim()
        ));
    }
    Ok(())
}

fn tail(output: &str) -> String {
    let lines: Vec<&str> = output.lines().rev().take(8).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
}

fn run_captured(
    argv: Vec<String>,
    env_overlay: Option<Vec<(String, String)>>,
) -> Result<(i32, String), String> {
    let process = match env_overlay {
        Some(overlay) => {
            let mut env: Vec<(String, String)> = std::env::vars()
                .filter(|(key, _)| !overlay.iter().any(|(k, _)| env_key_eq(k, key)))
                .collect();
            env.extend(overlay);
            subprocess::ManagedSubprocess::start(
                argv,
                None,
                env,
                true,
                invisible_helper_creationflags(),
            )
        }
        None => subprocess::ManagedSubprocess::start_inheriting_env(
            argv,
            None,
            true,
            invisible_helper_creationflags(),
        ),
    }
    .map_err(|err| format!("failed to start command: {err}"))?;
    let mut buf = Vec::<u8>::new();
    loop {
        match process.read_stdout(Some(Duration::from_millis(100))) {
            ReadStatus::Line(line) => {
                buf.extend_from_slice(&line);
                buf.push(b'\n');
            }
            ReadStatus::Timeout => {
                let _ = process.poll();
            }
            ReadStatus::Eof => break,
        }
    }
    let code = process
        .wait(Some(Duration::from_secs(900)))
        .map_err(|err| format!("failed to wait for command: {err}"))?;
    Ok((code, String::from_utf8_lossy(&buf).into_owned()))
}

pub fn prepend_path(dir: &Path, path: Option<&str>) -> String {
    let separator = if crate::platform::host::WINDOWS {
        ";"
    } else {
        ":"
    };
    let dir = dir.to_string_lossy();
    match path.filter(|value| !value.is_empty()) {
        Some(rest) => format!("{dir}{separator}{rest}"),
        None => dir.into_owned(),
    }
}

fn env_key_eq(left: &str, right: &str) -> bool {
    if crate::platform::host::WINDOWS {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

/// `clud dsh-update`: install (or confirm) the reviewed managed release.
/// Running the command is the consent; it writes only under the managed root.
pub fn run_update() -> i32 {
    let result = home_dir().and_then(|home| {
        let already = managed_executable(&home, crate::platform::host::WINDOWS).is_file();
        install(&home).map(|path| (already, path))
    });
    match result {
        Ok((true, path)) => {
            println!(
                "DeepSeek Harness {DSH_VERSION} is already installed at {}",
                path.display()
            );
            0
        }
        Ok((false, path)) => {
            println!(
                "Installed DeepSeek Harness {DSH_VERSION} (Node {NODE_VERSION}) at {}",
                path.display()
            );
            0
        }
        Err(error) => {
            eprintln!("DeepSeek Harness install failed: {error}");
            1
        }
    }
}

/// The environment variable dsh reads for a provider's key, if clud passes
/// that provider through to dsh.
pub fn key_env_for(provider: ModelProvider) -> Option<&'static str> {
    match provider {
        ModelProvider::DeepSeek => Some("DEEPSEEK_API_KEY"),
        ModelProvider::OpenRouter => Some("OPENROUTER_API_KEY"),
        _ => None,
    }
}

/// dsh's provider id for the OpenRouter route in its bundled pi-ai catalog.
pub const OPENROUTER_ROUTE: &str = "openrouter";

/// The overlay that makes OpenRouter dsh's active route with `wire_model`.
/// Declares the model explicitly so an ID outside dsh's bundled catalog
/// (such as `~anthropic/claude-sonnet-latest`) still resolves, and speaks the
/// same Anthropic Messages protocol at the same base URL clud's Claude route
/// uses.
pub fn openrouter_patch_yaml(wire_model: &str, base_url: &str) -> String {
    let model = yaml_quote(wire_model);
    let base_url = yaml_quote(base_url);
    format!(
        "# managed-by: clud. Regenerated on every OpenRouter dsh launch (#1829).\n\
         - id: llm-pi-ai\n  config:\n    providers:\n      {OPENROUTER_ROUTE}:\n\
         \x20       apiKeyEnv: OPENROUTER_API_KEY\n        api: anthropic-messages\n\
         \x20       baseURL: {base_url}\n        models:\n          - id: {model}\n\
         - id: agent-default-model\n  config:\n    provider: {OPENROUTER_ROUTE}\n\
         \x20   model: {model}\n"
    )
}

fn yaml_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Where the overlay for `wire_model` lives. One file per model, so
/// concurrent launches with different models never race on one path.
pub fn openrouter_patch_path(home: &Path, wire_model: &str) -> PathBuf {
    let safe: String = wire_model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    managed_root(home)
        .join("patches")
        .join(format!("openrouter-{safe}.yml"))
}

/// Write the overlay the plan's `--patch` argument names.
pub fn write_openrouter_patch(path: &Path, wire_model: &str, base_url: &str) -> Result<(), String> {
    let dir = path.parent().ok_or("patch path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let body = openrouter_patch_yaml(wire_model, base_url);
    let tmp = dir.join(format!(".{}.tmp", std::process::id()));
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The OpenRouter wire model a dsh launch selects: the resolved selection,
/// else OpenRouter's reviewed Sonnet role model.
pub fn openrouter_model(
    selection: Option<&crate::provider_catalog::ResolvedModelSelection>,
) -> Option<&str> {
    selection
        .and_then(|selection| {
            selection
                .wire_model
                .as_deref()
                .or(selection.model.as_deref())
        })
        .or_else(|| {
            crate::provider_registry::descriptor_for(ModelProvider::OpenRouter)
                .and_then(|descriptor| descriptor.role_models)
                .map(|roles| roles.sonnet)
        })
}

/// The `--patch` path for an OpenRouter dsh plan, or `None` when the plan is
/// not one.
pub fn plan_patch_path(provider: ModelProvider, wire_model: Option<&str>) -> Option<PathBuf> {
    if provider != ModelProvider::OpenRouter {
        return None;
    }
    let home = managed_home()?;
    Some(openrouter_patch_path(&home, wire_model?))
}

/// Facts the runtime needs to prepare a dsh child, kept apart from
/// `LaunchPlan` so the overlay is unit-testable.
pub struct ChildFacts<'a> {
    pub executable: &'a str,
    pub provider: ModelProvider,
    pub wire_model: Option<&'a str>,
    /// The base URL the OpenRouter overlay names. It comes from the launch's
    /// resolved route, so dsh never re-derives it here.
    pub base_url: Option<&'a str>,
}

/// Prepare the dsh child's environment and overlay (#1829):
/// - a managed dsh gets its private Node first on PATH;
/// - the provider's key from clud's vault becomes dsh's key variable unless
///   the launch environment already sets it (ambient wins, as on every
///   other route);
/// - an OpenRouter launch gets its `--patch` overlay written.
///
/// Keys go only into this child-local list, never into dsh's own store.
pub fn prepare_child(
    facts: &ChildFacts<'_>,
    home: Option<&Path>,
    env: &mut Vec<(String, String)>,
    ambient: &dyn Fn(&str) -> Option<String>,
    vault: &dyn Fn(ModelProvider) -> Option<String>,
) -> Result<(), String> {
    if let Some(bin) = home.and_then(|home| managed_bin_for(Path::new(facts.executable), home)) {
        let current = lookup(env, "PATH").or_else(|| ambient("PATH"));
        set(env, "PATH", &prepend_path(&bin, current.as_deref()));
    }
    let key_provider = match facts.provider {
        ModelProvider::OpenRouter => ModelProvider::OpenRouter,
        _ => ModelProvider::DeepSeek,
    };
    if let Some(var) = key_env_for(key_provider) {
        let present = lookup(env, var)
            .or_else(|| ambient(var))
            .is_some_and(|value| !value.is_empty());
        if !present {
            if let Some(key) = vault(key_provider) {
                set(env, var, &key);
            }
        }
    }
    if facts.provider == ModelProvider::OpenRouter {
        let home = home.ok_or("home directory unavailable for the OpenRouter overlay")?;
        let model = facts
            .wire_model
            .ok_or("no OpenRouter model resolved for DeepSeek Harness")?;
        // A clean-room acceptance run points dsh at a local mock; otherwise
        // the route's own base URL is the only source.
        let base_url = test_openrouter_base_url(ambient)
            .or_else(|| facts.base_url.map(str::to_string))
            .ok_or("no OpenRouter base URL on the resolved route")?;
        write_openrouter_patch(&openrouter_patch_path(home, model), model, &base_url)?;
    }
    Ok(())
}

/// Clean-room acceptance points a real dsh at a local mock. Honoured only
/// where the debug-only integration test vault is active, so a release build
/// can never be redirected.
pub const TEST_OPENROUTER_BASE_URL_ENV: &str = "CLUD_TEST_DSH_OPENROUTER_BASE_URL";

fn test_openrouter_base_url(ambient: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if !crate::provider_auth::test_vault_active() {
        return None;
    }
    ambient(TEST_OPENROUTER_BASE_URL_ENV).filter(|value| !value.is_empty())
}

fn lookup(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .rev()
        .find(|(candidate, _)| env_key_eq(candidate, key))
        .map(|(_, value)| value.clone())
}

fn set(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    env.retain(|(candidate, _)| !env_key_eq(candidate, key));
    env.push((key.to_string(), value.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_layout_is_versioned_under_clud_home() {
        let home = Path::new("/h");
        assert_eq!(
            managed_executable(home, false),
            Path::new("/h/.clud/harnesses/dsh")
                .join(DSH_VERSION)
                .join("node_modules/.bin/dsh")
        );
        assert!(managed_executable(home, true).ends_with("dsh.cmd"));
    }

    #[test]
    fn npm_argv_pins_dsh_and_private_node_into_the_prefix() {
        let argv = npm_install_argv("npm", Path::new("/p"));
        assert_eq!(argv[..4], ["npm", "install", "--prefix", "/p"]);
        assert!(argv.contains(&format!("@deepseek-ai/dsh@{DSH_VERSION}")));
        assert!(argv.contains(&format!("node@{NODE_VERSION}")));
        assert!(!argv.iter().any(|a| a == "-g" || a == "--global"));
    }

    #[test]
    fn only_a_managed_executable_gets_the_private_node_path() {
        let home = Path::new("/h");
        let managed = managed_executable(home, false);
        assert_eq!(
            managed_bin_for(&managed, home),
            Some(bin_dir(&managed_prefix(home)))
        );
        assert_eq!(managed_bin_for(Path::new("/usr/bin/dsh"), home), None);
    }

    #[test]
    fn prepend_path_puts_the_managed_bin_first() {
        let sep = if crate::platform::host::WINDOWS {
            ";"
        } else {
            ":"
        };
        assert_eq!(
            prepend_path(Path::new("/m"), Some("/usr/bin")),
            format!("/m{sep}/usr/bin")
        );
        assert_eq!(prepend_path(Path::new("/m"), None), "/m");
    }

    #[test]
    fn key_env_maps_only_passthrough_providers() {
        assert_eq!(
            key_env_for(ModelProvider::DeepSeek),
            Some("DEEPSEEK_API_KEY")
        );
        assert_eq!(
            key_env_for(ModelProvider::OpenRouter),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(key_env_for(ModelProvider::Claude), None);
    }

    #[test]
    fn openrouter_patch_selects_the_route_and_declares_the_model() {
        let yaml = openrouter_patch_yaml(
            "~anthropic/claude-sonnet-latest",
            "https://openrouter.ai/api",
        );
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("valid YAML");
        let entries = doc.as_sequence().unwrap();
        let pi = &entries[0]["config"]["providers"]["openrouter"];
        assert_eq!(entries[0]["id"], "llm-pi-ai");
        assert_eq!(pi["apiKeyEnv"], "OPENROUTER_API_KEY");
        assert_eq!(pi["api"], "anthropic-messages");
        assert_eq!(pi["baseURL"], "https://openrouter.ai/api");
        assert_eq!(pi["models"][0]["id"], "~anthropic/claude-sonnet-latest");
        assert_eq!(entries[1]["id"], "agent-default-model");
        assert_eq!(entries[1]["config"]["provider"], "openrouter");
        assert_eq!(
            entries[1]["config"]["model"],
            "~anthropic/claude-sonnet-latest"
        );
        assert!(!yaml.contains("sk-"), "the overlay never carries a key");
    }

    #[test]
    fn openrouter_patch_quotes_hostile_model_ids() {
        let yaml = openrouter_patch_yaml("a'b: c", "https://openrouter.ai/api");
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(doc[1]["config"]["model"], "a'b: c");
    }

    #[test]
    fn patch_path_is_per_model_and_filesystem_safe() {
        let path = openrouter_patch_path(Path::new("/h"), "~anthropic/claude-sonnet-latest");
        assert!(path.starts_with(managed_root(Path::new("/h")).join("patches")));
        assert_eq!(
            path.file_name().unwrap(),
            "openrouter-_anthropic_claude-sonnet-latest.yml"
        );
    }

    #[test]
    fn write_patch_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = openrouter_patch_path(dir.path(), "openai/gpt-5");
        write_openrouter_patch(&path, "openai/gpt-5", "https://openrouter.ai/api").unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("model: 'openai/gpt-5'"));
    }

    #[test]
    fn plan_patch_path_is_openrouter_only() {
        assert!(plan_patch_path(ModelProvider::DeepSeek, Some("x")).is_none());
        assert!(plan_patch_path(ModelProvider::OpenRouter, None).is_none());
    }

    fn facts(provider: ModelProvider, executable: &str) -> ChildFacts<'_> {
        ChildFacts {
            executable,
            provider,
            wire_model: Some("~anthropic/claude-sonnet-latest"),
            base_url: Some("https://openrouter.ai/api"),
        }
    }

    fn vault_with(key: &'static str) -> impl Fn(ModelProvider) -> Option<String> {
        move |provider| Some(format!("{key}-{provider:?}"))
    }

    #[test]
    fn deepseek_launch_gets_the_vault_key_and_no_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Vec::new();
        prepare_child(
            &facts(ModelProvider::DeepSeek, "/usr/bin/dsh"),
            Some(dir.path()),
            &mut env,
            &|_| None,
            &vault_with("v"),
        )
        .unwrap();
        assert_eq!(
            lookup(&env, "DEEPSEEK_API_KEY").as_deref(),
            Some("v-DeepSeek")
        );
        assert_eq!(lookup(&env, "OPENROUTER_API_KEY"), None);
        assert_eq!(
            lookup(&env, "PATH"),
            None,
            "a user-managed dsh keeps its PATH"
        );
        assert!(!managed_root(dir.path()).join("patches").exists());
    }

    #[test]
    fn openrouter_launch_gets_only_the_openrouter_key_and_its_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Vec::new();
        prepare_child(
            &facts(ModelProvider::OpenRouter, "/usr/bin/dsh"),
            Some(dir.path()),
            &mut env,
            &|_| None,
            &vault_with("v"),
        )
        .unwrap();
        assert_eq!(
            lookup(&env, "OPENROUTER_API_KEY").as_deref(),
            Some("v-OpenRouter")
        );
        assert_eq!(lookup(&env, "DEEPSEEK_API_KEY"), None);
        assert!(!env.iter().any(|(k, _)| k.starts_with("ANTHROPIC_")));
        let patch = openrouter_patch_path(dir.path(), "~anthropic/claude-sonnet-latest");
        let body = std::fs::read_to_string(patch).unwrap();
        assert!(body.contains("provider: openrouter"));
        assert!(body.contains("https://openrouter.ai/api"));
        assert!(!body.contains("v-OpenRouter"), "the key never reaches disk");
    }

    #[test]
    fn an_ambient_key_wins_over_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Vec::new();
        prepare_child(
            &facts(ModelProvider::DeepSeek, "/usr/bin/dsh"),
            Some(dir.path()),
            &mut env,
            &|name| (name == "DEEPSEEK_API_KEY").then(|| "ambient".to_string()),
            &vault_with("v"),
        )
        .unwrap();
        assert_eq!(
            lookup(&env, "DEEPSEEK_API_KEY"),
            None,
            "ambient value is inherited untouched"
        );
    }

    #[test]
    fn a_managed_dsh_runs_on_its_private_node() {
        let dir = tempfile::tempdir().unwrap();
        let exe = managed_executable(dir.path(), crate::platform::host::WINDOWS);
        let exe = exe.to_string_lossy().into_owned();
        let mut env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        prepare_child(
            &facts(ModelProvider::DeepSeek, &exe),
            Some(dir.path()),
            &mut env,
            &|_| None,
            &|_| None,
        )
        .unwrap();
        let path = lookup(&env, "PATH").unwrap();
        assert!(path.starts_with(&*bin_dir(&managed_prefix(dir.path())).to_string_lossy()));
        assert!(path.ends_with("/usr/bin"));
    }
}
