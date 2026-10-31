//! Consented, verified installation of one native executable.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt;
use running_process::ReadStatus;
use sha2::{Digest, Sha256};

use super::activation::{self, ActivationPlan};
use super::catalog::{Arch, Flavor, MediaType, Os, ResolvedAsset};
use super::entry::InstallIntent;
use crate::platform::host;
use crate::platform::self_install as fs_host;

const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_WHEEL_BYTES: u64 = 384 * 1024 * 1024;

#[derive(Debug)]
pub struct InstallPlan {
    pub destination: PathBuf,
    pub version: String,
    pub source_label: String,
    pub source_sha256: String,
    pub replacement: Option<String>,
    pub path_proposal: String,
    pub activation: Option<ActivationPlan>,
    source: Source,
}

#[derive(Debug)]
enum Source {
    Current {
        path: PathBuf,
        file: File,
        fingerprint: Fingerprint,
    },
    Published(ResolvedAsset),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
    identity: fs_host::FileIdentity,
}

impl Fingerprint {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            identity: fs_host::file_identity(metadata),
        }
    }
}

pub fn plan(intent: InstallIntent, modify_path: bool) -> Result<InstallPlan, String> {
    let destination = approved_destination()?;
    inspect_destination(&destination, false)?;
    let backup = destination.with_extension("clud-backup");
    let recovering =
        backup.exists() && (!destination.exists() || run_version(&destination).is_err());
    if recovering {
        inspect_destination(&backup, true)?;
    } else {
        inspect_destination(&destination, true)?;
    }
    let replacement = if recovering {
        Some(hash_path(&backup, MAX_BINARY_BYTES)?.0)
    } else if destination.exists() {
        Some(hash_path(&destination, MAX_BINARY_BYTES)?.0)
    } else {
        None
    };
    let activation = modify_path
        .then(|| activation::plan(&destination))
        .transpose()?;
    let path_proposal = activation.as_ref().map_or_else(
        || "none; --no-modify-path preserves profiles and User PATH".to_owned(),
        ActivationPlan::description,
    );
    match intent {
        InstallIntent::CurrentExecutable => {
            let path = std::env::current_exe().map_err(|error| error.to_string())?;
            let mut file =
                File::open(&path).map_err(|error| format!("open running executable: {error}"))?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if !metadata.is_file() || metadata.len() > MAX_BINARY_BYTES {
                return Err("running executable is not a bounded regular file".into());
            }
            let fingerprint = Fingerprint::of(&metadata);
            validate_native_header(&mut file, current_os(), current_arch(), Flavor::Native)?;
            let (sha, _) = hash_reader(&mut file, MAX_BINARY_BYTES)?;
            file.seek(SeekFrom::Start(0))
                .map_err(|error| error.to_string())?;
            let version = env!("CARGO_PKG_VERSION").to_owned();
            verify_version(&path, &version)?;
            Ok(InstallPlan {
                destination,
                version,
                source_label: path.display().to_string(),
                source_sha256: sha,
                replacement,
                path_proposal,
                activation,
                source: Source::Current {
                    path,
                    file,
                    fingerprint,
                },
            })
        }
        InstallIntent::Published(asset) => {
            validate_asset_url(&asset)?;
            if asset.os != current_os() || asset.arch != current_arch() {
                return Err("selected asset does not match this host".into());
            }
            if asset.size_bytes > MAX_WHEEL_BYTES {
                return Err("selected asset is too large".into());
            }
            Ok(InstallPlan {
                destination,
                version: asset.version.clone(),
                source_label: asset.url.clone(),
                source_sha256: asset.sha256.clone(),
                replacement,
                path_proposal,
                activation,
                source: Source::Published(asset),
            })
        }
    }
}

