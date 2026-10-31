//! Strict, bounded v1 release catalog resolution for the native installer.

use std::cmp::Ordering;
use std::collections::HashSet;

use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const SCHEMA: &str = "https://zackees.github.io/manifest.json/v1/manifest.schema.json";
const ONLINE_URL: &str = "https://talos-hq.github.io/clud/install/manifest.json";
// Catalogs published before the move to talos-hq (#2126) still name the old
// Pages host; accept it until the next publish rewrites the field.
const LEGACY_ONLINE_URL: &str = "https://zackees.github.io/clud/install/manifest.json";
// The canonical custom domain the catalog is fetched from (#2449).
const DOMAIN_ONLINE_URL: &str = "https://talosbot.ai/clud/install/manifest.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Darwin,
    Linux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

/// `VerifiedGlibc217` must come from a non-NixOS host probe that checked both
/// the GNU loader and glibc ABI floor. Catalog resolution never guesses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GnuEligibility {
    Unverified,
    VerifiedGlibc217,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    pub os: Os,
    pub arch: Arch,
    pub gnu: GnuEligibility,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionChoice {
    LatestStable,
    Exact(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Native,
    StaticMusl,
    Gnu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Direct,
    Wheel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAsset {
    pub version: String,
    pub filename: String,
    pub media_type: MediaType,
    pub size_bytes: u64,
    pub sha256: String,
    pub url: String,
    pub flavor: Flavor,
    pub os: Os,
    pub arch: Arch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRelease {
    pub version: String,
    pub asset: ResolvedAsset,
}

#[derive(Debug)]
pub struct Catalog {
    latest_stable: String,
    releases: Vec<Release>,
}

#[derive(Debug)]
struct Release {
    version: String,
    order: Version,
    assets: Vec<ResolvedAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    pre: Vec<Identifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Identifier {
    Numeric(u64),
    Text(String),
}

impl Ord for Identifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Numeric(a), Self::Numeric(b)) => a.cmp(b),
            (Self::Numeric(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Numeric(_)) => Ordering::Greater,
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Identifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Version {
    fn parse(input: &str) -> Result<Self, String> {
        let without_build = input.split_once('+').map_or(input, |(v, _)| v);
        let (core, pre) = without_build
            .split_once('-')
            .map_or((without_build, ""), |(c, p)| (c, p));
        let parts: Vec<_> = core.split('.').collect();
        if parts.len() != 3
            || parts.iter().any(|p| {
                p.is_empty()
                    || (p.len() > 1 && p.starts_with('0'))
                    || !p.bytes().all(|b| b.is_ascii_digit())
            })
        {
            return Err(format!("invalid catalog version: {input}"));
        }
        let mut numbers = [0; 3];
        for (slot, part) in numbers.iter_mut().zip(parts) {
            *slot = part
                .parse()
                .map_err(|_| format!("invalid catalog version: {input}"))?;
        }
        if input.contains('+') {
            let build = input.split_once('+').unwrap().1;
            if !valid_identifiers(build) {
                return Err(format!("invalid catalog version: {input}"));
            }
        }
        if without_build.contains('-') && !valid_identifiers(pre) {
            return Err(format!("invalid catalog version: {input}"));
        }
        let pre = if pre.is_empty() {
            Vec::new()
        } else {
            pre.split('.')
                .map(|part| {
                    if part.bytes().all(|b| b.is_ascii_digit()) {
                        if part.len() > 1 && part.starts_with('0') {
                            return Err(format!("invalid catalog version: {input}"));
                        }
                        part.parse::<u64>()
                            .map(Identifier::Numeric)
                            .map_err(|_| format!("invalid catalog version: {input}"))
                    } else {
                        Ok(Identifier::Text(part.to_owned()))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(Self { core: numbers, pre })
    }
}

/// Check an exact release version without fetching a catalog.
pub fn validate_version(value: &str) -> Result<(), String> {
    Version::parse(value).map(|_| ())
}

fn valid_identifiers(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

impl Catalog {
    pub fn latest_stable_version(&self) -> &str {
        &self.latest_stable
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        Self::parse_internal(bytes, None)
    }

    pub fn parse_candidate(bytes: &[u8], expected_version: &str) -> Result<Self, String> {
        Self::parse_internal(bytes, Some(expected_version))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
    )]
    fn parse_internal(bytes: &[u8], expected_candidate: Option<&str>) -> Result<Self, String> {
        let document = crate::server_settings::parse_strict_json(bytes, MAX_CATALOG_BYTES)?;
        let root = object(&document, "catalog")?;
        require_string(root, "$schema", SCHEMA)?;
        require_string(root, "kind", "Catalog")?;
        if root.get("schema_version").and_then(Value::as_u64) != Some(1) {
            return Err("unsupported catalog schema version".into());
        }
        require_string(root, "tool", "clud")?;
        let online_url = string(field(root, "online_url")?, "online_url")?;
        if ![ONLINE_URL, LEGACY_ONLINE_URL, DOMAIN_ONLINE_URL].contains(&online_url) {
            return Err("unexpected online_url".into());
        }
        let channels = object(field(root, "channels")?, "channels")?;
        let latest_stable = string(field(channels, "latest-stable")?, "latest-stable")?.to_owned();
        if !Version::parse(&latest_stable)?.pre.is_empty() {
            return Err("latest-stable points to a prerelease".into());
        }
        match expected_candidate {
            Some(expected) => {
                if channels.len() != 2
                    || channels.get("candidate").and_then(Value::as_str) != Some(expected)
                {
                    return Err("candidate channel does not match requested version".into());
                }
                let candidate_version = Version::parse(expected)?;
                if !candidate_version.pre.is_empty()
                    || candidate_version <= Version::parse(&latest_stable)?
                {
                    return Err("candidate must be a newer stable-form version".into());
                }
            }
            None if channels.contains_key("candidate") => {
                return Err("candidate channel requires explicit candidate mode".into());
            }
            None => {}
        }
        let raw_releases = field(root, "releases")?
            .as_array()
            .ok_or("releases must be an array")?;
        if raw_releases.is_empty() || raw_releases.len() > 1000 {
            return Err("invalid release count".into());
        }
        let mut releases = Vec::with_capacity(raw_releases.len());
        let mut seen_versions = HashSet::new();
        for raw in raw_releases {
            let row = object(raw, "release")?;
            let version = string(field(row, "version")?, "version")?.to_owned();
            let order = Version::parse(&version)?;
            let published_at = string(field(row, "published_at")?, "published_at")?;
            if published_at.len() < 20 || !published_at.ends_with('Z') {
                return Err(format!("invalid publication time for {version}"));
            }
            if !seen_versions.insert(version.clone()) {
                return Err(format!("duplicate release {version}"));
            }
            let platforms = field(row, "platforms")?
                .as_array()
                .ok_or("platforms must be an array")?;
            if platforms.len() > 16 {
                return Err(format!("too many assets for {version}"));
            }
            let mut assets = Vec::with_capacity(platforms.len());
            let mut targets = HashSet::new();
            for platform in platforms {
                let asset = parse_asset(platform, &version)?;
                if !targets.insert((asset.os as u8, asset.arch as u8, asset.flavor as u8)) {
                    return Err(format!("duplicate platform variant in {version}"));
                }
                assets.push(asset);
            }
            releases.push(Release {
                version,
                order,
                assets,
            });
        }
        let stable = releases
            .iter()
            .find(|r| r.version == latest_stable)
            .ok_or("latest-stable release is missing")?;
        if !complete(stable) {
            return Err("latest-stable release is incomplete".into());
        }
        if let Some(expected) = expected_candidate {
            let candidate = releases
                .iter()
                .find(|release| release.version == expected)
                .ok_or("candidate release is missing")?;
            if !complete(candidate) {
                return Err("candidate release is incomplete".into());
            }
        }
        let newest_complete_stable = releases
            .iter()
            .filter(|r| {
                r.order.pre.is_empty()
                    && complete(r)
                    && Some(r.version.as_str()) != expected_candidate
            })
            .max_by(|a, b| a.order.cmp(&b.order))
            .ok_or("no complete stable release")?;
        if newest_complete_stable.version != latest_stable {
            return Err("latest-stable is not the newest complete stable release".into());
        }
        releases.sort_by(|a, b| b.order.cmp(&a.order));
        Ok(Self {
            latest_stable,
            releases,
        })
    }

    pub fn compatible_releases(&self, host: Host) -> Vec<ResolvedRelease> {
        self.releases
            .iter()
            .filter(|r| complete(r))
            .filter_map(|r| {
                choose(&r.assets, host).map(|asset| ResolvedRelease {
                    version: r.version.clone(),
                    asset: asset.clone(),
                })
            })
            .collect()
    }

    pub fn resolve(&self, choice: VersionChoice, host: Host) -> Result<ResolvedAsset, String> {
        let wanted = match choice {
            VersionChoice::LatestStable => &self.latest_stable,
            VersionChoice::Exact(ref v) => v,
        };
        let release = self
            .releases
            .iter()
            .find(|r| &r.version == wanted)
            .ok_or_else(|| format!("release {wanted} is unavailable"))?;
        if !complete(release) {
            return Err(format!("release {wanted} is incomplete"));
        }
        choose(&release.assets, host)
            .cloned()
            .ok_or_else(|| format!("release {wanted} has no compatible asset"))
    }
}

impl ResolvedAsset {
    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() as u64 != self.size_bytes {
            return Err("asset size mismatch".into());
        }
        if format!("{:x}", Sha256::digest(bytes)) != self.sha256 {
            return Err("asset SHA-256 mismatch".into());
        }
        if self.flavor == Flavor::StaticMusl {
            verify_static_elf(bytes, self.arch)?;
        }
        Ok(())
    }
}

fn object<'a>(value: &'a Value, name: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{name} must be an object"))
}

fn field<'a>(map: &'a serde_json::Map<String, Value>, name: &str) -> Result<&'a Value, String> {
    map.get(name).ok_or_else(|| format!("missing {name}"))
}

fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("{name} must be a string"))
}

/// Owners whose release URLs a catalog row may name: the repository moved
/// from zackees to talos-hq (#2126), and published rows keep the old owner.
const RELEASE_OWNERS: [&str; 2] = ["talos-hq", "zackees"];

pub(super) fn is_release_asset_url(url: &str, version: &str, filename: &str) -> bool {
    RELEASE_OWNERS.iter().any(|owner| {
        ["", "v"].iter().any(|prefix| {
            url == format!(
                "https://github.com/{owner}/clud/releases/download/{prefix}{version}/{filename}"
            )
        })
    })
}

fn require_string(
    map: &serde_json::Map<String, Value>,
    name: &str,
    expected: &str,
) -> Result<(), String> {
    if string(field(map, name)?, name)? != expected {
        return Err(format!("unexpected {name}"));
    }
    Ok(())
}

fn parse_asset(value: &Value, version: &str) -> Result<ResolvedAsset, String> {
    let row = object(value, "platform row")?;
    let platform = object(field(row, "platform")?, "platform")?;
    let os = match string(field(platform, "os")?, "os")? {
        "windows" => Os::Windows,
        "darwin" => Os::Darwin,
        "linux" => Os::Linux,
        _ => return Err("unknown platform OS".into()),
    };
    let arch = match string(field(platform, "arch")?, "arch")? {
        "x86_64" => Arch::X86_64,
        "aarch64" => Arch::Aarch64,
        _ => return Err("unknown platform architecture".into()),
    };
    let flavor = if os == Os::Linux {
        let variant = object(field(row, "variant")?, "variant")?;
        if variant.len() != 1 {
            return Err("unknown Linux variant fields".into());
        }
        match string(field(variant, "flavor")?, "flavor")? {
            "static-musl" if !platform.contains_key("libc") => Flavor::StaticMusl,
            "gnu" if platform.get("libc").and_then(Value::as_str) == Some("glibc") => Flavor::Gnu,
            _ => return Err("unknown or mismatched Linux variant".into()),
        }
    } else {
        if row.contains_key("variant") || platform.contains_key("libc") {
            return Err("unexpected native platform variant".into());
        }
        Flavor::Native
    };
    let item = object(field(row, "asset")?, "asset")?;
    let filename = string(field(item, "filename")?, "filename")?;
    let prefix = format!("clud-{version}-");
    let suffix = filename
        .strip_prefix(&prefix)
        .ok_or("asset version differs from release")?;
    let media_type = match string(field(item, "media_type")?, "media_type")? {
        "application/octet-stream" => MediaType::Direct,
        "application/zip" => MediaType::Wheel,
        _ => return Err("unknown asset media type".into()),
    };
    if !valid_filename(suffix, os, arch, flavor, media_type) {
        return Err(format!("asset filename or format mismatch: {filename}"));
    }
    let size_bytes = field(item, "size_bytes")?
        .as_u64()
        .filter(|s| *s > 0)
        .ok_or("invalid asset size")?;
    let sha256 = string(field(item, "sha256")?, "sha256")?;
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid asset SHA-256".into());
    }
    let urls = field(item, "urls")?
        .as_array()
        .ok_or("asset URLs must be an array")?;
    if urls.len() != 1 {
        return Err("asset must have exactly one URL".into());
    }
    let url = string(&urls[0], "asset URL")?;
    if !is_release_asset_url(url, version, filename) {
        return Err("asset URL does not match release".into());
    }
    if field(item, "provides")? != &serde_json::json!(["clud"]) {
        return Err("asset does not provide clud".into());
    }
    Ok(ResolvedAsset {
        version: version.to_owned(),
        filename: filename.to_owned(),
        media_type,
        size_bytes,
        sha256: sha256.to_owned(),
        url: url.to_owned(),
        flavor,
        os,
        arch,
    })
}

fn valid_filename(suffix: &str, os: Os, arch: Arch, flavor: Flavor, media: MediaType) -> bool {
    let cpu = match arch {
        Arch::X86_64 => "x86_64",
        Arch::Aarch64 => "aarch64",
    };
    match media {
        MediaType::Direct => match (os, flavor) {
            (Os::Windows, Flavor::Native) => suffix == format!("{cpu}-pc-windows-msvc.exe"),
            (Os::Darwin, Flavor::Native) => suffix == format!("{cpu}-apple-darwin"),
            (Os::Linux, Flavor::Gnu) => suffix == format!("{cpu}-unknown-linux-gnu"),
            (Os::Linux, Flavor::StaticMusl) => suffix == format!("{cpu}-unknown-linux-musl"),
            _ => false,
        },
        MediaType::Wheel => match (os, arch, flavor) {
            (Os::Windows, Arch::X86_64, Flavor::Native) => suffix == "py3-none-win_amd64.whl",
            (Os::Windows, Arch::Aarch64, Flavor::Native) => suffix == "py3-none-win_arm64.whl",
            (Os::Darwin, Arch::X86_64, Flavor::Native) => {
                suffix == "py3-none-macosx_10_15_x86_64.whl"
            }
            (Os::Darwin, Arch::Aarch64, Flavor::Native) => {
                suffix == "py3-none-macosx_11_0_arm64.whl"
            }
            (Os::Linux, _, Flavor::Gnu) => {
                suffix == format!("py3-none-manylinux_2_17_{cpu}.manylinux2014_{cpu}.whl")
            }
            _ => false,
        },
    }
}

fn complete(release: &Release) -> bool {
    [Os::Windows, Os::Darwin, Os::Linux].into_iter().all(|os| {
        [Arch::X86_64, Arch::Aarch64]
            .into_iter()
            .all(|arch| release.assets.iter().any(|a| a.os == os && a.arch == arch))
    })
}

fn choose(assets: &[ResolvedAsset], host: Host) -> Option<&ResolvedAsset> {
    let matching = || {
        assets
            .iter()
            .filter(|a| a.os == host.os && a.arch == host.arch)
    };
    if host.os == Os::Linux {
        matching()
            .find(|a| a.flavor == Flavor::StaticMusl)
            .or_else(|| {
                (host.gnu == GnuEligibility::VerifiedGlibc217)
                    .then(|| matching().find(|a| a.flavor == Flavor::Gnu))
                    .flatten()
            })
    } else {
        matching().next()
    }
}

fn verify_static_elf(bytes: &[u8], arch: Arch) -> Result<(), String> {
    if bytes.len() < 64 || &bytes[..6] != b"\x7fELF\x02\x01" {
        return Err("static musl asset is not ELF64 little-endian".into());
    }
    let elf_type = u16::from_le_bytes([bytes[16], bytes[17]]);
    if elf_type != 2 && elf_type != 3 {
        return Err("static musl asset is not an ELF executable".into());
    }
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    if machine
        != match arch {
            Arch::X86_64 => 62,
            Arch::Aarch64 => 183,
        }
    {
        return Err("static musl ELF architecture mismatch".into());
    }
    let phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(bytes[54..56].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as usize;
    if phentsize < 56
        || phnum == 0
        || phoff
            .checked_add(
                phentsize
                    .checked_mul(phnum)
                    .ok_or("invalid ELF program headers")?,
            )
            .filter(|end| *end <= bytes.len())
            .is_none()
    {
        return Err("invalid ELF program headers".into());
    }
    for i in 0..phnum {
        let start = phoff + i * phentsize;
        let kind = u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap());
        if kind == 3 {
            return Err("musl executable has a PT_INTERP loader".into());
        }
        if kind == 2 {
            let offset =
                u64::from_le_bytes(bytes[start + 8..start + 16].try_into().unwrap()) as usize;
            let size =
                u64::from_le_bytes(bytes[start + 32..start + 40].try_into().unwrap()) as usize;
            if !size.is_multiple_of(16)
                || offset
                    .checked_add(size)
                    .filter(|end| *end <= bytes.len())
                    .is_none()
            {
                return Err("invalid ELF dynamic table".into());
            }
            for entry in (offset..offset + size).step_by(16) {
                if i64::from_le_bytes(bytes[entry..entry + 8].try_into().unwrap()) == 1 {
                    return Err("musl executable has a DT_NEEDED dependency".into());
                }
            }
        }
    }
    Ok(())
}

pub(super) fn verify_static_elf_file(file: &mut std::fs::File, arch: Arch) -> Result<(), String> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(256 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 256 * 1024 * 1024 {
        return Err("static musl executable is too large".into());
    }
    verify_static_elf(&bytes, arch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};

    fn elf(machine: u16) -> Vec<u8> {
        let mut bytes = vec![0; 128];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&1_u32.to_le_bytes());
        bytes
    }

    fn row(os: &str, arch: &str, flavor: Option<&str>, suffix: &str, bytes: &[u8]) -> Value {
        let version = "2.9.0";
        let filename = format!("clud-{version}-{suffix}");
        let mut platform = json!({"os": os, "arch": arch});
        if flavor == Some("gnu") {
            platform["libc"] = json!("glibc");
        }
        let mut item = json!({
            "platform": platform,
            "asset": {
                "filename": filename,
                "media_type": if suffix.ends_with(".whl") { "application/zip" } else { "application/octet-stream" },
                "size_bytes": bytes.len(),
                "sha256": format!("{:x}", Sha256::digest(bytes)),
                "urls": [format!("https://github.com/zackees/clud/releases/download/{version}/{filename}")],
                "provides": ["clud"]
            }
        });
        if let Some(flavor) = flavor {
            item["variant"] = json!({"flavor": flavor});
        }
        item
    }

    fn catalog(musl: bool, gnu: bool) -> Value {
        let mut platforms = vec![
            row(
                "windows",
                "x86_64",
                None,
                "x86_64-pc-windows-msvc.exe",
                b"MZx64",
            ),
            row(
                "windows",
                "aarch64",
                None,
                "aarch64-pc-windows-msvc.exe",
                b"MZarm",
            ),
            row("darwin", "x86_64", None, "x86_64-apple-darwin", b"mach-x64"),
            row(
                "darwin",
                "aarch64",
                None,
                "aarch64-apple-darwin",
                b"mach-arm",
            ),
        ];
        if gnu {
            platforms.extend([
                row(
                    "linux",
                    "x86_64",
                    Some("gnu"),
                    "py3-none-manylinux_2_17_x86_64.manylinux2014_x86_64.whl",
                    b"wheel-x64",
                ),
                row(
                    "linux",
                    "aarch64",
                    Some("gnu"),
                    "py3-none-manylinux_2_17_aarch64.manylinux2014_aarch64.whl",
                    b"wheel-arm",
                ),
            ]);
        }
        if musl {
            platforms.extend([
                row(
                    "linux",
                    "x86_64",
                    Some("static-musl"),
                    "x86_64-unknown-linux-musl",
                    &elf(62),
                ),
                row(
                    "linux",
                    "aarch64",
                    Some("static-musl"),
                    "aarch64-unknown-linux-musl",
                    &elf(183),
                ),
            ]);
        }
        json!({
            "$schema": "https://zackees.github.io/manifest.json/v1/manifest.schema.json",
            "kind": "Catalog", "schema_version": 1, "tool": "clud",
            "online_url": "https://talos-hq.github.io/clud/install/manifest.json",
            "channels": {"latest-stable": "2.9.0"},
            "releases": [{"version": "2.9.0", "published_at": "2026-09-01T00:00:00Z", "platforms": platforms}]
        })
    }

    fn linux_x64(gnu: GnuEligibility) -> Host {
        Host {
            os: Os::Linux,
            arch: Arch::X86_64,
            gnu,
        }
    }

    #[test]
    fn candidate_channel_keeps_previous_stable_and_requires_explicit_parse() {
        let mut document = catalog(true, false);
        let mut candidate = document["releases"][0].clone();
        candidate["version"] = json!("2.10.0");
        for row in candidate["platforms"].as_array_mut().unwrap() {
            let asset = row["asset"].as_object_mut().unwrap();
            for key in ["filename", "urls"] {
                let replaced = asset[key].to_string().replace("2.9.0", "2.10.0");
                asset.insert(key.to_owned(), serde_json::from_str(&replaced).unwrap());
            }
        }
        document["releases"].as_array_mut().unwrap().push(candidate);
        document["channels"]["candidate"] = json!("2.10.0");
        let bytes = serde_json::to_vec(&document).unwrap();
        assert!(Catalog::parse(&bytes).is_err());
        let parsed = Catalog::parse_candidate(&bytes, "2.10.0").unwrap();
        assert_eq!(parsed.latest_stable_version(), "2.9.0");
        assert!(parsed
            .resolve(
                VersionChoice::Exact("2.10.0".into()),
                linux_x64(GnuEligibility::Unverified),
            )
            .is_ok());
        assert!(Catalog::parse_candidate(&bytes, "2.11.0").is_err());
    }

    #[test]
    fn catalog_from_the_pre_transfer_pages_host_still_parses() {
        // #2126: catalogs published before the move to talos-hq name the old host.
        let mut document = catalog(true, false);
        document["online_url"] = json!(LEGACY_ONLINE_URL);
        assert!(Catalog::parse(&serde_json::to_vec(&document).unwrap()).is_ok());
        document["online_url"] = json!("https://example.com/clud/install/manifest.json");
        assert!(Catalog::parse(&serde_json::to_vec(&document).unwrap()).is_err());
    }

    #[test]
    fn both_linux_variants_choose_musl_even_when_gnu_comes_first() {
        let document = serde_json::to_vec(&catalog(true, true)).unwrap();
        let parsed = Catalog::parse(&document).unwrap();
        let resolved = parsed
            .resolve(
                VersionChoice::LatestStable,
                linux_x64(GnuEligibility::VerifiedGlibc217),
            )
            .unwrap();
        assert_eq!(resolved.flavor, Flavor::StaticMusl);
        assert!(resolved.filename.ends_with("-unknown-linux-musl"));
    }

    #[test]
    fn gnu_history_requires_explicit_loader_and_abi_proof() {
        let document = serde_json::to_vec(&catalog(false, true)).unwrap();
        let parsed = Catalog::parse(&document).unwrap();
        let eligible = linux_x64(GnuEligibility::VerifiedGlibc217);
        assert_eq!(
            parsed
                .resolve(VersionChoice::LatestStable, eligible)
                .unwrap()
                .flavor,
            Flavor::Gnu
        );
        let nixos = linux_x64(GnuEligibility::Unverified);
        assert!(parsed.compatible_releases(nixos).is_empty());
        assert!(parsed.resolve(VersionChoice::LatestStable, nixos).is_err());
    }

    #[test]
    fn bad_musl_digest_is_terminal_for_the_selected_asset() {
        let document = serde_json::to_vec(&catalog(true, true)).unwrap();
        let parsed = Catalog::parse(&document).unwrap();
        let selected = parsed
            .resolve(
                VersionChoice::LatestStable,
                linux_x64(GnuEligibility::VerifiedGlibc217),
            )
            .unwrap();
        assert_eq!(selected.flavor, Flavor::StaticMusl);
        selected.verify_bytes(&elf(62)).unwrap();
        let mut corrupted = elf(62);
        corrupted[100] = 1;
        assert!(selected.verify_bytes(&corrupted).is_err());
    }

    #[test]
    fn musl_verification_rejects_loader_and_needed_library() {
        let mut object = elf(62);
        object[16..18].copy_from_slice(&1_u16.to_le_bytes());
        assert!(verify_static_elf(&object, Arch::X86_64).is_err());

        let mut loader = elf(62);
        loader[64..68].copy_from_slice(&3_u32.to_le_bytes());
        assert!(verify_static_elf(&loader, Arch::X86_64).is_err());

        let mut needed = elf(62);
        needed[64..68].copy_from_slice(&2_u32.to_le_bytes());
        needed[72..80].copy_from_slice(&120_u64.to_le_bytes());
        needed[96..104].copy_from_slice(&16_u64.to_le_bytes());
        needed.resize(136, 0);
        needed[120..128].copy_from_slice(&1_i64.to_le_bytes());
        assert!(verify_static_elf(&needed, Arch::X86_64).is_err());

        needed[120..128].copy_from_slice(&0_i64.to_le_bytes());
        verify_static_elf(&needed, Arch::X86_64).unwrap();
    }

    #[test]
    fn ambiguous_legacy_and_incomplete_catalogs_fail_closed() {
        let mut duplicate = catalog(true, true);
        let extra = duplicate["releases"][0]["platforms"][6].clone();
        duplicate["releases"][0]["platforms"]
            .as_array_mut()
            .unwrap()
            .push(extra);
        assert!(Catalog::parse(&serde_json::to_vec(&duplicate).unwrap()).is_err());

        let mut legacy = catalog(false, true);
        legacy["releases"][0]["platforms"][4]
            .as_object_mut()
            .unwrap()
            .remove("variant");
        assert!(Catalog::parse(&serde_json::to_vec(&legacy).unwrap()).is_err());

        let mut unknown = catalog(true, false);
        unknown["releases"][0]["platforms"][4]["variant"]["extra"] = json!(true);
        assert!(Catalog::parse(&serde_json::to_vec(&unknown).unwrap()).is_err());

        let mut incomplete = catalog(true, false);
        incomplete["releases"][0]["platforms"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert!(Catalog::parse(&serde_json::to_vec(&incomplete).unwrap()).is_err());

        let mut prerelease = catalog(true, false);
        prerelease["channels"]["latest-stable"] = json!("2.9.0-rc.1");
        assert!(Catalog::parse(&serde_json::to_vec(&prerelease).unwrap()).is_err());
    }

    #[test]
    fn duplicate_json_keys_and_mismatched_metadata_are_rejected() {
        let duplicate_keys = br#"{"kind":"Catalog","kind":"Catalog"}"#;
        assert!(Catalog::parse(duplicate_keys).is_err());

        let mut wrong_hash = catalog(true, true);
        wrong_hash["releases"][0]["platforms"][4]["asset"]["sha256"] = json!("xyz");
        assert!(Catalog::parse(&serde_json::to_vec(&wrong_hash).unwrap()).is_err());

        let mut wrong_url = catalog(true, true);
        wrong_url["releases"][0]["platforms"][4]["asset"]["urls"] =
            json!(["https://example.com/clud"]);
        assert!(Catalog::parse(&serde_json::to_vec(&wrong_url).unwrap()).is_err());

        let mut wrong_arch = catalog(true, true);
        wrong_arch["releases"][0]["platforms"][4]["platform"]["arch"] = json!("aarch64");
        assert!(Catalog::parse(&serde_json::to_vec(&wrong_arch).unwrap()).is_err());
    }

    #[test]
    fn list_is_semantic_and_skips_incomplete_versions() {
        let mut document = catalog(true, false);
        let mut older = document["releases"][0].clone();
        older["version"] = json!("2.10.0");
        for row in older["platforms"].as_array_mut().unwrap() {
            let filename = row["asset"]["filename"]
                .as_str()
                .unwrap()
                .replace("2.9.0", "2.10.0");
            let url = row["asset"]["urls"][0]
                .as_str()
                .unwrap()
                .replace("2.9.0", "2.10.0");
            row["asset"]["filename"] = json!(filename);
            row["asset"]["urls"][0] = json!(url);
        }
        older["platforms"].as_array_mut().unwrap().pop();
        document["releases"].as_array_mut().unwrap().push(older);
        let parsed = Catalog::parse(&serde_json::to_vec(&document).unwrap()).unwrap();
        let versions: Vec<_> = parsed
            .compatible_releases(linux_x64(GnuEligibility::Unverified))
            .into_iter()
            .map(|r| r.version)
            .collect();
        assert_eq!(versions, vec!["2.9.0"]);
        assert!(parsed
            .resolve(
                VersionChoice::Exact("2.10.0".into()),
                linux_x64(GnuEligibility::Unverified)
            )
            .is_err());
    }

    #[test]
    fn versions_reject_ambiguous_numeric_prerelease_identifiers() {
        assert!(Version::parse("2.9.0-01").is_err());
        assert!(Version::parse("2.9.0-1").is_ok());
        assert!(Version::parse("2.9.0-rc.01").is_err());
        assert!(Version::parse("2.9.0-rc.1").is_ok());
    }
}
