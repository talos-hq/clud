//! The one entry path for every `clud-shim` alias (#1546).
//!
//! This module alone owns the three decisions every shim shares:
//!
//! 1. **Session detection.** A session is valid when [`registry::ABI_KEY`]
//!    equals [`registry::SHIM_ABI`] and the shim's own keys are present and
//!    sane. A session built by a different clud version fails the stamp.
//! 2. **Target resolution.** A session target must be absolute, executable,
//!    outside every shim directory, and not a copy of this binary.
//! 3. **Fail-open passthrough.** Without a valid session a `Passthrough` shim
//!    execs the next same-named binary on PATH, printing nothing. The only
//!    failure is the shell's own `127 command not found`, when no real binary
//!    exists.
//!
//! Handlers in `shim_main.rs` receive a [`Session`] and never read a session
//! key, so none of them can fail closed for a session reason. The guard test
//! in `shim_main.rs` enforces that split.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::shim_registry::{self as registry, Fallback, ShimKind};

/// The personality's own name; `clud-shim --registry` prints the registry.
const SELF_NAME: &str = "clud-shim";

pub struct GhSession {
    pub target: PathBuf,
    /// `CLUD_EXE`, for the `pr checks --watch` and `run watch` upgrades.
    /// Without it a PR-checks watch runs unchanged and a run watch is
    /// refused.
    pub watcher: Option<PathBuf>,
    pub fail_fast: bool,
    /// #1743: the daemon read broker for `gh api` GETs, when the session
    /// enabled it (`git.gh_read_broker`). `None` runs every call on the
    /// real `gh`.
    pub read_broker: Option<crate::gh_broker::client::BrokerClient>,
    /// #2352: where calls go after the watch upgrades and a read-broker miss.
    pub route: GhRoute,
}

/// #2352: `gh.backend` resolved for this call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhRoute {
    /// The real gh, with the read broker when enabled.
    Gh,
    /// `ghd gh ARGS`, told where the real gh is.
    Ghd(PathBuf),
    /// `gh.backend` is `"ghd"` but no ghd is on PATH: fail, never fall back.
    GhdMissing,
}

/// The route for a backend and the ghd found on PATH, if any.
pub fn route_for(backend: crate::repo_clud_config::GhBackend, ghd: Option<PathBuf>) -> GhRoute {
    match (backend, ghd) {
        (crate::repo_clud_config::GhBackend::Gh, _) => GhRoute::Gh,
        (crate::repo_clud_config::GhBackend::Ghd, Some(path)) => GhRoute::Ghd(path),
        (crate::repo_clud_config::GhBackend::Ghd, None) => GhRoute::GhdMissing,
    }
}

/// `gh.backend` for the repo the call runs in (user settings, then the repo's).
fn gh_route(facts: &Facts) -> GhRoute {
    let backend = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::repo_clud_config::discover_effective_clud_config(&cwd))
        .map(|config| config.gh.backend)
        .unwrap_or_default();
    let ghd = (backend == crate::repo_clud_config::GhBackend::Ghd)
        .then(|| resolve_passthrough("ghd", facts))
        .flatten();
    route_for(backend, ghd)
}

pub enum Session {
    Python {
        target: PathBuf,
    },
    Gh(Box<GhSession>),
    /// #1486: the real binary behind the in-session clone/worktree policy.
    Git {
        target: PathBuf,
    },
    Rm,
}

/// Everything dispatch reads from the process, injectable for tests.
pub struct Facts<'a> {
    pub self_exe: PathBuf,
    pub path: OsString,
    pub home: Option<PathBuf>,
    pub var: &'a dyn Fn(&str) -> Option<OsString>,
}

pub fn run(argv: &[OsString]) -> i32 {
    let argv0 = argv.first().cloned().unwrap_or_default();
    let args = argv.get(1..).unwrap_or(&[]);
    let name = registry::invoked_name(&argv0);
    let is_self_name = name
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(SELF_NAME));
    if is_self_name && args == [OsString::from("--registry")] {
        println!("{}", registry_json());
        return 0;
    }
    let var = |key: &str| std::env::var_os(key);
    let home_key = if crate::platform::host::WINDOWS {
        "USERPROFILE"
    } else {
        "HOME"
    };
    let facts = Facts {
        self_exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from(&argv0)),
        path: std::env::var_os("PATH").unwrap_or_default(),
        home: std::env::var_os(home_key).map(PathBuf::from),
        var: &var,
    };
    match decide(&argv0, &facts) {
        Decision::Native(kind) => native(kind, args),
        Decision::Session(session) => handle(session, args),
        Decision::Passthrough(name) => match resolve_passthrough(&name, &facts) {
            Some(real) => super::exec(&name, &real, args),
            None => {
                eprintln!("{name}: command not found");
                127
            }
        },
    }
}