impl InstallPlan {
    pub fn description(&self) -> String {
        format!(
            "Selected version: {}\nSource: {}\nSHA-256: {}\nDestination: {}\nReplacement: {}\nPATH/profile proposal: {}",
            self.version,
            self.source_label,
            self.source_sha256,
            self.destination.display(),
            self.replacement.as_deref().unwrap_or("none; create new executable"),
            self.path_proposal
        )
    }
}

/// A committed binary is still pending fresh name-based lookup by #1495.
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub fn execute(mut plan: InstallPlan) -> Result<(), String> {
    let parent = plan.destination.parent().ok_or("missing install parent")?;
    inspect_destination(&plan.destination, false)?;
    fs_host::create_install_dir(parent)?;
    inspect_destination(&plan.destination, false)?;
    let lock_path = parent.join(".clud-install.lock");
    if host::WINDOWS {
        if let Ok(meta) = fs::symlink_metadata(&lock_path) {
            if fs_host::is_reparse_point(&meta) {
                return Err("installer lock is a reparse point".into());
            }
        }
    }
    let mut lock_options = OpenOptions::new();
    lock_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    fs_host::restrict_lock_options(&mut lock_options);
    let lock = lock_options
        .open(&lock_path)
        .map_err(|error| format!("open install lock: {error}"))?;
    let lock_meta = lock.metadata().map_err(|error| error.to_string())?;
    if !lock_meta.is_file() {
        return Err("installer lock is not a regular file".into());
    }
    if !fs_host::owned_exclusively(&lock_meta) {
        return Err("installer lock is not exclusively owned by this user".into());
    }
    if fs_host::is_reparse_point(&lock_meta) {
        return Err("installer lock is a reparse point".into());
    }
    lock.lock_exclusive()
        .map_err(|error| format!("lock installer: {error}"))?;
    inspect_destination(&plan.destination, false)?;
    recover(&plan.destination)?;
    inspect_destination(&plan.destination, true)?;
    if plan.destination.exists() {
        let current = hash_path(&plan.destination, MAX_BINARY_BYTES)?.0;
        if plan.replacement.as_deref() != Some(current.as_str()) {
            return Err("installed executable changed after consent".into());
        }
    } else if plan.replacement.is_some() {
        return Err("installed executable disappeared after consent".into());
    }
    let suffix = if host::WINDOWS { ".exe" } else { ".bin" };
    let mut stage = tempfile::Builder::new()
        .prefix(".clud-install-")
        .suffix(suffix)
        .tempfile_in(parent)
        .map_err(|error| format!("create private stage: {error}"))?;
    match &mut plan.source {
        Source::Current {
            path,
            file,
            fingerprint,
        } => {
            file.seek(SeekFrom::Start(0))
                .map_err(|error| error.to_string())?;
            io::copy(&mut file.take(MAX_BINARY_BYTES + 1), stage.as_file_mut())
                .map_err(|error| format!("copy running executable: {error}"))?;
            if stage
                .as_file()
                .metadata()
                .map_err(|error| error.to_string())?
                .len()
                != fingerprint.len
            {
                return Err("running executable changed during copy".into());
            }
            if Fingerprint::of(&fs::metadata(&*path).map_err(|error| error.to_string())?)
                != *fingerprint
            {
                return Err("running executable identity changed after consent".into());
            }
            if hash_path(path, MAX_BINARY_BYTES)?.0 != plan.source_sha256 {
                return Err("running executable digest changed after consent".into());
            }
        }
        Source::Published(asset) => stage_published(asset, stage.as_file_mut())?,
    }
    stage
        .as_file_mut()
        .flush()
        .map_err(|error| error.to_string())?;
    stage
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    let stage_path = stage.path().to_path_buf();
    let (stage_digest, _) = hash_path(&stage_path, MAX_BINARY_BYTES)?;
    match &plan.source {
        Source::Current { .. } if stage_digest != plan.source_sha256 => {
            return Err("staged executable digest differs from source".into());
        }
        Source::Published(asset)
            if asset.media_type == MediaType::Direct && stage_digest != asset.sha256 =>
        {
            return Err("staged executable digest differs from selected release".into());
        }
        _ => {}
    }
    let flavor = match &plan.source {
        Source::Current { .. } => Flavor::Native,
        Source::Published(asset) => asset.flavor,
    };
    validate_native_header(stage.as_file_mut(), current_os(), current_arch(), flavor)?;
    make_executable(&stage_path)?;
    let stage = stage.into_temp_path();
    verify_version(&stage_path, &plan.version)?;
    commit(&plan.destination, &stage, &plan.version, &stage_digest)?;
    Ok(())
}

