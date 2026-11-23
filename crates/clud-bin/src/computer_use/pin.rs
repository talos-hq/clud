//! The one pinned cua-driver release clud provisions, per host target.
//!
//! The digests are the archives' SHA-256 values from the release's
//! `SHA256SUMS` (talos-hq/computer-use `computer-use-v<VERSION>`). Bump all rows
//! together.

use crate::platform::host;

/// The pinned cua-driver version.
pub const VERSION: &str = "0.57.2";

/// The release tag the archives are published under.
const TAG_PREFIX: &str =
    "https://github.com/talos-hq/computer-use/releases/download/computer-use-v";

/// How the archive is packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    Zip,
}

/// One host target's pinned archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Artifact {
    /// Target label (`<os>-<arch>`).
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
/// `ComputerUse.app`, whose identity (com.taloshq.computer-use) holds the TCC grants.
const DARWIN: (&str, &str, u64, &str) = (
    "computer-use-0.57.2-darwin-universal.tar.gz",
    "9ecfbba8c1b13100bf1f2a869c1d2a0ecbd5a326555fcc394c9a3581747d6a7d",
    24_863_380,
    "ComputerUse.app/Contents/MacOS/cua-driver",
);

pub const ARTIFACTS: &[Artifact] = &[
    Artifact {
        target: "linux-x64",
        asset: "computer-use-0.57.2-linux-x86_64.tar.gz",
        sha256: "529857b9ebcb1a22e4b7450f2f7e935e96374df2c60385a9d84115e5e15002e8",
        size: 16_549_799,
        kind: ArchiveKind::TarGz,
        binary: "cua-driver",
    },
    Artifact {
        target: "linux-arm64",
        asset: "computer-use-0.57.2-linux-aarch64.tar.gz",
        sha256: "cffe922c0d9ca6e281cbd358cc21677f1fbb7beb80c488234521d5a50fbd060e",
        size: 15_596_744,
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
        asset: "computer-use-0.57.2-windows-x86_64.zip",
        sha256: "919309cf98213c341c24810ff65e7441b5cb02aaa83f7587aec429620a48a997",
        size: 26_365_036,
        kind: ArchiveKind::Zip,
        binary: "cua-driver.exe",
    },
    Artifact {
        target: "win32-arm64",
        asset: "computer-use-0.57.2-windows-aarch64.zip",
        sha256: "cd199f5d2fb89b3a157b8b304f5db05c08ed9517af560f6d027092fea1185135",
        size: 24_418_552,
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