pub enum Decision {
    Native(ShimKind),
    Session(Session),
    Passthrough(String),
}

pub fn decide(argv0: &OsStr, facts: &Facts) -> Decision {
    let Some(spec) = registry::lookup(argv0) else {
        let name = registry::invoked_name(argv0).unwrap_or_default();
        return Decision::Passthrough(name);
    };
    if spec.fallback == Fallback::Native {
        return Decision::Native(spec.kind);
    }
    match session(spec.kind, facts) {
        Some(session) => Decision::Session(session),
        None => Decision::Passthrough(spec.name.to_string()),
    }
}

fn session(kind: ShimKind, facts: &Facts) -> Option<Session> {
    let var = facts.var;
    if var(registry::ABI_KEY).as_deref() != Some(OsStr::new(registry::SHIM_ABI)) {
        return None;
    }
    let session_dir = var(registry::SESSION_DIR_KEY)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && dir.is_dir());
    let dirs = registry::shim_dirs(
        &facts.self_exe,
        session_dir.as_deref(),
        facts.home.as_deref(),
    );
    let target = |key: &str| {
        var(key)
            .map(PathBuf::from)
            .filter(|path| registry::valid_target(path, &facts.self_exe, &dirs))
    };
    match kind {
        ShimKind::Python => {
            target(registry::PYTHON_TARGET_KEY).map(|target| Session::Python { target })
        }
        ShimKind::Gh => target(registry::GH_TARGET_KEY).map(|target| {
            Session::Gh(Box::new(GhSession {
                read_broker: read_broker(&target, facts),
                route: gh_route(facts),
                target,
                watcher: var(registry::CLUD_EXE_KEY)
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute() && path.is_file()),
                fail_fast: var(registry::GH_FAIL_FAST_KEY).as_deref() != Some(OsStr::new("0")),
            }))
        }),
        ShimKind::Git => target(registry::GIT_TARGET_KEY).map(|target| Session::Git { target }),
        // `--unsafe` opts this session out of clud's catastrophe floor. The
        // normal passthrough path resolves the real rm after the shim dir.
        ShimKind::Rm if var(crate::runner::UNSAFE_MODE_ENV).as_deref() == Some(OsStr::new("1")) => {
            None
        }
        ShimKind::Rm => session_dir.map(|_| Session::Rm),
        // Native: never validated here.
        ShimKind::SafeRm | ShimKind::SafeMktemp => None,
    }
}

/// #1743: the read-broker client for a session that enabled it. The state
/// dir follows the daemon's own rule: `CLUD_DAEMON_STATE_DIR`, else
/// `~/.clud/state`.
fn read_broker(
    target: &std::path::Path,
    facts: &Facts,
) -> Option<crate::gh_broker::client::BrokerClient> {
    let var = facts.var;
    let enabled = match var(registry::GH_READ_BROKER_KEY) {
        Some(value) => value == "1",
        // A session launched before the key existed (clud < 2.8.24) running
        // a refreshed alias: follow the current setting, default on, like
        // `shim_session::activate_rm` does at launch.
        None => facts.home.as_deref().is_none_or(|home| {
            crate::clud_settings::load_gh_read_broker_enabled_at(home).unwrap_or(true)
        }),
    };
    if !enabled {
        return None;
    }
    let state_dir = var(registry::DAEMON_STATE_DIR_KEY)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            facts
                .home
                .as_ref()
                .map(|home| home.join(".clud").join("state"))
        })?;
    let env = crate::gh_broker::FORWARDED_ENV
        .iter()
        .filter_map(|key| {
            let value = var(key)?.into_string().ok()?;
            Some((key.to_string(), value))
        })
        .collect();
    Some(crate::gh_broker::client::BrokerClient {
        state_dir,
        gh: target.to_path_buf(),
        env,
        session_id: var(registry::SESSION_ID_KEY).and_then(|id| id.into_string().ok()),
        fresh: var(registry::GH_FRESH_KEY).as_deref() == Some(OsStr::new("1")),
        gh_repo: var("GH_REPO").and_then(|repo| repo.into_string().ok()),
    })
}