fn approved_destination() -> Result<PathBuf, String> {
    fs_host::approved_destination()
}

pub(crate) fn inspect_destination(destination: &Path, probe_version: bool) -> Result<(), String> {
    let parent = destination.parent().ok_or("missing install directory")?;
    let home = if host::UNIX {
        Some(
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or("HOME is unavailable")?,
        )
    } else {
        None
    };
    let mut path = PathBuf::new();
    for part in parent.components() {
        path.push(part);
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(format!("unsafe install directory: {}", path.display()));
            }
            if home.as_ref().is_some_and(|home| path.starts_with(home))
                && !fs_host::user_controlled_dir(&meta)
            {
                return Err(format!(
                    "install directory is not exclusively user-controlled: {}",
                    path.display()
                ));
            }
            if fs_host::is_reparse_point(&meta) {
                return Err(format!(
                    "reparse-point install directory: {}",
                    path.display()
                ));
            }
        }
    }
    if let Ok(meta) = fs::symlink_metadata(destination) {
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err("installed path is not an owned regular file".into());
        }
        if !fs_host::owned_exclusively(&meta) {
            return Err("installed executable is not exclusively owned by this user".into());
        }
        if fs_host::is_reparse_point(&meta) {
            return Err("installed executable is a reparse point".into());
        }
        if probe_version {
            let output = run_version(destination)?;
            if !output.starts_with("clud ") {
                return Err("existing destination is not a clud executable".into());
            }
        }
    }
    Ok(())
}

fn hash_path(path: &Path, limit: u64) -> Result<(String, u64), String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    hash_reader(&mut file, limit)
}

fn hash_reader(reader: &mut impl Read, limit: u64) -> Result<(String, u64), String> {
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .ok_or("executable too large")?;
        if count > limit {
            return Err("executable exceeds size limit".into());
        }
        hasher.update(&buffer[..read]);
    }
    Ok((format!("{:x}", hasher.finalize()), count))
}

fn current_os() -> Os {
    if host::WINDOWS {
        Os::Windows
    } else if host::MACOS {
        Os::Darwin
    } else {
        Os::Linux
    }
}

fn current_arch() -> Arch {
    if host::AARCH64 {
        Arch::Aarch64
    } else {
        Arch::X86_64
    }
}

