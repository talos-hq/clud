//! Locate the pinned wheel's archive through clud's managed uv runtime.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::pin::{self, Artifact};

pub(super) fn read(home: &Path, artifact: &Artifact) -> Result<Vec<u8>, String> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    let state_root = home.join(".clud").join("state");
    let uv = crate::shim_uv::discover_uv(&path_env, &state_root).ok_or("uv is unavailable")?;
    let output = crate::subprocess::capture_separate_with_env(
        uv_args(uv),
        Duration::from_secs(90),
        Some(
            std::env::vars()
                .filter(|(key, _)| {
                    let upper = key.to_ascii_uppercase();
                    !super::session_env::is_credential(key)
                        && !["UV_", "PIP_", "PYTHON", "VIRTUAL_ENV"]
                            .iter()
                            .any(|prefix| upper.starts_with(prefix))
                })
                .chain(std::iter::once(("PYTHONIOENCODING".into(), "utf-8".into())))
                .collect(),
        ),
    )?;
    if output.exit != 0 {
        return Err(format!("uv tool exited with status {}", output.exit));
    }
    read_archive(&output.stdout, artifact)
}

fn read_archive(stdout: &[u8], artifact: &Artifact) -> Result<Vec<u8>, String> {
    let path = Path::new(
        std::str::from_utf8(stdout)
            .map_err(|error| error.to_string())?
            .trim_end_matches(['\r', '\n']),
    );
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some(artifact.asset)
    {
        return Err("uv tool did not report the pinned archive path".into());
    }
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(artifact.size + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

fn uv_args(uv: PathBuf) -> Vec<String> {
    [
        uv.to_string_lossy().into_owned(),
        "tool".into(),
        "run".into(),
        "--no-config".into(),
        "--isolated".into(),
        "--index-url".into(),
        "https://pypi.org/simple".into(),
        "--only-binary".into(),
        ":all:".into(),
        "--python".into(),
        ">=3.12".into(),
        "--from".into(),
        format!("clud-computer-use=={}", pin::VERSION),
        "clud-computer-use-archive".into(),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use super::{pin, read_archive};

    #[test]
    fn uv_archive_must_name_the_pinned_asset_and_has_a_bounded_read() {
        let temp = tempfile::tempdir().unwrap();
        let mut artifact = pin::ARTIFACTS[0];
        artifact.size = 4;
        let path = temp.path().join(artifact.asset);
        std::fs::write(&path, b"123456789").unwrap();

        assert_eq!(
            read_archive(format!("{}\r\n", path.display()).as_bytes(), &artifact).unwrap(),
            b"12345",
        );
        assert!(read_archive(artifact.asset.as_bytes(), &artifact).is_err());
    }
}