/// The next real `name` on PATH after the shim directories.
pub fn resolve_passthrough(name: &str, facts: &Facts) -> Option<PathBuf> {
    let session_dir = (facts.var)(registry::SESSION_DIR_KEY).map(PathBuf::from);
    let dirs = registry::shim_dirs(
        &facts.self_exe,
        session_dir.as_deref(),
        facts.home.as_deref(),
    );
    registry::next_on_path(
        &registry::file_name(name),
        &facts.path,
        &facts.self_exe,
        &dirs,
        false,
    )
    .ok()
}

fn handle(session: Session, args: &[OsString]) -> i32 {
    match session {
        Session::Python { target } => super::python_shim::run(&target, args),
        // #2352: `gh.backend = "ghd"` is mandatory, so a missing ghd is a
        // missing command, never a silent fall back to the real gh.
        Session::Gh(gh) if gh.route == GhRoute::GhdMissing => {
            eprintln!(
                "gh: this repo sets gh.backend = \"ghd\", but ghd is not on PATH; \
                 install ghd (https://github.com/forge-ci/ghd) or unset gh.backend"
            );
            127
        }
        Session::Gh(gh) => super::gh_shim::run(&gh, args),
        Session::Git { target } => super::git_shim::run(&target, args),
        Session::Rm => super::rm_shim::run(args),
    }
}

fn native(kind: ShimKind, args: &[OsString]) -> i32 {
    match kind {
        ShimKind::SafeRm => super::safe_rm::run(args),
        ShimKind::SafeMktemp => crate::safe_mktemp::run(args),
        // A registry test keeps every other kind `Passthrough`.
        ShimKind::Python | ShimKind::Gh | ShimKind::Git | ShimKind::Rm => {
            unreachable!("{kind:?} is not a native shim")
        }
    }
}