fn validate_native_header(
    file: &mut File,
    os: Os,
    arch: Arch,
    flavor: Flavor,
) -> Result<(), String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut header = [0_u8; 512];
    let count = file.read(&mut header).map_err(|error| error.to_string())?;
    let data = &header[..count];
    match os {
        Os::Linux => {
            if data.len() < 64 || &data[..6] != b"\x7fELF\x02\x01" {
                return Err("expected ELF64 little-endian executable".into());
            }
            let machine = u16::from_le_bytes([data[18], data[19]]);
            if machine != if arch == Arch::Aarch64 { 183 } else { 62 } {
                return Err("ELF architecture mismatch".into());
            }
            if flavor == Flavor::StaticMusl {
                // Full program-header and dynamic-table checks are in catalog.rs.
                super::catalog::verify_static_elf_file(file, arch)?;
            }
        }
        Os::Darwin => {
            if data.len() < 8 || &data[..4] != b"\xcf\xfa\xed\xfe" {
                return Err("expected Mach-O 64-bit executable".into());
            }
            let cpu = u32::from_le_bytes(data[4..8].try_into().map_err(|_| "short Mach-O header")?);
            if cpu
                != if arch == Arch::Aarch64 {
                    0x0100_000c
                } else {
                    0x0100_0007
                }
            {
                return Err("Mach-O architecture mismatch".into());
            }
        }
        Os::Windows => {
            if data.len() < 64 || &data[..2] != b"MZ" {
                return Err("expected PE executable".into());
            }
            let offset = u32::from_le_bytes(data[60..64].try_into().map_err(|_| "short PE header")?)
                as usize;
            if offset.checked_add(6).is_none_or(|end| end > data.len())
                || &data[offset..offset + 4] != b"PE\0\0"
            {
                return Err("invalid PE header".into());
            }
            let machine = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
            if machine
                != if arch == Arch::Aarch64 {
                    0xaa64
                } else {
                    0x8664
                }
            {
                return Err("PE architecture mismatch".into());
            }
        }
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn run_version(path: &Path) -> Result<String, String> {
    let command = vec![path.to_string_lossy().to_string(), "--version".to_owned()];
    let process =
        crate::subprocess::ManagedSubprocess::start_inheriting_env(command, None, true, None)
            .map_err(|error| format!("execute {}: {error}", path.display()))?;
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            let _ = process.kill();
            return Err("executable version probe timed out".into());
        }
        match process.read_stdout(Some(Duration::from_millis(100))) {
            ReadStatus::Line(line) => output.extend_from_slice(&line),
            ReadStatus::Timeout => {
                let _ = process.poll();
            }
            ReadStatus::Eof => break,
        }
        if output.len() > 4096 {
            return Err("version output too long".into());
        }
    }
    let code = process
        .wait(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    if code != 0 {
        return Err(format!("{} --version failed with {code}", path.display()));
    }
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

fn verify_version(path: &Path, expected: &str) -> Result<(), String> {
    let version = run_version(path)?;
    if version != format!("clud {expected}") {
        return Err(format!(
            "executable version mismatch: expected clud {expected}, got {version}"
        ));
    }
    Ok(())
}

fn validate_asset_url(asset: &ResolvedAsset) -> Result<(), String> {
    if !super::catalog::is_release_asset_url(&asset.url, &asset.version, &asset.filename) {
        return Err("release URL changed after catalog resolution".into());
    }
    Ok(())
}

fn stage_published(asset: &ResolvedAsset, target: &mut File) -> Result<(), String> {
    validate_asset_url(asset)?;
    if asset.media_type == MediaType::Direct {
        let current = std::env::current_exe().map_err(|error| error.to_string())?;
        #[cfg(feature = "installer-ci-fixture")]
        let force_download =
            std::env::var_os("CLUD_INSTALLER_CI_FORCE_DOWNLOAD").is_some_and(|value| value == "1");
        #[cfg(not(feature = "installer-ci-fixture"))]
        let force_download = false;
        if !force_download && current_matches_asset(&current, asset)? {
            let mut source = File::open(current).map_err(|error| error.to_string())?;
            io::copy(&mut source, target).map_err(|error| error.to_string())?;
            return Ok(());
        }
    }
    #[cfg(feature = "installer-ci-fixture")]
    if let Some(directory) = std::env::var_os("CLUD_INSTALLER_CI_FIXTURE_DIR") {
        let path = PathBuf::from(directory)
            .join("assets")
            .join(&asset.filename);
        let mut source = File::open(&path)
            .map_err(|error| format!("candidate asset {}: {error}", path.display()))?;
        return consume_published_reader(&mut source, target, asset);
    }
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(30))
        .build();
    let mut url = asset.url.clone();
    let mut response = None;
    for hop in 0..=4 {
        let value = match agent.get(&url).call() {
            Ok(value) | Err(ureq::Error::Status(_, value)) => value,
            Err(error) => return Err(format!("release download failed: {error}")),
        };
        if (300..400).contains(&value.status()) {
            if hop == 4 {
                return Err("too many release redirects".into());
            }
            let next = value
                .header("Location")
                .ok_or("release redirect has no Location")?;
            validate_redirect(next)?;
            url = next.to_owned();
            continue;
        }
        if value.status() != 200 {
            return Err(format!("release download returned HTTP {}", value.status()));
        }
        response = Some(value);
        break;
    }
    let response = response.ok_or("too many release redirects")?;
    let mut reader = response.into_reader();
    consume_published_reader(&mut reader, target, asset)
}

