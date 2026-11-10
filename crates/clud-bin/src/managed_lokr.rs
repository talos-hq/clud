//! Verified distribution of the one credential prompt and catalog executable.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::platform::{host, self_install as fs_host};

const BYTES: &[u8] = include_bytes!(env!("CLUD_BUNDLED_LOKR_PATH"));
const VERSION: &str = env!("CLUD_BUNDLED_LOKR_VERSION");
const SHA256: &str = env!("CLUD_BUNDLED_LOKR_SHA256");
const FALLBACK_BYTES: &[u8] = include_bytes!(env!("CLUD_BUNDLED_LOKR_FALLBACK_PATH"));
const FALLBACK_VERSION: &str = env!("CLUD_BUNDLED_LOKR_FALLBACK_VERSION");
const FALLBACK_SHA256: &str = env!("CLUD_BUNDLED_LOKR_FALLBACK_SHA256");
const URL: &str = env!("CLUD_BUNDLED_LOKR_URL");
const SIZE: &str = env!("CLUD_BUNDLED_LOKR_SIZE");
const FALLBACK_URL: &str = env!("CLUD_BUNDLED_LOKR_FALLBACK_URL");
const FALLBACK_SIZE: &str = env!("CLUD_BUNDLED_LOKR_FALLBACK_SIZE");
const MAX_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Remote<'a> {
    url: &'a str,
    size: usize,
}

#[derive(Clone, Copy)]
struct Bundle<'a> {
    version: &'a str,
    digest: &'a str,
    bytes: &'a [u8],
    remote: Option<Remote<'a>>,
}

fn bundled() -> Result<Bundle<'static>, String> {
    if VERSION.is_empty() {
        return Err("this development build has no pinned lokr; use a published clud installer or prepare the lokr distribution".into());
    }
    let (version, digest, bytes, url, size) =
        if !FALLBACK_VERSION.is_empty() && !fs_host::lokr_gnu_loader_available() {
            (
                FALLBACK_VERSION,
                FALLBACK_SHA256,
                FALLBACK_BYTES,
                FALLBACK_URL,
                FALLBACK_SIZE,
            )
        } else {
            (VERSION, SHA256, BYTES, URL, SIZE)
        };
    let remote = if bytes.is_empty() {
        let size = size
            .parse::<usize>()
            .map_err(|_| "invalid pinned lokr size")?;
        if size == 0 || size > MAX_BYTES {
            return Err("invalid pinned lokr size".into());
        }
        validate_location(url, version, true)?;
        Some(Remote { url, size })
    } else {
        None
    };
    Ok(Bundle {
        version,
        digest,
        bytes,
        remote,
    })
}

fn destination(home: &Path, bundle: &Bundle<'_>) -> Result<PathBuf, String> {
    if bundle.version.is_empty()
        || !bundle
            .version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        || bundle.version.starts_with("0.0.")
        || bundle.digest.len() != 64
        || !bundle.digest.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("invalid functional lokr bundle identity".into());
    }
    Ok(home
        .join(".clud")
        .join("libexec")
        .join("lokr")
        .join(format!("{}-{}", bundle.version, bundle.digest))
        .join(if host::WINDOWS { "lokr.exe" } else { "lokr" }))
}

/// This metadata-only preview never extracts or executes the prompt.
pub(crate) fn preview() -> String {
    if let Ok(bundle) = bundled() {
        let location = crate::home::user_home()
            .and_then(|home| destination(&home, &bundle).ok())
            .map_or_else(String::new, |path| format!(" at {}", path.display()));
        let kind = if bundle.remote.is_some() {
            "pinned"
        } else {
            "bundled"
        };
        format!("{kind} lokr {}{location}", bundle.version)
    } else {
        "lokr (development PATH lookup)".into()
    }
}

pub(crate) fn require_distribution() -> Result<(), String> {
    bundled().map(|_| ())
}

/// Native binaries extract offline; release wheels fetch pinned bytes once. Never opens UI.
pub(crate) fn ensure_installed() -> Result<PathBuf, String> {
    let home = crate::home::user_home().ok_or("home directory unavailable for lokr")?;
    install(&home, &bundled()?)
}

pub(crate) fn has_distribution() -> bool {
    !VERSION.is_empty()
}

fn install(home: &Path, bundle: &Bundle<'_>) -> Result<PathBuf, String> {
    install_with_fetch(home, bundle, |remote| download(remote, bundle.version))
}