fn registry_json() -> String {
    let shims: Vec<_> = registry::SHIMS
        .iter()
        .map(|spec| {
            serde_json::json!({
                "name": spec.name,
                "fallback": format!("{:?}", spec.fallback),
                "session_keys": spec.session_keys,
            })
        })
        .collect();
    serde_json::json!({
        "abi_key": registry::ABI_KEY,
        "abi": registry::SHIM_ABI,
        "session_dir_key": registry::SESSION_DIR_KEY,
        "shims": shims,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn gh_backend_ghd_routes_to_ghd_and_fails_closed_without_it() {
        use crate::repo_clud_config::GhBackend;
        let ghd = PathBuf::from("/opt/ghd");
        assert_eq!(route_for(GhBackend::Gh, Some(ghd.clone())), GhRoute::Gh);
        assert_eq!(
            route_for(GhBackend::Ghd, Some(ghd.clone())),
            GhRoute::Ghd(ghd)
        );
        // Mandatory: no silent fallback to the real gh.
        assert_eq!(route_for(GhBackend::Ghd, None), GhRoute::GhdMissing);
    }

    fn write_exe(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        crate::platform::shim_fs::set_mode(path, 0o755).unwrap();
    }

    struct World {
        _temp: TempDir,
        shim_dir: PathBuf,
        real_dir: PathBuf,
    }

    impl World {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let shim_dir = temp.path().join("shim");
            let real_dir = temp.path().join("real");
            for spec in registry::SHIMS {
                write_exe(&shim_dir.join(registry::file_name(spec.name)), b"shim");
                write_exe(&real_dir.join(registry::file_name(spec.name)), b"real");
            }
            World {
                _temp: temp,
                shim_dir,
                real_dir,
            }
        }

        fn decide(&self, name: &str, vars: &HashMap<&str, OsString>) -> Decision {
            self.decide_in_home(name, vars, None)
        }

        fn decide_in_home(
            &self,
            name: &str,
            vars: &HashMap<&str, OsString>,
            home: Option<&Path>,
        ) -> Decision {
            let var = |key: &str| vars.get(key).cloned();
            let facts = Facts {
                self_exe: self.shim_dir.join(registry::file_name(name)),
                path: std::env::join_paths([&self.shim_dir, &self.real_dir]).unwrap(),
                home: home.map(Path::to_path_buf),
                var: &var,
            };
            decide(OsStr::new(name), &facts)
        }

        fn real(&self, name: &str) -> PathBuf {
            self.real_dir.join(registry::file_name(name))
        }

        fn valid_session(&self) -> HashMap<&'static str, OsString> {
            HashMap::from([
                (registry::ABI_KEY, OsString::from(registry::SHIM_ABI)),
                (
                    registry::SESSION_DIR_KEY,
                    self.shim_dir.clone().into_os_string(),
                ),
                (
                    registry::PYTHON_TARGET_KEY,
                    self.real("python").into_os_string(),
                ),
                (registry::GH_TARGET_KEY, self.real("gh").into_os_string()),
                (registry::GIT_TARGET_KEY, self.real("git").into_os_string()),
            ])
        }
    }

    fn passthrough_name(decision: Decision) -> Option<String> {
        match decision {
            Decision::Passthrough(name) => Some(name),
            _ => None,
        }
    }

    #[test]
    fn every_passthrough_shim_passes_through_with_an_empty_session() {
        let world = World::new();
        for spec in registry::SHIMS {
            let decision = world.decide(spec.name, &HashMap::new());
            match spec.fallback {
                Fallback::Passthrough => {
                    assert_eq!(passthrough_name(decision).as_deref(), Some(spec.name));
                }
                Fallback::Native => {
                    assert!(matches!(decision, Decision::Native(_)), "{}", spec.name)
                }
            }
        }
    }

    #[test]
    fn every_passthrough_shim_runs_its_handler_in_a_valid_session() {
        let world = World::new();
        let vars = world.valid_session();
        for spec in registry::SHIMS
            .iter()
            .filter(|s| s.fallback == Fallback::Passthrough)
        {
            assert!(
                matches!(world.decide(spec.name, &vars), Decision::Session(_)),
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn a_stale_or_foreign_session_passes_through() {
        let world = World::new();
        let valid = world.valid_session();
        let mut cases: Vec<(&str, HashMap<&str, OsString>)> = Vec::new();
        let mut no_stamp = valid.clone();
        no_stamp.remove(registry::ABI_KEY);
        cases.push(("missing ABI stamp (older clud's session)", no_stamp));
        let mut other_abi = valid.clone();
        other_abi.insert(registry::ABI_KEY, OsString::from("0"));
        cases.push(("different ABI", other_abi));
        for (key, name) in [
            (registry::GH_TARGET_KEY, "gh"),
            (registry::GIT_TARGET_KEY, "git"),
            (registry::PYTHON_TARGET_KEY, "python"),
        ] {
            for (label, value) in [
                ("deleted", world.real_dir.join("deleted").into_os_string()),
                ("relative", OsString::from(registry::file_name(name))),
                (
                    "into the shim dir",
                    world
                        .shim_dir
                        .join(registry::file_name(name))
                        .into_os_string(),
                ),
            ] {
                let mut vars = valid.clone();
                vars.insert(key, value);
                let decision = world.decide(name, &vars);
                assert_eq!(passthrough_name(decision).as_deref(), Some(name), "{label}");
            }
        }
        let mut no_dir = valid.clone();
        no_dir.insert(
            registry::SESSION_DIR_KEY,
            world.real_dir.join("gone").into_os_string(),
        );
        assert_eq!(
            passthrough_name(world.decide("rm", &no_dir)).as_deref(),
            Some("rm")
        );
        for (label, vars) in cases {
            for spec in registry::SHIMS
                .iter()
                .filter(|s| s.fallback == Fallback::Passthrough)
            {
                let decision = world.decide(spec.name, &vars);
                assert_eq!(
                    passthrough_name(decision).as_deref(),
                    Some(spec.name),
                    "{label}"
                );
            }
        }
    }

    #[test]
    fn passthrough_resolves_the_real_binary_and_never_the_shim() {
        let world = World::new();
        let var = |_: &str| None;
        for spec in registry::SHIMS {
            let facts = Facts {
                self_exe: world.shim_dir.join(registry::file_name(spec.name)),
                path: std::env::join_paths([&world.shim_dir, &world.real_dir]).unwrap(),
                home: None,
                var: &var,
            };
            assert_eq!(
                resolve_passthrough(spec.name, &facts),
                Some(world.real(spec.name))
            );
            let alone = Facts {
                path: std::env::join_paths([&world.shim_dir]).unwrap(),
                ..facts
            };
            assert_eq!(
                resolve_passthrough(spec.name, &alone),
                None,
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn unsafe_session_passes_rm_through_but_safe_rm_stays_native() {
        let world = World::new();
        let mut vars = world.valid_session();
        assert!(matches!(
            world.decide("rm", &vars),
            Decision::Session(Session::Rm)
        ));
        vars.insert(crate::runner::UNSAFE_MODE_ENV, OsString::from("1"));
        assert!(matches!(
            world.decide("rm", &vars),
            Decision::Passthrough(_)
        ));
        assert!(matches!(
            world.decide("safe-rm", &vars),
            Decision::Native(ShimKind::SafeRm)
        ));
    }

    #[test]
    fn gh_watch_upgrade_needs_a_watcher_and_honors_the_fail_fast_opt_out() {
        let world = World::new();
        let mut vars = world.valid_session();
        let Decision::Session(Session::Gh(gh)) = world.decide("gh", &vars) else {
            panic!("valid gh session");
        };
        assert!(gh.watcher.is_none() && gh.fail_fast);
        vars.insert(registry::CLUD_EXE_KEY, world.real("gh").into_os_string());
        vars.insert(registry::GH_FAIL_FAST_KEY, OsString::from("0"));
        let Decision::Session(Session::Gh(gh)) = world.decide("gh", &vars) else {
            panic!("valid gh session");
        };
        assert_eq!(gh.watcher, Some(world.real("gh")));
        assert!(!gh.fail_fast);
    }

    #[test]
    fn gh_read_broker_needs_the_session_opt_in() {
        let world = World::new();
        let mut vars = world.valid_session();
        let Decision::Session(Session::Gh(gh)) = world.decide("gh", &vars) else {
            panic!("valid gh session");
        };
        assert!(
            gh.read_broker.is_none(),
            "no key, no home and no state dir: nowhere to find the daemon"
        );
        vars.insert(registry::GH_READ_BROKER_KEY, OsString::from("0"));
        let Decision::Session(Session::Gh(gh)) = world.decide("gh", &vars) else {
            panic!("valid gh session");
        };
        assert!(gh.read_broker.is_none());
        vars.insert(registry::GH_READ_BROKER_KEY, OsString::from("1"));
        vars.insert(registry::DAEMON_STATE_DIR_KEY, OsString::from("/state"));
        vars.insert("GH_TOKEN", OsString::from("t"));
        vars.insert("UNRELATED", OsString::from("x"));
        vars.insert(registry::GH_FRESH_KEY, OsString::from("1"));
        let Decision::Session(Session::Gh(gh)) = world.decide("gh", &vars) else {
            panic!("valid gh session");
        };
        let broker = gh.read_broker.expect("enabled");
        assert_eq!(broker.state_dir, PathBuf::from("/state"));
        assert_eq!(broker.gh, world.real("gh"));
        assert_eq!(broker.env, [("GH_TOKEN".to_string(), "t".to_string())]);
        assert!(broker.fresh);
        assert_eq!(broker.gh_repo, None);
    }

    /// A session launched before 2.8.24 has no `CLUD_GH_READ_BROKER`; once
    /// its shared alias is refreshed it follows the current setting instead
    /// of treating the missing key as off. An explicit `0` stays off.
    #[test]
    fn a_session_without_the_broker_key_follows_the_current_setting() {
        let world = World::new();
        let home = TempDir::new().unwrap();
        let broker = |vars: &HashMap<&str, OsString>| {
            let Decision::Session(Session::Gh(gh)) =
                world.decide_in_home("gh", vars, Some(home.path()))
            else {
                panic!("valid gh session");
            };
            gh.read_broker
        };
        let mut vars = world.valid_session();
        let enabled = broker(&vars).expect("the setting defaults to on");
        assert_eq!(enabled.state_dir, home.path().join(".clud").join("state"));
        assert_eq!(enabled.gh, world.real("gh"));

        crate::clud_settings::save_gh_read_broker_enabled_at(home.path(), false).unwrap();
        assert!(broker(&vars).is_none(), "the setting turned it off");

        crate::clud_settings::save_gh_read_broker_enabled_at(home.path(), true).unwrap();
        vars.insert(registry::GH_READ_BROKER_KEY, OsString::from("0"));
        assert!(
            broker(&vars).is_none(),
            "an explicit 0 wins over the setting"
        );
    }

    #[test]
    fn unregistered_names_pass_through_under_their_own_name() {
        let world = World::new();
        assert_eq!(
            passthrough_name(world.decide("node", &world.valid_session())).as_deref(),
            Some("node")
        );
    }
}
