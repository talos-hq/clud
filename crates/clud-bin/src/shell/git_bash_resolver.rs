//! Lazily fetch a pinned portable Git Bash bundle so callers can hand
//! `CLAUDE_CODE_GIT_BASH_PATH` to Claude Code without depending on a
//! system-wide Git for Windows install.
//!
//! Refs https://github.com/zackees/clud/issues/447.
//!
//! ## Layout
//!
//! Manifest: `crates/clud-bin/vendor/win32/git-bash-bin.toml`. The file is
//! hand-edited and parsed at runtime — embed it with `include_str!` so the
//! resolver works regardless of where the binary is launched from.
//!
//! Cache: `~/.clud/vendor/win32/git-bash-bin-<sha256[..12]>/` plus a sibling
//! `git-bash-bin-<sha256[..12]>.complete` sentinel that is only written after
//! the archive's bytes verify against the pinned sha256 and the zip extracts
//! cleanly. A partial extraction is never reused — the sentinel's presence is
//! the only "ready" signal.
//!
//! ## Failure modes
//!
//! - Offline / GitHub down: [`FetchError::Network`]. Callers should suggest
//!   the user set `CLAUDE_CODE_GIT_BASH_PATH` to a Git Bash already on disk.
//! - sha256 mismatch: [`FetchError::ChecksumMismatch`]. Surfaces both the
//!   expected and observed digest so the manifest can be bumped deliberately.
//! - Anti-virus quarantine of extracted DLLs: surfaced as
//!   [`FetchError::Io`]; the `clud-windows-trash` skill quarantine + retry
//!   pattern is the documented remediation.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use kernal_api::hash::Sha256Hasher as Sha256;
use serde::Deserialize;

/// Manifest TOML embedded at compile time. See module docs.
pub const EMBEDDED_MANIFEST_TOML: &str = include_str!("../../vendor/win32/git-bash-bin.toml");

/// Parsed shape of `crates/clud-bin/vendor/win32/git-bash-bin.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GitBashManifest {
    pub git_bash_bin: GitBashEntry,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GitBashEntry {
    pub url: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub upstream_repo: String,
    pub upstream_commit_sha: String,
    pub relative_bash_path: String,
}

/// Default manifest, parsed once from the embedded TOML.
pub fn embedded_manifest() -> Result<GitBashManifest, FetchError> {
    toml::from_str(EMBEDDED_MANIFEST_TOML).map_err(|error| FetchError::Manifest {
        message: format!("embedded git-bash-bin.toml malformed: {error}"),
    })
}

/// Vendor cache root under the user home: `<home>/.clud/vendor/win32/`.
pub fn vendor_cache_dir(home: &Path) -> PathBuf {
    home.join(".clud").join("vendor").join("win32")
}

/// Per-archive extraction root: `<vendor>/git-bash-bin-<sha[..12]>/`.
pub fn extraction_dir(home: &Path, sha256_hex: &str) -> PathBuf {
    let suffix = sha256_hex.get(..12).unwrap_or(sha256_hex);
    vendor_cache_dir(home).join(format!("git-bash-bin-{suffix}"))
}

/// Sibling sentinel file: presence means the extraction completed and
/// verified. A partial extraction never has it.
pub fn sentinel_path(home: &Path, sha256_hex: &str) -> PathBuf {
    let suffix = sha256_hex.get(..12).unwrap_or(sha256_hex);
    vendor_cache_dir(home).join(format!("git-bash-bin-{suffix}.complete"))
}

/// Absolute path the resolver returns once the bundle is ready.
pub fn bash_exe_path(home: &Path, manifest: &GitBashManifest) -> PathBuf {
    extraction_dir(home, &manifest.git_bash_bin.sha256)
        .join(&manifest.git_bash_bin.relative_bash_path)
}

/// Fetch + verify + extract, returning the resolved `bash.exe` path. No-ops
/// after the first successful run because the sentinel short-circuits.
pub fn resolve_or_fetch_git_bash(home: &Path) -> Result<PathBuf, FetchError> {
    let manifest = embedded_manifest()?;
    resolve_or_fetch_with(home, &manifest, &UreqFetcher)
}

