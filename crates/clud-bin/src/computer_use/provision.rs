//! Find or install the pinned cua-driver.
//!
//! Resolution order, first match wins:
//!
//! 1. `CLUD_CUA_DRIVER` names an executable: used as-is, externally owned.
//! 2. clud's managed copy under `~/.clud/libexec/cua-driver/<version>-<sha>/`,
//!    re-verified against the digest recorded when it was unpacked.
//!
//! Another product's copy (for example one Hermes installed) is never
//! reused: only setup and install may read Hermes' tree or environment
//! (`setup_command::hermes`, #2609), and this runs on the MCP serve path.
//!
//! Otherwise [`ensure`] uses the embedded archive, the pinned PyPI wheel through
//! uv, or the GitHub release archive. It
//! checks its size and SHA-256 before unpacking anything, and publishes the
//! unpacked tree with one rename so concurrent first uses converge.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::pin::{self, ArchiveKind, Artifact};

/// Explicit driver override; an absolute path to an executable.
pub const OVERRIDE_ENV: &str = "CLUD_CUA_DRIVER";
const ARCHIVE: &[u8] = include_bytes!(env!("CLUD_BUNDLED_COMPUTER_USE_PATH"));
/// Records the archive digest and the unpacked binary's digest.
const MARKER: &str = ".clud-pin";

/// Where a resolved driver came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Override,
    Managed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Driver {
    pub path: PathBuf,
    pub source: Source,
}

/// The provisioning decision, before any download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Ready(Driver),
    /// The override names no executable file; never silently replaced.
    BadOverride(PathBuf),
    Missing,
}

/// The inputs to a resolution, so tests can supply every one.
pub struct Inputs<'a> {
    pub home: &'a Path,
    pub override_path: Option<PathBuf>,
    pub artifact: &'a Artifact,
}

impl Inputs<'_> {
    pub fn managed_dir(&self) -> PathBuf {
        managed_dir(self.home, self.artifact)
    }
}

pub fn managed_dir(home: &Path, artifact: &Artifact) -> PathBuf {
    home.join(".clud")
        .join("libexec")
        .join("cua-driver")
        .join(format!("{}-{}", pin::VERSION, &artifact.sha256[..16]))
}

