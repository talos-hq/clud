//! The one pinned cua-driver release clud provisions, per host target.
//!
//! The digests are the archives' SHA-256 values from the release's
//! `SHA256SUMS` (trycua/cua `cua-driver-rs-v<VERSION>`). Hermes pins the same
//! archive names, so a Hermes install of this exact version is reusable (see
//! `provision::hermes_install`). Bump all rows together.

use crate::platform::host;

/// The pinned cua-driver version.
pub const VERSION: &str = "0.55.0";

/// The release tag the archives are published under.
const TAG_PREFIX: &str = "https://github.com/trycua/cua/releases/download/cua-driver-rs-v";

/// How the archive is packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    Zip,
}

/// One host target's pinned archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Artifact {
    /// Target label, also Hermes PM's target key.
    pub target: &'static str,
    /// Archive file name under the release tag.
    pub asset: &'static str,
    /// SHA-256 of the archive, lowercase hex.
    pub sha256: &'static str,
    /// Exact archive size in bytes; the download is cut off past it.
    pub size: u64,
    pub kind: ArchiveKind,
    /// The driver executable, relative to the unpacked root.
    pub binary: &'static str,
}

impl Artifact {
    pub fn url(&self) -> String {
        format!("{TAG_PREFIX}{VERSION}/{}", self.asset)
    }
}

/// macOS needs the directory archive: only it carries the signed
/// `CuaDriver.app`, whose identity (com.trycua.driver) holds the TCC grants.
const DARWIN: (&str, &str, u64, &str) = (
    "cua-driver-rs-0.55.0-darwin-universal.tar.gz",
    "faf11948f8f9b410a76750e65ae14cb1d95cc321aa61fc158f70de8a68c270b1",
    70_055_816,
    "CuaDriver.app/Contents/MacOS/cua-driver",
);

pub const ARTIFACTS: &[Artifact] = &[
    Artifact {
        target: "linux-x64",
        asset: "cua-driver-rs-0.55.0-linux-x86_64-binary.tar.gz",
        sha256: "1cb8aa6d30e5cdf5bf4d222cb3693a50af2c178073a49dc59e88737c5f7807e6",
        size: 30_235_418,
        kind: ArchiveKind::TarGz,
        binary: "cua-driver",
    },
    Artifact {
        target: "linux-arm64",
        asset: "cua-driver-rs-0.55.0-linux-arm64-binary.tar.gz",
        sha256: "401f7f4a9ad276dbcdb324f1b00e152987a46985b68c9e625ac01101f5e6af77",
        size: 28_132_426,
        kind: ArchiveKind::TarGz,
        binary: "cua-driver",
    },
    Artifact {
        target: "darwin-x64",
        asset: DARWIN.0,
        sha256: DARWIN.1,
        size: DARWIN.2,
        kind: ArchiveKind::TarGz,
        binary: DARWIN.3,
    },
    Artifact {
        target: "darwin-arm64",
        asset: DARWIN.0,
        sha256: DARWIN.1,
        size: DARWIN.2,
        kind: ArchiveKind::TarGz,
        binary: DARWIN.3,
    },
    Artifact {
        target: "win32-x64",
        asset: "cua-driver-rs-0.55.0-windows-x86_64-binary.zip",
        sha256: "357b93254183c0e24a501eb9ad3a6e0f3738a67bfed55c0c292959b540d9b4a7",
        size: 32_550_400,
        kind: ArchiveKind::Zip,
        binary: "cua-driver.exe",
    },
    Artifact {
        target: "win32-arm64",
        asset: "cua-driver-rs-0.55.0-windows-arm64-binary.zip",
        sha256: "ab1515f8893fb8b1acda5a4ffe1d2e97d19a938460d0f5ac7b6e9ed88f37f020",
        size: 30_339_984,
        kind: ArchiveKind::Zip,
        binary: "cua-driver.exe",
    },
];

/// The target label for a host described by its OS and architecture.
pub fn target_label(
    windows: bool,
    macos: bool,
    linux: bool,
    aarch64: bool,
) -> Option<&'static str> {
    let arch = if aarch64 { "arm64" } else { "x64" };
    let os = if windows {
        "win32"
    } else if macos {
        "darwin"
    } else if linux {
        "linux"
    } else {
        return None;
    };
    ARTIFACTS
        .iter()
        .map(|artifact| artifact.target)
        .find(|target| *target == format!("{os}-{arch}"))
}

/// The pinned artifact for the running host, or `None` where cua-driver
/// publishes no build (for example the BSDs).
pub fn for_host() -> Option<&'static Artifact> {
    let label = target_label(host::WINDOWS, host::MACOS, host::LINUX, host::AARCH64)?;
    ARTIFACTS.iter().find(|artifact| artifact.target == label)
}
