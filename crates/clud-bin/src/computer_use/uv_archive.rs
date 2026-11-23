//! Install the pinned wheel through clud's uv runtime and read its archive.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::pin::{self, Artifact};

pub(super) fn read(home: &Path, artifact: &Artifact) -> Result<Vec<u8>, String> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    let state = home.join(".clud").join("state");
    let uv = crate::shim_uv::discover_uv(&path_env, &state).ok_or("uv is unavailable")?;
    let install = state.join("computer-use").join(pin::VERSION);
    let _lock = crate::shim_uv::acquire_install_lock(&install).map_err(|e| e.to_string())?;
    let tools = install.join("tools");
    let environment = tools.join("clud-computer-use");
    if let Some(path) = archive_path(&environment, artifact, crate::platform::host::WINDOWS) {
        return read_bounded(&path, artifact.size);
    }

    let output = crate::subprocess::capture_separate_with_env(
        uv_args(uv),
        Duration::from_secs(90),
        Some(uv_env(&tools, &install.join("bin"))),
    )?;
    if output.exit != 0 {
        return Err(format!("uv tool exited with status {}", output.exit));
    }
    let path = archive_path(&environment, artifact, crate::platform::host::WINDOWS)
        .ok_or("uv tool did not install the pinned archive")?;
    read_bounded(&path, artifact.size)
}

fn archive_path(environment: &Path, artifact: &Artifact, windows: bool) -> Option<PathBuf> {
    let relative = Path::new("site-packages")
        .join("clud_computer_use")
        .join(artifact.asset);
    if windows {
        let path = environment.join("Lib").join(relative);
        return path.is_file().then_some(path);
    }
    std::fs::read_dir(environment.join("lib"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("python"))
        .map(|entry| entry.path().join(&relative))
        .find(|path| path.is_file())
}

fn read_bounded(path: &Path, size: u64) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(size + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

fn uv_env(tools: &Path, scripts: &Path) -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| {
            let upper = key.to_ascii_uppercase();
            !super::session_env::is_credential(key)
                && !["UV_", "PIP_", "PYTHON", "VIRTUAL_ENV"]
                    .iter()
                    .any(|prefix| upper.starts_with(prefix))
        })
        .chain([
            ("UV_TOOL_DIR".into(), tools.to_string_lossy().into_owned()),
            (
                "UV_TOOL_BIN_DIR".into(),
                scripts.to_string_lossy().into_owned(),
            ),
        ])
        .collect()
}

fn uv_args(uv: PathBuf) -> Vec<String> {
    [
        uv.to_string_lossy().into_owned(),
        "tool".into(),
        "install".into(),
        "--no-config".into(),
        "--force".into(),
        "--index-url".into(),
        "https://pypi.org/simple".into(),
        "--no-build".into(),
        "--python".into(),
        ">=3.12".into(),
        format!("clud-computer-use=={}", pin::VERSION),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use super::{archive_path, pin, read_bounded};

    #[test]
    fn uv_archive_is_read_from_an_installed_tool_without_running_it() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = &pin::ARTIFACTS[0];
        // python-name-lint: allow-next-line — uv's on-disk venv layout.
        for (windows, parent) in [(false, "lib/python3.12"), (true, "Lib")] {
            let site = temp
                .path()
                .join(parent)
                .join("site-packages/clud_computer_use");
            std::fs::create_dir_all(&site).unwrap();
            let path = site.join(artifact.asset);
            std::fs::write(&path, b"123456789").unwrap();
            assert_eq!(
                archive_path(temp.path(), artifact, windows),
                Some(path.clone())
            );
            assert_eq!(read_bounded(&path, 4).unwrap(), b"12345");
        }
    }
}