fn install_with_fetch<'a>(
    home: &Path,
    bundle: &Bundle<'a>,
    fetch: impl FnOnce(Remote<'a>) -> Result<Vec<u8>, String>,
) -> Result<PathBuf, String> {
    if let Some(remote) = bundle.remote {
        if remote.size == 0 || remote.size > MAX_BYTES {
            return Err("invalid pinned lokr size".into());
        }
        validate_location(remote.url, bundle.version, true)?;
        let path = destination(home, bundle)?;
        crate::self_install::transaction::inspect_destination(&path, false)?;
        if path.exists() {
            return verify_installed(path, bundle);
        }
        let bytes = fetch(remote)?;
        if bytes.len() != remote.size {
            return Err("downloaded lokr size mismatch".into());
        }
        let materialized = Bundle {
            bytes: &bytes,
            remote: None,
            ..*bundle
        };
        return install_embedded(home, &materialized);
    }
    install_embedded(home, bundle)
}

fn install_embedded(home: &Path, bundle: &Bundle<'_>) -> Result<PathBuf, String> {
    if format!("{:x}", Sha256::digest(bundle.bytes)) != bundle.digest {
        return Err("bundled lokr digest mismatch".into());
    }
    let path = destination(home, bundle)?;
    crate::self_install::transaction::inspect_destination(&path, false)?;
    let parent = path.parent().ok_or("missing lokr install parent")?;
    fs_host::create_install_dir(parent)?;
    crate::self_install::transaction::inspect_destination(&path, false)?;
    if path.exists() {
        return verify_installed(path, bundle);
    }
    let mut stage = tempfile::Builder::new()
        .prefix(".lokr-")
        .tempfile_in(parent)
        .map_err(|error| error.to_string())?;
    stage
        .write_all(bundle.bytes)
        .and_then(|()| stage.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    crate::self_install::transaction::validate_host_executable(stage.as_file_mut())?;
    fs_host::make_executable(stage.path())?;
    match stage.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.to_string()),
    }
    // Concurrent first use can publish the same immutable bundle first. Reuse
    // it only after the same path-safety and digest checks as an existing copy.
    verify_installed(path, bundle)
}