fn consume_published_reader(
    reader: &mut impl Read,
    target: &mut File,
    asset: &ResolvedAsset,
) -> Result<(), String> {
    if asset.media_type == MediaType::Direct {
        verify_direct_reader(reader, target, asset)?;
    } else {
        let mut wheel = tempfile::tempfile().map_err(|error| error.to_string())?;
        io::copy(&mut reader.take(asset.size_bytes + 1), &mut wheel)
            .map_err(|error| error.to_string())?;
        wheel
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        let (digest, size) = hash_reader(&mut wheel, MAX_WHEEL_BYTES)?;
        if size != asset.size_bytes || digest != asset.sha256 {
            return Err("wheel size or digest mismatch".into());
        }
        wheel
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        extract_wheel(wheel, target, asset)?;
    }
    Ok(())
}

fn current_matches_asset(path: &Path, asset: &ResolvedAsset) -> Result<bool, String> {
    if asset.media_type != MediaType::Direct {
        return Ok(false);
    }
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() != asset.size_bytes {
        return Ok(false);
    }
    Ok(hash_path(path, MAX_BINARY_BYTES)?.0 == asset.sha256)
}

fn verify_direct_reader(
    reader: &mut impl Read,
    target: &mut File,
    asset: &ResolvedAsset,
) -> Result<(), String> {
    io::copy(&mut reader.take(asset.size_bytes + 1), target).map_err(|error| error.to_string())?;
    target.flush().map_err(|error| error.to_string())?;
    target
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let (digest, size) = hash_reader(target, MAX_BINARY_BYTES)?;
    if size != asset.size_bytes || digest != asset.sha256 {
        return Err("release size or digest mismatch".into());
    }
    Ok(())
}

fn validate_redirect(value: &str) -> Result<(), String> {
    let parsed = url::Url::parse(value).map_err(|error| error.to_string())?;
    if parsed.scheme() != "https"
        || parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err("unsafe release redirect".into());
    }
    // github.com itself only for the hop from the pre-transfer owner to the
    // repository's new home (#2126); the asset bytes still come from the CDN.
    let moved_repository = parsed.host_str() == Some("github.com")
        && parsed
            .path()
            .starts_with("/talos-hq/clud/releases/download/");
    if !moved_repository
        && !matches!(
            parsed.host_str(),
            Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com")
        )
    {
        return Err("release redirect left the approved CDN".into());
    }
    Ok(())
}

fn extract_wheel(wheel: File, target: &mut File, asset: &ResolvedAsset) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(wheel).map_err(|error| error.to_string())?;
    if archive.len() > 10_000 {
        return Err("wheel has too many entries".into());
    }
    let expected = if asset.os == Os::Windows {
        "clud.exe"
    } else {
        "clud"
    };
    let mut found = false;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry
            .name()
            .map_err(|error| error.to_string())?
            .into_owned();
        let path = Path::new(&name);
        if name.contains(['\\', ':', '\0'])
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err("unsafe wheel entry path".into());
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err("wheel contains a symlink".into());
        }
        if path.file_name().is_some_and(|name| name == expected) {
            if found {
                return Err("wheel has duplicate clud executables".into());
            }
            found = true;
            if entry.size() > MAX_BINARY_BYTES {
                return Err("wheel executable is too large".into());
            }
            io::copy(&mut entry.by_ref().take(MAX_BINARY_BYTES + 1), target)
                .map_err(|error| error.to_string())?;
        }
    }
    if !found {
        return Err("wheel lacks a clud executable".into());
    }
    Ok(())
}