/// Test-friendly variant: caller supplies the manifest and the fetcher so
/// integration tests can drive the resolver against a local fixture without
/// touching the network.
pub fn resolve_or_fetch_with(
    home: &Path,
    manifest: &GitBashManifest,
    fetcher: &dyn ArchiveFetcher,
) -> Result<PathBuf, FetchError> {
    let entry = &manifest.git_bash_bin;
    let sentinel = sentinel_path(home, &entry.sha256);
    if sentinel.exists() {
        return Ok(bash_exe_path(home, manifest));
    }

    fs::create_dir_all(vendor_cache_dir(home))?;

    let archive_bytes = fetcher.fetch(&entry.url)?;
    if archive_bytes.len() as u64 != entry.size_bytes {
        return Err(FetchError::SizeMismatch {
            expected: entry.size_bytes,
            observed: archive_bytes.len() as u64,
        });
    }

    let observed_hex = sha256_hex(&archive_bytes);
    if !observed_hex.eq_ignore_ascii_case(&entry.sha256) {
        return Err(FetchError::ChecksumMismatch {
            expected: entry.sha256.clone(),
            observed: observed_hex,
        });
    }

    let dest = extraction_dir(home, &entry.sha256);
    // Best-effort cleanup of a previous partial extraction. Ignore errors —
    // if the path doesn't exist this is a no-op, and if the OS refuses we
    // fall through to extract_zip which will surface its own error.
    let _ = fs::remove_dir_all(&dest);
    fs::create_dir_all(&dest)?;
    extract_zip(&archive_bytes, &dest)?;

    let resolved = dest.join(&entry.relative_bash_path);
    if !resolved.exists() {
        return Err(FetchError::EntryPointMissing {
            relative: entry.relative_bash_path.clone(),
            extraction_root: dest,
        });
    }

    // Write the sentinel last. Anything earlier and a crash would leave a
    // half-extracted dir advertised as ready.
    fs::write(&sentinel, observed_hex.as_bytes())?;
    Ok(resolved)
}

/// Abstraction over the network fetch so tests can substitute a fixture.
pub trait ArchiveFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, FetchError>;
}

/// Default fetcher backed by `ureq`. Times out at 60 s — the bundle is ~9 MB.
pub struct UreqFetcher;

impl ArchiveFetcher for UreqFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60))
            .build();
        let response = agent.get(url).call().map_err(|error| FetchError::Network {
            url: url.to_string(),
            message: error.to_string(),
        })?;
        let mut buf = Vec::new();
        response
            .into_reader()
            .take(64 * 1024 * 1024)
            .read_to_end(&mut buf)
            .map_err(FetchError::Io)?;
        Ok(buf)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{message}")]
    Manifest { message: String },
    #[error("fetch {url}: {message}")]
    Network { url: String, message: String },
    #[error("{0}")]
    Io(io::Error),
    #[error("zip: {0}")]
    Zip(String),
    #[error("git-bash-bin.zip size mismatch: expected {expected} bytes, observed {observed}")]
    SizeMismatch { expected: u64, observed: u64 },
    #[error("git-bash-bin.zip sha256 mismatch: expected {expected}, observed {observed}")]
    ChecksumMismatch { expected: String, observed: String },
    #[error("extracted git-bash-bin.zip is missing {relative} (looked under {})", extraction_root.display())]
    EntryPointMissing {
        relative: String,
        extraction_root: PathBuf,
    },
}

