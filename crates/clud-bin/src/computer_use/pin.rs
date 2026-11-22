//! The one pinned cua-driver release clud provisions, per host target.
//!
//! The digests are the archives' SHA-256 values from the release's
//! `SHA256SUMS` (talos-hq/computer-use `computer-use-v<VERSION>`). Bump all rows
//! together.

use crate::platform::host;

/// The pinned cua-driver version.
pub const VERSION: &str = "0.57.1";

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
    "computer-use-0.57.1-darwin-universal.tar.gz",
    "508f1f103dceb9a62caa88a5ee9bbeeb72fc5825a149b80028ee5acc4451ce26",
    24_864_305,
    "ComputerUse.app/Contents/MacOS/cua-driver",
);

pub const ARTIFACTS: &[Artifact] = &[
    Artifact {
        target: "linux-x64",
        asset: "computer-use-0.57.1-linux-x86_64.tar.gz",
        sha256: "2bacb5aad2e058197070296967c7c713865085d2a13f3125b53a9f252b2b1af4",
        size: 16_588_526,
        kind: ArchiveKind::TarGz,
        binary: "cua-driver",
    },
    Artifact {
        target: "linux-arm64",
        asset: "computer-use-0.57.1-linux-aarch64.tar.gz",
        sha256: "60895da81cfc421a2ae3f3ea5ad669cf580a52759751d846d4be0d007fd949a8",
        size: 15_593_797,
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
        asset: "computer-use-0.57.1-windows-x86_64.zip",
        sha256: "b9028e8d716c05cf54c6a731a4a89285ab1e7ea91afa51f014059cfd66e380fb",
        size: 26_366_400,
        kind: ArchiveKind::Zip,
        binary: "cua-driver.exe",
    },
    Artifact {
        target: "win32-arm64",
        asset: "computer-use-0.57.1-windows-aarch64.zip",
        sha256: "ad52d3dbe5aeacdee092f809201988151bc75d24c275c434f1d98314622d31f3",
        size: 24_357_982,
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