fn verify_installed(path: PathBuf, bundle: &Bundle<'_>) -> Result<PathBuf, String> {
    crate::self_install::transaction::inspect_destination(&path, false)?;
    let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    let size = bundle
        .remote
        .map_or(bundle.bytes.len(), |remote| remote.size);
    file.take(size as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if format!("{:x}", Sha256::digest(&bytes)) != bundle.digest {
        return Err("installed managed lokr digest mismatch; refusing to execute it".into());
    }
    fs_host::make_executable(&path)?;
    Ok(path)
}

fn validate_location(value: &str, version: &str, initial: bool) -> Result<(), String> {
    let parsed = url::Url::parse(value).map_err(|_| "invalid lokr download URL")?;
    let github = parsed.host_str() == Some("github.com")
        && parsed.path().starts_with(&format!(
            "/forge-ci/lokr/releases/download/v{version}/lokr-"
        ));
    let cdn = matches!(
        parsed.host_str(),
        Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com")
    );
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.fragment().is_some()
        || (initial && parsed.query().is_some())
        || (!github && (initial || !cdn))
    {
        return Err("unapproved lokr download URL".into());
    }
    Ok(())
}

fn download(remote: Remote<'_>, version: &str) -> Result<Vec<u8>, String> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let mut location = remote.url.to_owned();
    for hop in 0..=4 {
        let response = match agent.get(&location).call() {
            Ok(value) | Err(ureq::Error::Status(_, value)) => value,
            Err(_) => return Err("pinned lokr download failed".into()),
        };
        if (300..400).contains(&response.status()) {
            if hop == 4 {
                return Err("too many lokr release redirects".into());
            }
            let next = response
                .header("Location")
                .ok_or("lokr redirect has no Location")?;
            // The initial URL has already been checked against the exact pinned
            // version. Redirects can only stay on the release host or its CDN.
            validate_location(next, version, false)?;
            location = next.to_owned();
            continue;
        }
        if response.status() != 200 {
            return Err(format!("lokr download returned HTTP {}", response.status()));
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(remote.size as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "read pinned lokr download failed")?;
        return Ok(bytes);
    }
    Err("too many lokr release redirects".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tampered_bundle_never_installs_or_executes() {
        let home = tempfile::tempdir().unwrap();
        let bundle = Bundle {
            version: "0.1.0",
            digest: &"0".repeat(64),
            bytes: b"fake",
            remote: None,
        };
        assert!(install(home.path(), &bundle)
            .unwrap_err()
            .contains("digest mismatch"));
        assert!(!home.path().join(".clud").exists());
    }

    #[test]
    fn placeholder_and_unsafe_versions_cannot_choose_an_install_path() {
        let digest = "0".repeat(64);
        for version in ["0.0.1", "../escape", ""] {
            let bundle = Bundle {
                version,
                digest: &digest,
                bytes: b"fake",
                remote: None,
            };
            assert!(destination(Path::new("home"), &bundle).is_err());
        }
    }

    fn fake_native() -> Vec<u8> {
        let mut bytes = vec![0; 512];
        if host::WINDOWS {
            bytes[..2].copy_from_slice(b"MZ");
            bytes[60..64].copy_from_slice(&128_u32.to_le_bytes());
            bytes[128..132].copy_from_slice(b"PE\0\0");
            let machine: u16 = if host::AARCH64 { 0xaa64 } else { 0x8664 };
            bytes[132..134].copy_from_slice(&machine.to_le_bytes());
        } else if host::MACOS {
            bytes[..4].copy_from_slice(b"\xcf\xfa\xed\xfe");
            let cpu: u32 = if host::AARCH64 {
                0x0100_000c
            } else {
                0x0100_0007
            };
            bytes[4..8].copy_from_slice(&cpu.to_le_bytes());
        } else {
            bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
            let machine: u16 = if host::AARCH64 { 183 } else { 62 };
            bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn concurrent_first_use_reuses_the_verified_winner() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let bytes = fake_native();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let bundle = Bundle {
            version: "0.1.0",
            digest: &digest,
            bytes: &bytes,
            remote: None,
        };
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        install(&home, &bundle)
                    })
                })
                .collect();
            for worker in workers {
                let path = worker.join().unwrap().unwrap();
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
        });
    }

    #[test]
    fn wheel_download_is_verified_once_and_reused_offline() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let bytes = fake_native();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let bundle = Bundle {
            version: "0.1.0",
            digest: &digest,
            bytes: &[],
            remote: Some(Remote {
                url: "https://github.com/forge-ci/lokr/releases/download/v0.1.0/lokr-test",
                size: bytes.len(),
            }),
        };
        let path = install_with_fetch(&home, &bundle, |remote| {
            assert_eq!(remote.size, bytes.len());
            Ok(bytes.clone())
        })
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            install_with_fetch(&home, &bundle, |_| panic!(
                "cached helper must stay offline"
            ))
            .unwrap(),
            path
        );
        std::fs::write(&path, b"tampered").unwrap();
        assert!(
            install_with_fetch(&home, &bundle, |_| panic!("never replace a tampered cache"))
                .unwrap_err()
                .contains("digest mismatch")
        );
    }

    #[test]
    fn wheel_download_rejects_wrong_size_or_digest_before_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let bytes = fake_native();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let bundle = Bundle {
            version: "0.1.0",
            digest: &digest,
            bytes: &[],
            remote: Some(Remote {
                url: "https://github.com/forge-ci/lokr/releases/download/v0.1.0/lokr-test",
                size: bytes.len(),
            }),
        };
        let target = destination(temporary.path(), &bundle).unwrap();
        for payload in [vec![0; bytes.len() + 1], vec![0; bytes.len()]] {
            assert!(install_with_fetch(temporary.path(), &bundle, |_| Ok(payload)).is_err());
            assert!(!target.exists());
        }
    }

    #[test]
    fn wheel_download_locations_are_closed_to_other_origins() {
        for value in [
            "http://github.com/forge-ci/lokr/releases/download/v0.1.0/lokr-test",
            "https://github.com/another/repo/releases/download/v0.1.0/lokr-test",
            "https://example.invalid/lokr-test",
            "https://user@github.com/forge-ci/lokr/releases/download/v0.1.0/lokr-test",
        ] {
            assert!(validate_location(value, "0.1.0", true).is_err());
            assert!(validate_location(value, "0.1.0", false).is_err());
        }
        let cdn = "https://release-assets.githubusercontent.com/asset?signature=fixture";
        assert!(validate_location(cdn, "0.1.0", true).is_err());
        assert!(validate_location(cdn, "0.1.0", false).is_ok());
    }

    #[test]
    fn offline_install_is_verified_idempotent_and_detects_tampering() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let bytes = fake_native();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let bundle = Bundle {
            version: "0.1.0",
            digest: &digest,
            bytes: &bytes,
            remote: None,
        };
        let path = install(&home, &bundle).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(install(&home, &bundle).unwrap(), path);
        std::fs::write(&path, b"tampered").unwrap();
        assert!(install(&home, &bundle)
            .unwrap_err()
            .contains("digest mismatch"));
    }
}