fn make_executable(path: &Path) -> Result<(), String> {
    fs_host::make_executable(path)
}

pub(crate) fn recover(destination: &Path) -> Result<(), String> {
    let backup = destination.with_extension("clud-backup");
    let metadata = match fs::symlink_metadata(&backup) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect prior backup: {error}")),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("installer backup is not a regular file".into());
    }
    if !fs_host::owned_exclusively(&metadata) {
        return Err("installer backup is not exclusively owned by this user".into());
    }
    if fs_host::is_reparse_point(&metadata) {
        return Err("installer backup is a reparse point".into());
    }
    let mut file = File::open(&backup).map_err(|error| error.to_string())?;
    validate_native_header(&mut file, current_os(), current_arch(), Flavor::Native)?;
    drop(file);
    if destination.exists() && run_version(destination).is_ok() {
        fs::remove_file(&backup).map_err(|error| format!("clear old backup: {error}"))?;
    } else {
        if host::WINDOWS && destination.exists() {
            fs::remove_file(destination).map_err(|error| error.to_string())?;
        }
        rename_when_released(&backup, destination)
            .map_err(|error| format!("restore prior clud: {error}"))?;
    }
    Ok(())
}

/// How long [`rename_when_released`] waits for another process to let go of
/// a just-written executable.
const RELEASE_WAIT: Duration = Duration::from_secs(5);

