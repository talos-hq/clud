//! Offline distribution of the one credential prompt and catalog executable.
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

struct Bundle<'a> {
    version: &'a str,
    digest: &'a str,
    bytes: &'a [u8],
}

fn bundled() -> Result<Bundle<'static>, String> {
    if BYTES.is_empty() {
        return Err("this development build has no bundled lokr; use a published native clud installer or prepare the pinned lokr bundle".into());
    }
    if !FALLBACK_BYTES.is_empty() && !fs_host::lokr_gnu_loader_available() {
        return Ok(Bundle {
            version: FALLBACK_VERSION,
            digest: FALLBACK_SHA256,
            bytes: FALLBACK_BYTES,
        });
    }
    Ok(Bundle {
        version: VERSION,
        digest: SHA256,
        bytes: BYTES,
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
        format!("bundled lokr {}{location}", bundle.version)
    } else {
        "lokr (development PATH lookup)".into()
    }
}

pub(crate) fn require_bundle() -> Result<(), String> {
    bundled().map(|_| ())
}

/// Called by native installation and lazily by wheel/direct-binary users. No network or UI.
pub(crate) fn ensure_installed() -> Result<PathBuf, String> {
    let home = crate::home::user_home().ok_or("home directory unavailable for lokr")?;
    install(&home, &bundled()?)
}

pub(crate) fn has_bundle() -> bool {
    !BYTES.is_empty()
}

fn install(home: &Path, bundle: &Bundle<'_>) -> Result<PathBuf, String> {
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
    file.take(bundle.bytes.len() as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if format!("{:x}", Sha256::digest(&bytes)) != bundle.digest {
        return Err("installed managed lokr digest mismatch; refusing to execute it".into());
    }
    fs_host::make_executable(&path)?;
    Ok(path)
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
    fn offline_install_is_verified_idempotent_and_detects_tampering() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let bytes = fake_native();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let bundle = Bundle {
            version: "0.1.0",
            digest: &digest,
            bytes: &bytes,
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