/// Reads the process environment for the override.
pub fn host_inputs<'a>(home: &'a Path, artifact: &'a Artifact) -> Inputs<'a> {
    let override_path = std::env::var_os(OVERRIDE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    Inputs {
        home,
        override_path,
        artifact,
    }
}

pub fn resolve(inputs: &Inputs<'_>) -> Resolution {
    if let Some(path) = &inputs.override_path {
        return if path.is_file() {
            Resolution::Ready(Driver {
                path: path.clone(),
                source: Source::Override,
            })
        } else {
            Resolution::BadOverride(path.clone())
        };
    }
    if let Some(path) = verified_managed(&inputs.managed_dir(), inputs.artifact) {
        return Resolution::Ready(Driver {
            path,
            source: Source::Managed,
        });
    }
    Resolution::Missing
}

/// Resolves the driver, installing the pinned archive when it is missing.
pub fn embedded_archive_available() -> bool {
    !ARCHIVE.is_empty()
}

pub fn ensure(inputs: &Inputs<'_>) -> Result<Driver, String> {
    ensure_with_fetch(inputs, |artifact| {
        if embedded_archive_available() {
            Ok(ARCHIVE.to_vec())
        } else {
            super::uv_archive::read(inputs.home, artifact).or_else(|_| download(artifact))
        }
    })
}

pub fn ensure_with_fetch(
    inputs: &Inputs<'_>,
    fetch: impl FnOnce(&Artifact) -> Result<Vec<u8>, String>,
) -> Result<Driver, String> {
    match resolve(inputs) {
        Resolution::Ready(driver) => Ok(driver),
        Resolution::BadOverride(path) => Err(format!(
            "{OVERRIDE_ENV}={} is not an executable file; fix or unset it",
            path.display()
        )),
        Resolution::Missing => {
            let path = install(&inputs.managed_dir(), inputs.artifact, fetch)?;
            Ok(Driver {
                path,
                source: Source::Managed,
            })
        }
    }
}

fn file_sha256(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(format!("{:x}", hasher.finalize()))
}

/// The managed binary, when its marker names this pin and the binary still
/// hashes to the digest recorded at unpack time.
fn verified_managed(dir: &Path, artifact: &Artifact) -> Option<PathBuf> {
    let marker = std::fs::read_to_string(dir.join(MARKER)).ok()?;
    let mut lines = marker.lines();
    if lines.next()? != artifact.sha256 {
        return None;
    }
    let binary_digest = lines.next()?;
    let binary = dir.join(artifact.binary);
    (file_sha256(&binary)?.as_str() == binary_digest).then_some(binary)
}

/// Verifies `fetch`'s bytes against the pin, unpacks them and publishes the
/// tree at `dir`. A concurrent winner is accepted after the same verification.
pub fn install(
    dir: &Path,
    artifact: &Artifact,
    fetch: impl FnOnce(&Artifact) -> Result<Vec<u8>, String>,
) -> Result<PathBuf, String> {
    if let Some(path) = verified_managed(dir, artifact) {
        return Ok(path);
    }
    let bytes = fetch(artifact)?;
    if bytes.len() as u64 != artifact.size {
        return Err(format!(
            "cua-driver archive is {} bytes, pinned size is {}",
            bytes.len(),
            artifact.size
        ));
    }
    if format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256 {
        return Err("cua-driver archive digest mismatch; refusing to unpack it".into());
    }
    let parent = dir.parent().ok_or("cua-driver install dir has no parent")?;
    crate::platform::self_install::create_install_dir(parent)?;
    if dir.exists() {
        // An interrupted or tampered tree under the pinned name: it failed
        // verification above, so replace it rather than fail forever.
        std::fs::remove_dir_all(dir)
            .map_err(|error| format!("remove unverified {}: {error}", dir.display()))?;
    }
    let stage = tempfile::Builder::new()
        .prefix(".cua-driver-")
        .tempdir_in(parent)
        .map_err(|error| format!("create staging dir: {error}"))?;
    unpack(&bytes, artifact.kind, stage.path())?;
    let root = unpacked_root(stage.path(), artifact)?;
    let binary = root.join(artifact.binary);
    crate::platform::self_install::make_executable(&binary)?;
    let digest = file_sha256(&binary).ok_or("hash the unpacked cua-driver")?;
    std::fs::write(
        root.join(MARKER),
        format!("{}\n{digest}\n", artifact.sha256),
    )
    .map_err(|error| format!("write cua-driver marker: {error}"))?;
    if let Err(error) = std::fs::rename(&root, dir) {
        // Another process published first; its tree must verify too.
        return verified_managed(dir, artifact)
            .ok_or_else(|| format!("publish cua-driver at {}: {error}", dir.display()));
    }
    verified_managed(dir, artifact).ok_or_else(|| "installed cua-driver failed verification".into())
}

fn unpack(bytes: &[u8], kind: ArchiveKind, into: &Path) -> Result<(), String> {
    match kind {
        ArchiveKind::TarGz => {
            let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
            archive.set_preserve_permissions(true);
            archive
                .unpack(into)
                .map_err(|error| format!("unpack cua-driver archive: {error}"))
        }
        ArchiveKind::Zip => zip::ZipArchive::new(Cursor::new(bytes))
            .and_then(|mut archive| archive.extract(into))
            .map_err(|error| format!("unpack cua-driver archive: {error}")),
    }
}

/// The directory holding the binary: the staging dir itself, or the single
/// top-level directory some archives wrap their contents in.
fn unpacked_root(stage: &Path, artifact: &Artifact) -> Result<PathBuf, String> {
    if stage.join(artifact.binary).is_file() {
        return Ok(stage.to_path_buf());
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(stage)
        .map_err(|error| error.to_string())?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    match entries.as_slice() {
        [only] if only.is_dir() && only.join(artifact.binary).is_file() => Ok(only.clone()),
        _ => Err(format!(
            "cua-driver archive has no {} executable",
            artifact.binary
        )),
    }
}

/// Only the pinned release URL may start a download; redirects may stay on
/// GitHub's release asset CDN.
pub fn validate_location(value: &str, initial: bool) -> Result<(), String> {
    let parsed = url::Url::parse(value).map_err(|_| "invalid cua-driver download URL")?;
    let release = parsed.host_str() == Some("github.com")
        && parsed.path().starts_with(&format!(
            "/talos-hq/computer-use/releases/download/computer-use-v{}/",
            pin::VERSION
        ));
    let cdn = matches!(
        parsed.host_str(),
        Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com")
    );
    let clean = parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none()
        && parsed.fragment().is_none();
    let allowed = if initial {
        release && parsed.query().is_none()
    } else {
        release || cdn
    };
    if clean && allowed {
        Ok(())
    } else {
        Err("unapproved cua-driver download URL".into())
    }
}

fn download(artifact: &Artifact) -> Result<Vec<u8>, String> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(60))
        .build();
    let mut location = artifact.url();
    validate_location(&location, true)?;
    for _ in 0..5 {
        let response = match agent.get(&location).call() {
            Ok(value) | Err(ureq::Error::Status(_, value)) => value,
            Err(error) => return Err(format!("cua-driver download failed: {error}")),
        };
        if (300..400).contains(&response.status()) {
            let next = response
                .header("Location")
                .ok_or("cua-driver redirect has no Location")?;
            validate_location(next, false)?;
            location = next.to_owned();
            continue;
        }
        if response.status() != 200 {
            return Err(format!(
                "cua-driver download returned HTTP {}",
                response.status()
            ));
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(artifact.size + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read cua-driver download: {error}"))?;
        return Ok(bytes);
    }
    Err("too many cua-driver release redirects".into())
}

#[cfg(test)]
mod embedded_tests {
    use super::*;

    #[test]
    fn embedded_archive_provisions_without_network() {
        if ARCHIVE.is_empty() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let artifact = pin::for_host().expect("embedded archive requires a supported host");
        let driver = ensure(&Inputs {
            home: home.path(),
            artifact,
            override_path: None,
        })
        .unwrap();
        assert_eq!(driver.source, Source::Managed);
        assert!(driver.path.is_file());
    }
}