impl From<io::Error> for FetchError {
    fn from(error: io::Error) -> Self {
        FetchError::Io(error)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(hasher.finalize().as_bytes())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), FetchError> {
    let reader = io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|error| FetchError::Zip(error.to_string()))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| FetchError::Zip(error.to_string()))?;
        let raw_name = match entry.enclosed_name() {
            Some(name) => name.to_path_buf(),
            None => {
                return Err(FetchError::Zip(format!(
                    "rejected zip entry with unsafe path: {}",
                    entry
                        .name()
                        .map_or_else(|error| format!("<{error}>"), |name| name.into_owned())
                )));
            }
        };
        let target = dest.join(&raw_name);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&target)?;
        io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::tempdir;

    /// Synthetic in-memory fetcher that hands back fixed bytes. Lets tests
    /// drive the resolver without touching the network.
    struct StaticFetcher {
        bytes: Vec<u8>,
    }

    impl ArchiveFetcher for StaticFetcher {
        fn fetch(&self, _url: &str) -> Result<Vec<u8>, FetchError> {
            Ok(self.bytes.clone())
        }
    }

    fn build_fixture_zip() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(io::Cursor::new(&mut buf));
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("git-bash-bin/bash.exe", options).unwrap();
            writer.write_all(b"#!/fake/bash shim").unwrap();
            writer
                .start_file("git-bash-bin/msys-2.0.dll", options)
                .unwrap();
            writer.write_all(b"\x4dZ fake dll").unwrap();
            writer.finish().unwrap();
        }
        buf
    }

    fn fixture_manifest(bytes: &[u8]) -> GitBashManifest {
        GitBashManifest {
            git_bash_bin: GitBashEntry {
                url: "https://example.invalid/git-bash-bin.zip".to_string(),
                size_bytes: bytes.len() as u64,
                sha256: sha256_hex(bytes),
                upstream_repo: "test/fixture".to_string(),
                upstream_commit_sha: "fixture-commit".to_string(),
                relative_bash_path: "git-bash-bin/bash.exe".to_string(),
            },
        }
    }

    #[test]
    fn embedded_manifest_parses() {
        let manifest = embedded_manifest().expect("embedded manifest must parse");
        assert_eq!(manifest.git_bash_bin.size_bytes, 9_384_477);
        assert_eq!(
            manifest.git_bash_bin.relative_bash_path,
            "git-bash-bin/bash.exe"
        );
        assert_eq!(manifest.git_bash_bin.sha256.len(), 64, "sha256 hex length");
    }

    #[test]
    fn extraction_dir_uses_short_sha_suffix() {
        let home = tempdir().unwrap();
        let dir = extraction_dir(home.path(), "1234567890abcdef1234567890abcdef");
        let last = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(last, "git-bash-bin-1234567890ab");
    }

    #[test]
    fn fetch_extracts_and_writes_sentinel_when_checksum_matches() {
        let bytes = build_fixture_zip();
        let manifest = fixture_manifest(&bytes);
        let home = tempdir().unwrap();
        let fetcher = StaticFetcher {
            bytes: bytes.clone(),
        };

        let resolved =
            resolve_or_fetch_with(home.path(), &manifest, &fetcher).expect("fetch must succeed");
        assert!(resolved.exists(), "bash.exe must exist at: {resolved:?}");
        assert!(
            sentinel_path(home.path(), &manifest.git_bash_bin.sha256).exists(),
            "sentinel must be written after successful extract"
        );
        assert_eq!(resolved, bash_exe_path(home.path(), &manifest));
    }

    #[test]
    fn fetch_short_circuits_when_sentinel_exists() {
        let bytes = build_fixture_zip();
        let manifest = fixture_manifest(&bytes);
        let home = tempdir().unwrap();
        let fetcher = StaticFetcher { bytes };

        // First call extracts.
        resolve_or_fetch_with(home.path(), &manifest, &fetcher).unwrap();

        // Subsequent call must not need to extract again. Swap in a fetcher
        // that would fail if it were consulted.
        struct PanickyFetcher;
        impl ArchiveFetcher for PanickyFetcher {
            fn fetch(&self, _url: &str) -> Result<Vec<u8>, FetchError> {
                panic!("fetcher must not be consulted after sentinel exists");
            }
        }
        let resolved = resolve_or_fetch_with(home.path(), &manifest, &PanickyFetcher)
            .expect("warm path must not consult fetcher");
        assert_eq!(resolved, bash_exe_path(home.path(), &manifest));
    }

    #[test]
    fn checksum_mismatch_is_a_hard_error_and_leaves_no_sentinel() {
        let bytes = build_fixture_zip();
        let mut manifest = fixture_manifest(&bytes);
        // Flip a nibble in the expected hash.
        let mut bad = manifest.git_bash_bin.sha256.clone();
        bad.replace_range(0..1, "0");
        manifest.git_bash_bin.sha256 = bad;
        let home = tempdir().unwrap();
        let fetcher = StaticFetcher { bytes };

        let err = resolve_or_fetch_with(home.path(), &manifest, &fetcher)
            .expect_err("checksum mismatch must fail");
        match err {
            FetchError::ChecksumMismatch { .. } => {}
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
        assert!(
            !sentinel_path(home.path(), &manifest.git_bash_bin.sha256).exists(),
            "sentinel must not be written when checksum fails"
        );
    }

    #[test]
    fn size_mismatch_is_caught_before_checksum() {
        let bytes = build_fixture_zip();
        let mut manifest = fixture_manifest(&bytes);
        manifest.git_bash_bin.size_bytes += 1; // claim a different size
        let home = tempdir().unwrap();
        let fetcher = StaticFetcher { bytes };

        let err = resolve_or_fetch_with(home.path(), &manifest, &fetcher)
            .expect_err("size mismatch must fail");
        match err {
            FetchError::SizeMismatch { .. } => {}
            other => panic!("expected SizeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn missing_entry_point_inside_zip_is_reported() {
        let bytes = build_fixture_zip();
        let mut manifest = fixture_manifest(&bytes);
        manifest.git_bash_bin.relative_bash_path = "git-bash-bin/not-there.exe".to_string();
        let home = tempdir().unwrap();
        let fetcher = StaticFetcher { bytes };

        let err = resolve_or_fetch_with(home.path(), &manifest, &fetcher)
            .expect_err("missing entry point must fail");
        match err {
            FetchError::EntryPointMissing { relative, .. } => {
                assert_eq!(relative, "git-bash-bin/not-there.exe");
            }
            other => panic!("expected EntryPointMissing, got {other:?}"),
        }
        assert!(
            !sentinel_path(home.path(), &manifest.git_bash_bin.sha256).exists(),
            "sentinel must not be written when entry point is missing"
        );
    }
}