/// `fs::rename`, retried briefly while Windows reports the file in use.
///
/// A freshly written `.exe` is routinely held open for a moment by an
/// antivirus scan (or the version probe's image still being unmapped), and
/// the rename then fails with ERROR_SHARING_VIOLATION (32), or with
/// ERROR_ACCESS_DENIED (5) for a delete-pending name. Both clear on their own,
/// so wait up to [`RELEASE_WAIT`]; every other error, and every error off
/// Windows, returns at once.
fn rename_when_released(from: &Path, to: &Path) -> io::Result<()> {
    let deadline = Instant::now() + RELEASE_WAIT;
    loop {
        match fs::rename(from, to) {
            Err(error)
                if host::WINDOWS
                    && matches!(error.raw_os_error(), Some(32 | 5))
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

fn commit(destination: &Path, stage: &Path, version: &str, digest: &str) -> Result<(), String> {
    let backup = destination.with_extension("clud-backup");
    if destination.exists() {
        let mut source = File::open(destination).map_err(|error| error.to_string())?;
        let mut saved = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
            .map_err(|error| format!("create prior backup: {error}"))?;
        io::copy(&mut source, &mut saved).map_err(|error| format!("backup prior clud: {error}"))?;
        drop(source);
        make_executable(&backup)?;
        saved.sync_all().map_err(|error| error.to_string())?;
        drop(saved);
        if host::WINDOWS {
            if let Err(error) = fs::remove_file(destination) {
                let _ = fs::remove_file(&backup);
                return Err(format!("prior clud is in use; it was preserved: {error}"));
            }
        }
    }
    if let Err(error) = rename_when_released(stage, destination) {
        if !destination.exists() && backup.exists() {
            let _ = fs::rename(&backup, destination);
        } else if backup.exists() {
            let _ = fs::remove_file(&backup);
        }
        return Err(format!("activate staged clud: {error}"));
    }
    let verified = hash_path(destination, MAX_BINARY_BYTES)
        .and_then(|(actual, _)| {
            if actual == digest {
                Ok(())
            } else {
                Err("committed executable digest changed".into())
            }
        })
        .and_then(|()| verify_version(destination, version));
    if let Err(error) = verified {
        if backup.exists() {
            if host::WINDOWS {
                fs::remove_file(destination).map_err(|failure| failure.to_string())?;
            }
            fs::rename(&backup, destination).map_err(|failure| failure.to_string())?;
        } else {
            fs::remove_file(destination).map_err(|failure| failure.to_string())?;
        }
        return Err(error);
    }
    if backup.exists() {
        fs::remove_file(backup).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_asset(bytes: &[u8]) -> ResolvedAsset {
        ResolvedAsset {
            version: "2.8.14".into(),
            filename: "clud-2.8.14-x86_64-unknown-linux-musl".into(),
            media_type: MediaType::Direct,
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
            url: "https://github.com/zackees/clud/releases/download/2.8.14/clud-2.8.14-x86_64-unknown-linux-musl".into(),
            flavor: Flavor::StaticMusl,
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    #[test]
    fn exact_release_body_rejects_truncation_and_changed_digest() {
        let bytes = b"published native bytes";
        let asset = direct_asset(bytes);
        let mut complete = tempfile::tempfile().unwrap();
        verify_direct_reader(&mut &bytes[..], &mut complete, &asset).unwrap();
        let mut truncated = tempfile::tempfile().unwrap();
        assert!(verify_direct_reader(&mut &bytes[..5], &mut truncated, &asset).is_err());
        let mut altered = tempfile::tempfile().unwrap();
        assert!(
            verify_direct_reader(&mut &b"published native bytez"[..], &mut altered, &asset)
                .is_err()
        );
    }

    #[test]
    fn exact_digest_decides_release_reuse() {
        let bytes = b"published native bytes";
        let asset = direct_asset(bytes);
        let mut local = tempfile::NamedTempFile::new().unwrap();
        local.write_all(bytes).unwrap();
        assert!(current_matches_asset(local.path(), &asset).unwrap());
        local.as_file_mut().set_len(0).unwrap();
        local.write_all(b"different bytes").unwrap();
        assert!(!current_matches_asset(local.path(), &asset).unwrap());
    }

    #[test]
    fn unsafe_redirects_are_rejected() {
        for url in [
            "http://release-assets.githubusercontent.com/a",
            "https://evil.example/a",
            "https://user@release-assets.githubusercontent.com/a",
            "https://release-assets.githubusercontent.com:444/a",
        ] {
            assert!(validate_redirect(url).is_err(), "{url}");
        }
        assert!(validate_redirect("https://release-assets.githubusercontent.com/a").is_ok());
        assert!(validate_redirect(
            "https://github.com/talos-hq/clud/releases/download/2.8.13/clud-2.8.13.whl"
        )
        .is_ok());
        assert!(
            validate_redirect("https://github.com/someone-else/clud/releases/download/a").is_err()
        );
    }

    #[test]
    fn wrong_native_format_is_rejected() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"not a native executable").unwrap();
        assert!(
            validate_native_header(&mut file, Os::Linux, Arch::X86_64, Flavor::Native).is_err()
        );
    }

    #[test]
    fn rename_when_released_moves_a_free_file_and_fails_fast_on_a_missing_one() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("stage");
        let to = temp.path().join("clud");
        fs::write(&from, b"x").unwrap();
        rename_when_released(&from, &to).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"x");
        // Not a sharing violation: no retry, so it returns well inside the wait.
        let started = Instant::now();
        assert!(rename_when_released(&from, &to).is_err());
        assert!(started.elapsed() < RELEASE_WAIT);
    }

    #[test]
    fn failed_post_commit_verification_restores_prior_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("clud");
        fs::write(&destination, b"prior bytes").unwrap();
        let stage = dir.path().join("stage");
        fs::write(&stage, b"new bytes").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"new bytes"));
        assert!(commit(&destination, &stage, "0.0.0", &digest).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"prior bytes");
        assert!(!destination.with_extension("clud-backup").exists());
    }
}
