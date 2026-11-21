//! The environment cua-driver runs with: permission mode, privacy defaults,
//! credential scrubbing and the Linux display repairs.
//!
//! Everything here is a pure function of an environment snapshot plus two
//! filesystem probes, so the policy is unit-tested on every host.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Carries the launch's policy to `clud computer-use mcp`.
pub const MODE_ENV: &str = "CLUD_CUA_PERMISSION_MODE";
/// `0` keeps cua-driver on XWayland only under a Wayland session.
pub const NATIVE_WAYLAND_ENV: &str = "CLUD_CUA_NATIVE_WAYLAND";

const CUA_MODE: &str = "CUA_DRIVER_PERMISSION_MODE";
const CUA_BYPASS: &str = "CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS";
const CUA_TELEMETRY: &str = "CUA_DRIVER_RS_TELEMETRY_ENABLED";
const CUA_UPDATE_CHECK: &str = "CUA_DRIVER_RS_UPDATE_CHECK";
const CUA_WAYLAND: &str = "CUA_DRIVER_RS_ENABLE_WAYLAND";

/// cua-driver's authorization profile, fixed for the driver's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// clud's default (YOLO) launch: cua-driver's approval checks are off.
    Unrestricted,
    /// `--safe`: cua-driver's own standard profile; it denies, never prompts.
    Standard,
}

impl PermissionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::Standard => "standard",
        }
    }

    /// The launch's choice: `--safe` keeps cua-driver's standard profile.
    pub const fn for_launch(safe: bool) -> Self {
        if safe {
            Self::Standard
        } else {
            Self::Unrestricted
        }
    }

    /// Reads [`MODE_ENV`]; anything but `unrestricted` is standard, so a
    /// missing or garbled value can only narrow.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value {
            Some("unrestricted") => Self::Unrestricted,
            _ => Self::Standard,
        }
    }
}

/// The graphical session cua-driver will see on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayKind {
    X11,
    /// Wayland compositor; `xwayland` when `DISPLAY` is also exported.
    Wayland {
        xwayland: bool,
    },
    /// No `DISPLAY` and no `WAYLAND_DISPLAY`.
    Headless,
    /// macOS or Windows: the OS has one desktop API.
    Native,
}

fn get<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

fn set(env: &mut Vec<(String, String)>, name: &str, value: &str) {
    env.retain(|(key, _)| key != name);
    env.push((name.to_string(), value.to_string()));
}

pub fn display_kind(env: &[(String, String)], linux: bool) -> DisplayKind {
    if !linux {
        return DisplayKind::Native;
    }
    let x11 = get(env, "DISPLAY").is_some();
    let wayland =
        get(env, "WAYLAND_DISPLAY").is_some() || get(env, "XDG_SESSION_TYPE") == Some("wayland");
    match (wayland, x11) {
        (true, xwayland) => DisplayKind::Wayland { xwayland },
        (false, true) => DisplayKind::X11,
        (false, false) => DisplayKind::Headless,
    }
}

/// Names that carry credentials; a third-party driver never inherits them.
pub fn is_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "API_KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
        "PRIVATE_KEY",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

/// An Xauthority cookie file for the live X server, when the inherited
/// `XAUTHORITY` is missing or stale.
///
/// A long-lived terminal keeps the `XAUTHORITY` of the login that started it;
/// after a re-login (KDE writes `xauth_XXXXXX`, mutter `.mutter-Xwaylandauth.*`
/// under `XDG_RUNTIME_DIR`) the old path is gone and every X11 call fails with
/// "Authorization required", which cua-driver reports as zero windows.
pub fn repaired_xauthority(
    env: &[(String, String)],
    exists: &dyn Fn(&Path) -> bool,
    list_runtime_dir: &dyn Fn(&Path) -> Vec<(PathBuf, SystemTime)>,
) -> Option<PathBuf> {
    get(env, "DISPLAY")?;
    if let Some(current) = get(env, "XAUTHORITY") {
        if exists(Path::new(current)) {
            return None;
        }
    } else if get(env, "HOME").is_some_and(|home| exists(&Path::new(home).join(".Xauthority"))) {
        return None;
    }
    let runtime = PathBuf::from(get(env, "XDG_RUNTIME_DIR")?);
    list_runtime_dir(&runtime)
        .into_iter()
        .filter(|(path, _)| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("xauth_") || name.starts_with(".mutter-Xwaylandauth.")
                })
        })
        .max_by_key(|(_, modified)| *modified)
        .map(|(path, _)| path)
}

/// Facts about the filesystem the child environment depends on.
pub struct Probes<'a> {
    pub exists: &'a dyn Fn(&Path) -> bool,
    pub list_runtime_dir: &'a dyn Fn(&Path) -> Vec<(PathBuf, SystemTime)>,
}

/// The full environment for a cua-driver child: the parent's environment
/// without credentials, plus clud's policy.
pub fn child_env(
    parent: Vec<(String, String)>,
    mode: PermissionMode,
    linux: bool,
    probes: &Probes<'_>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = parent
        .into_iter()
        .filter(|(key, _)| !is_credential(key))
        .collect();
    match mode {
        PermissionMode::Unrestricted => {
            set(&mut env, CUA_MODE, "unrestricted");
            set(&mut env, CUA_BYPASS, "1");
        }
        PermissionMode::Standard => {
            env.retain(|(key, _)| key != CUA_MODE && key != CUA_BYPASS);
        }
    }
    if get(&env, CUA_TELEMETRY).is_none() {
        set(&mut env, CUA_TELEMETRY, "0");
    }
    // clud owns the version pin; the driver must not nag about or fetch others.
    set(&mut env, CUA_UPDATE_CHECK, "0");
    if linux {
        apply_linux_display(&mut env, probes);
    }
    env
}

fn apply_linux_display(env: &mut Vec<(String, String)>, probes: &Probes<'_>) {
    if let Some(path) = repaired_xauthority(env, probes.exists, probes.list_runtime_dir) {
        set(env, "XAUTHORITY", &path.to_string_lossy());
    }
    let wayland = matches!(display_kind(env, true), DisplayKind::Wayland { .. });
    if wayland && get(env, NATIVE_WAYLAND_ENV) != Some("0") && get(env, CUA_WAYLAND).is_none() {
        set(env, CUA_WAYLAND, "1");
    }
}

/// The real filesystem behind [`Probes`].
pub fn host_exists(path: &Path) -> bool {
    path.exists()
}

pub fn host_list_runtime_dir(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| {
                (
                    entry.path(),
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                )
            })
        })
        .collect()
}

pub fn host_probes() -> Probes<'static> {
    Probes {
        exists: &host_exists,
        list_runtime_dir: &host_list_runtime_dir,
    }
}
