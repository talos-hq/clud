//! `clud symbols` — inspect or verify crash-report symbolication.
//!
//! Background: clud builds with `debug = "line-tables-only"` keep the
//! line tables in the binary itself (#374 PR 1, see
//! [`crate::crash_report`]), so a backtrace resolves to file:line with
//! nothing to fetch. Local dev builds stop there — everything is
//! embedded. Release builds no longer do: `[profile.release]` sets
//! `split-debuginfo = "packed"`, which moves the rest of the DWARF into
//! a sidecar (a `.dwp` on Linux, the `.pdb` / `.dSYM` Windows and macOS
//! always produced) so the manylinux wheel fits under PyPI's 100 MB
//! project limit. What a release binary loses without its `.dwp` is the
//! inlined-subroutine DIEs: file:line still resolves per *physical*
//! frame and function names still come from `.symtab`, but the expanded
//! inline caller chain does not. CI attaches the `.dwp` to the GitHub
//! release, so the "fetch sidecars on first unsymbolicated report" path
//! from the original issue is real work again rather than a no-op.
//!
//! - `clud symbols` (bare) prints a five-line summary of the crash-
//!   reports directory.
//! - `clud symbols install` fetches the sidecar the most recent report
//!   needs, but only after the release's `BUILD-IDS.txt` says the report's
//!   GNU build-id is the published binary for its triple. It checks the
//!   sidecar against the release's `SHA256SUMS` and caches it under
//!   `~/.clud/state/symbols/<build-id>/`. A report from a local build or a
//!   same-version rebuild has no matching entry, so nothing is fetched and
//!   the user is pointed at the `target/` tree beside the binary instead.
//! - `clud symbols verify [--all]` checks that the running binary can
//!   resolve a report's backtrace. Exits 1 if it can't. `--all` widens
//!   the scope from the most recent report to every report.
//!
//! # Network
//!
//! `install` is the only thing here that reaches the network, and only
//! when a person types it. The crash path does not fetch — the process is
//! already dying and may be in a signal handler where allocation is
//! unsafe — the startup notice does not fetch, and `verify` does not
//! fetch. Air-gapped hosts therefore keep working, with local
//! unsymbolicated output as the fallback (#1016).
//!
//! The opportunistic startup notice (see [`crate::crash_report::install`])
//! prints a one-line hint pointing at `clud symbols verify` when a fresh
//! report's backtrace is unsymbolicated, so users discover the command
//! without needing to remember it.

use std::fs;
use std::path::{Path, PathBuf};

use crate::args::{Args, SymbolsSubcommand};

/// Where a released sidecar lives, as one named constant.
///
/// #1016 item 1: the URL is derivable from the version, but building it at
/// the call site puts the tag-vs-version convention in as many places as
/// there are callers, and leaves a fork or a mirror nothing to repoint.
///
/// The tag is the bare version -- clud's releases are tagged `2.7.9`, not
/// `v2.7.9` -- and that convention lives here and nowhere else.
pub const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/zackees/clud/releases/download";

/// The sidecar asset for `target`, or `None` when that triple ships none.
///
/// `ci/xbuild.py::collect_debuginfo` stages exactly one file per ELF triple,
/// named `clud-<triple>.dwp`, and deliberately not the `deps/clud-<hash>.dwp`
/// it was copied from -- its own comment says an asset name carrying a build
/// hash "is one #1016's fetcher could not predict". This is that fetcher's
/// half of the arrangement.
///
/// Only ELF targets are covered, which is not an oversight: on MSVC and Apple
/// `split-debuginfo = "packed"` is already the default, their `.pdb` / `.dSYM`
/// were never embedded in the shipped wheel, and `collect_debuginfo` does not
/// stage them. Returning `Some` for those would name an asset that does not
/// exist, which is worse than saying there is nothing to fetch.
#[must_use]
pub fn sidecar_asset_name(target: &str) -> Option<String> {
    target
        .contains("-linux-")
        .then(|| format!("clud-{target}.dwp"))
}

/// The full download URL for `version`'s sidecar for `target`.
#[must_use]
pub fn sidecar_url(version: &str, target: &str) -> Option<String> {
    let asset = sidecar_asset_name(target)?;
    Some(format!("{RELEASE_DOWNLOAD_BASE}/{version}/{asset}"))
}

/// The release's checksum manifest, which covers the sidecar assets.
///
/// `auto-release.yml` publishes one `SHA256SUMS` per release spanning `dist/`
/// and `debuginfo/`, so a fetched sidecar can be checked against the bytes the
/// release actually published.
pub const CHECKSUM_MANIFEST: &str = "SHA256SUMS";

/// URL of the checksum manifest for `version`.
#[must_use]
pub fn checksum_manifest_url(version: &str) -> String {
    format!("{RELEASE_DOWNLOAD_BASE}/{version}/{CHECKSUM_MANIFEST}")
}

/// The release asset that pairs each shipped Linux binary with its sidecar.
///
/// One line per triple, `<gnu-build-id-hex>  <triple>`, written by
/// `ci/build_ids.py` from the shipped binary's `.note.gnu.build-id` and listed
/// in [`CHECKSUM_MANIFEST`]. `tests/test_build_ids.py` pins this name against
/// the Python side.
pub const BUILD_IDS_ASSET: &str = "BUILD-IDS.txt";

/// URL of `version`'s [`BUILD_IDS_ASSET`].
#[must_use]
pub fn build_ids_url(version: &str) -> String {
    format!("{RELEASE_DOWNLOAD_BASE}/{version}/{BUILD_IDS_ASSET}")
}

/// The published build-id for `target`, parsed out of a `BUILD-IDS.txt` body,
/// lowercased. `None` when the triple has no entry.
#[must_use]
pub fn published_build_id(build_ids: &str, target: &str) -> Option<String> {
    build_ids.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let id = parts.next()?;
        let triple = parts.next()?;
        (triple == target && parts.next().is_none()).then(|| id.to_ascii_lowercase())
    })
}

/// The expected sha256 of `asset`, parsed out of a `SHA256SUMS` body.
///
/// Entries are `<hex>  ./<name>`, the `sha256sum` format the release job
/// generates from inside the directory, hence the `./` prefix. Both spellings
/// are accepted so a manifest generated without it still matches.
///
/// # What a checksum proves, and what pairs the sidecar to the binary
///
/// This authenticates the *asset*: the bytes are the ones the release
/// published for this version and triple. It says nothing about which binary
/// they belong to, and the `.dwp` cannot say either -- a DWARF package is a
/// relocatable object carrying `.debug_*.dwo` sections and `.debug_cu_index`,
/// with no `.note.gnu.build-id` (`readelf -n` on release 2.7.9's sidecar
/// reports no notes at all).
///
/// The pairing comes from [`BUILD_IDS_ASSET`] instead: the release job reads
/// the build-id of each shipped Linux binary in the job that produced its
/// `.dwp` and publishes `<build-id>  <triple>` lines. [`install_with`] fetches
/// the sidecar only when the crash report's `build_id` equals that entry, so
/// a local build or a same-version rebuild is never paired with the published
/// DWARF (#1016, option 2 of the discussion there).
#[must_use]
pub fn expected_sha256(manifest: &str, asset: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let (hex, name) = line.split_once("  ")?;
        let name = name.trim();
        let name = name.strip_prefix("./").unwrap_or(name);
        (name == asset && !hex.is_empty()).then(|| hex.trim().to_string())
    })
}

/// True for a plausible GNU build-id: 8 to 128 hex digits and nothing else.
fn is_build_id_hex(id: &str) -> bool {
    (8..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Heuristic for whether a single backtrace line is an `at FILE:LINE`
/// frame produced by `std::backtrace::Backtrace`.
///
/// Pattern: leading whitespace, the literal `at `, then any path, then
/// `:` and at least one digit. Optional `:column`. We match by character
/// scan rather than regex to avoid pulling in `regex` for one helper.
fn is_resolved_frame_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix("at ") else {
        return false;
    };
    // Find the last `:` followed by digits. Backtrace lines on Windows
    // can contain drive-letter colons (`C:\...`), so we scan from the
    // right.
    let last_colon = match rest.rfind(':') {
        Some(i) => i,
        None => return false,
    };
    let after_colon = &rest[last_colon + 1..];
    // Trim trailing whitespace + an optional `:column` suffix —
    // rustc-format lines look like `at file.rs:42:5`.
    let after_colon = after_colon.trim_end();
    // If there's a `:column`, split it off first.
    let head = after_colon.split(':').next().unwrap_or("");
    !head.is_empty() && head.chars().all(|c| c.is_ascii_digit())
}

/// Count `at FILE:LINE` frame lines in a backtrace string.
pub(crate) fn count_resolved_frames(backtrace: &str) -> usize {
    backtrace
        .lines()
        .filter(|l| is_resolved_frame_line(l))
        .count()
}

/// True when the backtrace contains zero `at FILE:LINE` lines. Empty
/// backtraces are treated as unsymbolicated.
pub(crate) fn is_unsymbolicated(backtrace: &str) -> bool {
    count_resolved_frames(backtrace) == 0
}

/// Sort `dir` entries by filename's leading unix-ms prefix, newest
/// first. Returns paths only.
fn list_reports_newest_first(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut entries: Vec<(u128, PathBuf)> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy().into_owned();
            if !name.ends_with(".json") {
                return None;
            }
            let ms = name.split('-').next()?.parse::<u128>().ok()?;
            Some((ms, e.path()))
        })
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    Ok(entries.into_iter().map(|(_, p)| p).collect())
}

fn read_report_backtrace(path: &Path) -> Option<(String, String, u128)> {
    let raw = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let backtrace = value.get("backtrace")?.as_str()?.to_string();
    let role = value
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let ts = value
        .get("timestamp_unix_ms")
        .and_then(|v| v.as_u64())
        .map(|t| t as u128)
        .unwrap_or(0);
    Some((backtrace, role, ts))
}

/// Turn a URL into bytes.
///
/// A seam, so the install path below is exercised without a network: an 85 MB
/// `.dwp` is not something a test suite should be downloading, and a test that
/// needs the real release to exist would fail for reasons that have nothing to
/// do with the code.
pub type Fetch<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, FetchError>;

/// Why a fetch produced no bytes.
///
/// `NotFound` is its own case because it is an *answer*, not a failure: a
/// release with no [`BUILD_IDS_ASSET`] (a version never published, or one
/// published before the pairing existed) means "this binary cannot be paired",
/// which is a skip, while a network error is a real failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    NotFound,
    Failed(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("404 not found"),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

/// The real fetcher. `ureq` is already this crate's HTTP client (`codex_auth`,
/// `codex_bridge`).
fn ureq_fetch(url: &str) -> Result<Vec<u8>, FetchError> {
    let response = match ureq::get(url).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Err(FetchError::NotFound),
        Err(err) => return Err(FetchError::Failed(err.to_string())),
    };
    let mut bytes = Vec::new();
    // `into_reader`, not `into_string`: the payload is a binary sidecar tens
    // of megabytes long, and `into_string` both caps at 10 MB and would mangle
    // it as UTF-8.
    std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)
        .map_err(|err| FetchError::Failed(err.to_string()))?;
    Ok(bytes)
}

/// Where fetched sidecars live: `~/.clud/state/symbols/<build-id>/<asset>`.
///
/// Beside `~/.clud/state/crashes/`, as #1016 suggested. Keyed by the GNU
/// build-id because that is what identifies the binary the sidecar belongs
/// to; a version names a release, and a rebuild at the same version is a
/// different binary. Only a build-id that [`BUILD_IDS_ASSET`] vouches for is
/// ever used as a key.
fn cache_root() -> std::io::Result<PathBuf> {
    let dir = crate::crash_report::crashes_dir()?
        .parent()
        .ok_or_else(|| std::io::Error::other("crashes dir has no parent"))?
        .join("symbols");
    Ok(dir)
}

/// Which build wrote a report. Every field is optional: reports written
/// before #1016 carry no `target` and no `build_id`, and non-ELF builds never
/// carry a `build_id`.
#[derive(Debug)]
struct ReportIdentity {
    version: Option<String>,
    target: Option<String>,
    build_id: Option<String>,
}

fn read_report_identity(path: &Path) -> ReportIdentity {
    let value: Option<serde_json::Value> = fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let field = |name: &str| {
        value
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    ReportIdentity {
        version: field("version"),
        target: field("target"),
        build_id: field("build_id").map(|id| id.to_ascii_lowercase()),
    }
}

/// The note printed whenever a binary cannot be paired with a published
/// sidecar. Deliberately not phrased as an error: a local build *has* its
/// symbols -- cargo left them beside the binary.
fn print_not_published(reason: &str, target: &str) {
    println!(
        "clud symbols: {reason}\n\
         This binary is not a published build, so no sidecar was fetched. Its \
         symbols are beside it in target/{target}/<profile>/ (clud.dwp, or \
         deps/clud-*.dwp) of the tree that built it."
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    use kernal_api::hash::Sha256Hasher as Sha256;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// How many builds' sidecars the cache keeps.
///
/// Two: the build you are running and the one you just upgraded from, which
/// is the pair a crash report in hand can plausibly need. A `.dwp` is ~85 MB,
/// so this is a bound of roughly 170 MB rather than an unbounded tree.
///
/// #1016 asks for an eviction rule specifically because of #1014 -- nothing
/// sweeps `~/.clud/state/`, and adding a directory that only grows would be
/// repeating the mistake that issue was filed about.
const MAX_CACHED_BUILDS: usize = 2;

/// Drop all but the newest [`MAX_CACHED_BUILDS`] build-id directories.
///
/// Any directory counts, so the `<version>/` directories an earlier clud
/// cached under are aged out by the same rule rather than left behind.
///
/// Runs after a successful install rather than on a timer: the cache only
/// grows when something is added to it, so the moment of adding is the only
/// moment a bound can be crossed. That also keeps this out of the daemon's
/// periodic work, which #542 asks not to grow.
///
/// Newest by directory mtime: a build-id has no order at all, and "the one I
/// fetched least recently" is exactly the thing worth dropping and is what
/// mtime records.
///
/// Failures are non-fatal. A cache that could not be pruned is a disk-space
/// problem; an install that reported failure because pruning failed would be a
/// correctness problem, and the sidecar is already safely in place by now.
fn prune_cache(cache_root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(cache_root) else {
        return 0;
    };
    let mut builds: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let mtime = entry.metadata().ok()?.modified().ok()?;
            Some((mtime, entry.path()))
        })
        .collect();
    if builds.len() <= MAX_CACHED_BUILDS {
        return 0;
    }
    builds.sort_by_key(|build| std::cmp::Reverse(build.0));
    let mut removed = 0;
    for (_, path) in builds.into_iter().skip(MAX_CACHED_BUILDS) {
        // Audit before acting (#893). These are large files clud fetched
        // rather than the user's own data, but the audit line is what proves
        // afterwards what was removed and under which rule.
        crate::gc::delete_audit::record(
            "gc.symbols-cache",
            &path,
            &format!("symbols-cache keep-newest>{MAX_CACHED_BUILDS}"),
        );
        if fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// `clud symbols install` — fetch the sidecar this machine's most recent crash
/// report needs, verify it against the release manifest, and cache it.
///
/// # Network happens here and nowhere else
///
/// #1016 is explicit that a tool which phones a release host after a crash
/// will surprise people, and that some environments are air-gapped. So the
/// only thing that ever reaches the network is this subcommand, typed by a
/// person. The crash path does not fetch, the startup notice does not fetch,
/// and `verify` does not fetch. That also keeps the fetch out of a dying
/// process where allocation may be unsafe.
pub fn install(reports: &[PathBuf]) -> i32 {
    let cache = match cache_root() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("clud symbols: cannot resolve the symbol cache: {err}");
            return 1;
        }
    };
    install_with(reports, &ureq_fetch, &cache)
}

/// Test seam for [`install`].
///
/// Order matters: `BUILD-IDS.txt` is consulted *before* anything that could
/// fetch the sidecar, so a binary the release does not vouch for never costs
/// an 85 MB download, and every "cannot pair" outcome is a skip (exit 0), not
/// an error -- a local build is a normal thing to be running.
#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub fn install_with(reports: &[PathBuf], fetch: Fetch, cache_root: &Path) -> i32 {
    let Some(report) = reports.first() else {
        println!("clud symbols: no crash reports, so no sidecar to fetch.");
        return 0;
    };
    let identity = read_report_identity(report);
    let (Some(version), Some(target)) = (identity.version, identity.target) else {
        println!(
            "clud symbols: {} records no version/target, so no published \
             sidecar can be paired with it (reports written before #1016 do \
             not carry them). Nothing was fetched.",
            report.display()
        );
        return 0;
    };
    let Some(asset) = sidecar_asset_name(&target) else {
        // Not a failure of this machine: no such asset is published. Saying
        // so beats a 404 the user has to interpret.
        println!(
            "clud symbols: no sidecar is published for {target}. Debug info \
             for that target is not split out of the wheel, so there is \
             nothing to fetch."
        );
        return 0;
    };
    // The build-id becomes a cache path component, so it must be plain hex:
    // a report is a file on disk, and a crafted `"../.."` must not steer a
    // write anywhere.
    let Some(build_id) = identity.build_id.filter(|id| is_build_id_hex(id)) else {
        print_not_published(
            &format!(
                "{} records no GNU build-id (reports written before #1129 do \
                 not carry one), so it cannot be matched against release \
                 {version}'s {BUILD_IDS_ASSET}.",
                report.display()
            ),
            &target,
        );
        return 0;
    };

    let destination = cache_root.join(&build_id).join(&asset);
    if destination.is_file() {
        println!(
            "clud symbols: already have {} ({})",
            asset,
            destination.display()
        );
        return 0;
    }

    // Pair first. Only a build-id the release published for this triple may
    // be matched with the published sidecar; anything else would symbolicate
    // to confident, wrong line numbers.
    let pairing_url = build_ids_url(&version);
    let build_ids_bytes = match fetch(&pairing_url) {
        Ok(bytes) => bytes,
        Err(FetchError::NotFound) => {
            print_not_published(
                &format!(
                    "release {version} publishes no {BUILD_IDS_ASSET} (it was \
                     never released, or predates build-id pairing)."
                ),
                &target,
            );
            return 0;
        }
        Err(err) => {
            eprintln!("clud symbols: cannot fetch {pairing_url}: {err}");
            return 1;
        }
    };
    let build_ids = String::from_utf8_lossy(&build_ids_bytes);
    match published_build_id(&build_ids, &target) {
        Some(published) if published == build_id => {}
        Some(published) => {
            print_not_published(
                &format!(
                    "build-id {build_id} is not release {version}'s published \
                     {target} binary ({published})."
                ),
                &target,
            );
            return 0;
        }
        None => {
            print_not_published(
                &format!("release {version}'s {BUILD_IDS_ASSET} has no entry for {target}."),
                &target,
            );
            return 0;
        }
    }

    let manifest_url = checksum_manifest_url(&version);
    let manifest = match fetch(&manifest_url) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) => {
            eprintln!("clud symbols: cannot fetch {manifest_url}: {err}");
            return 1;
        }
    };
    // The pairing itself must be the published one: a tampered BUILD-IDS.txt
    // could vouch for any binary. Checked before the payload is fetched.
    if expected_sha256(&manifest, BUILD_IDS_ASSET).as_deref()
        != Some(sha256_hex(&build_ids_bytes).as_str())
    {
        eprintln!(
            "clud symbols: {BUILD_IDS_ASSET} does not match release {version}'s \
             {CHECKSUM_MANIFEST}; refusing to pair a sidecar on its word."
        );
        return 1;
    }
    // The expected digest is read *before* the payload, so there is never a
    // window where a downloaded file exists with nothing to check it against.
    let Some(expected) = expected_sha256(&manifest, &asset) else {
        eprintln!(
            "clud symbols: release {version} publishes no checksum for \
             {asset}; refusing to install bytes the release does not vouch \
             for."
        );
        return 1;
    };

    let Some(url) = sidecar_url(&version, &target) else {
        eprintln!("clud symbols: no sidecar URL for {target}");
        return 1;
    };
    println!("clud symbols: fetching {url}");
    let bytes = match fetch(&url) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("clud symbols: cannot fetch {url}: {err}");
            return 1;
        }
    };

    let actual = sha256_hex(&bytes);
    if actual != expected {
        // Nothing is written. A sidecar that is not the published one would
        // symbolicate to confident, wrong line numbers -- worse than not
        // symbolicating, which is the whole argument in #1016 item 3.
        eprintln!(
            "clud symbols: checksum mismatch for {asset}\n  expected {expected}\n  got      {actual}\nRefusing to install."
        );
        return 1;
    }

    if let Some(parent) = destination.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            eprintln!("clud symbols: cannot create {}: {err}", parent.display());
            return 1;
        }
    }
    // Write beside and rename, so a reader never sees a half-written sidecar
    // and an interrupted install leaves no file that looks complete.
    let staging = destination.with_extension("part");
    if let Err(err) = fs::write(&staging, &bytes) {
        eprintln!("clud symbols: cannot write {}: {err}", staging.display());
        return 1;
    }
    if let Err(err) = fs::rename(&staging, &destination) {
        let _ = fs::remove_file(&staging);
        eprintln!(
            "clud symbols: cannot place {}: {err}",
            destination.display()
        );
        return 1;
    }
    println!(
        "clud symbols: installed {} ({} bytes, sha256 verified)",
        destination.display(),
        bytes.len()
    );
    let pruned = prune_cache(cache_root);
    if pruned > 0 {
        println!(
            "clud symbols: dropped {pruned} older sidecar version(s), keeping \
             the newest {MAX_CACHED_BUILDS}"
        );
    }
    0
}

/// Dispatch entry called from `main.rs`. Returns a process exit code.
pub fn run(_args: &Args, subcommand: Option<SymbolsSubcommand>) -> i32 {
    let dir = match crate::crash_report::crashes_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("clud symbols: cannot resolve crash-report dir: {e}");
            return 1;
        }
    };
    let reports = match list_reports_newest_first(&dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("clud symbols: cannot read {}: {e}", dir.display());
            return 1;
        }
    };
    match subcommand {
        None => print_summary(&dir, &reports),
        // #1016: `install` now installs. It used to be an alias for `verify`,
        // which was honest while there were no sidecars to fetch and became
        // wrong the moment `split-debuginfo = "packed"` shipped them.
        Some(SymbolsSubcommand::Install) => install(&reports),
        Some(SymbolsSubcommand::Verify { all }) => verify(&reports, all),
    }
}

fn print_summary(dir: &Path, reports: &[PathBuf]) -> i32 {
    println!("clud symbols: crashes dir: {}", dir.display());
    println!("total reports: {}", reports.len());
    if reports.is_empty() {
        println!("no reports to inspect");
        return 0;
    }
    let mut resolved = 0usize;
    let mut unresolved = 0usize;
    for path in reports {
        if let Some((bt, _, _)) = read_report_backtrace(path) {
            if is_unsymbolicated(&bt) {
                unresolved += 1;
            } else {
                resolved += 1;
            }
        }
    }
    println!("reports with file:line frames: {resolved}");
    println!("reports without file:line frames: {unresolved}");
    if let Some(newest) = reports.first() {
        if let Some((_, role, ts)) = read_report_backtrace(newest) {
            println!(
                "most recent: {} (role={}, unix_ms={})",
                newest.display(),
                role,
                ts
            );
        } else {
            println!("most recent: {}", newest.display());
        }
    }
    0
}

fn verify(reports: &[PathBuf], all: bool) -> i32 {
    if reports.is_empty() {
        println!("clud symbols: no crash reports to verify");
        return 0;
    }
    let targets: &[PathBuf] = if all { reports } else { &reports[..1] };
    let mut all_resolved = true;
    for path in targets {
        match read_report_backtrace(path) {
            Some((bt, role, _)) => {
                let resolved = count_resolved_frames(&bt);
                if resolved == 0 {
                    println!(
                        "FAIL {} (role={}): backtrace contains 0 file:line frames",
                        path.display(),
                        role
                    );
                    all_resolved = false;
                } else {
                    println!(
                        "OK   {} (role={}): {} file:line frames",
                        path.display(),
                        role,
                        resolved
                    );
                }
            }
            None => {
                println!(
                    "FAIL {}: unreadable JSON or missing backtrace",
                    path.display()
                );
                all_resolved = false;
            }
        }
    }
    if all_resolved {
        println!(
            "clud symbols: OK — embedded line tables resolved {} report(s)",
            targets.len()
        );
        0
    } else {
        println!(
            "clud symbols: FAIL — embedded line tables did not resolve one or more reports.\n\
             Build with `debug = \"line-tables-only\"` (already the project default) and ensure\n\
             the binary running `clud symbols verify` is the same build that produced the report."
        );
        // #1016: a release build's inline caller chain lives in a sidecar
        // beside the release, not in the binary. Naming the exact asset is
        // the difference between "symbolication failed" and something the
        // reader can act on -- and it is derivable here because the triple is
        // baked in (`crash_report::BUILD_TARGET`).
        match sidecar_url(env!("CARGO_PKG_VERSION"), crate::crash_report::BUILD_TARGET) {
            Some(url) => println!(
                "\nA release binary's sidecar debug info (the inlined-frame DIEs it\n\
                 does not carry) is published beside the release:\n  {url}\n\
                 `clud symbols install` fetches it when the report's build-id is the\n\
                 published one; a local build's sidecar is already beside it in target/."
            ),
            None => println!(
                "\nNo sidecar is published for {} — on this platform the debug info\n\
                 was never split out of the shipped binary, so there is nothing to fetch.",
                crate::crash_report::BUILD_TARGET
            ),
        }
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_resolved_lines_in_typical_backtrace() {
        let bt = "   0: clud::main::h1234abcd\n             at /home/user/clud/src/main.rs:42:5\n   1: core::ops::function::FnOnce::call_once\n             at /rustc/abcdef/library/core/src/ops/function.rs:250:5\n";
        assert_eq!(count_resolved_frames(bt), 2);
        assert!(!is_unsymbolicated(bt));
    }

    #[test]
    fn detects_unsymbolicated_backtrace() {
        let bt = "   0: 0x7fffabcd1234\n   1: 0x7fffabcd5678\n   2: 0x7fffabcd9abc\n";
        assert_eq!(count_resolved_frames(bt), 0);
        assert!(is_unsymbolicated(bt));
    }

    #[test]
    fn empty_backtrace_is_unsymbolicated() {
        assert_eq!(count_resolved_frames(""), 0);
        assert!(is_unsymbolicated(""));
    }

    #[test]
    fn windows_drive_letter_does_not_trip_resolution() {
        let bt =
            "   0: clud::main::h1234abcd\n             at C:\\Users\\me\\clud\\src\\main.rs:42:5\n";
        assert_eq!(count_resolved_frames(bt), 1);
    }

    #[test]
    fn frame_without_at_prefix_is_not_resolved() {
        let bt = "             /home/user/clud/src/main.rs:42:5\n";
        assert_eq!(count_resolved_frames(bt), 0);
    }

    #[test]
    fn frame_without_line_number_is_not_resolved() {
        let bt = "             at /home/user/clud/src/main.rs\n";
        assert_eq!(count_resolved_frames(bt), 0);
    }

    #[test]
    fn list_reports_orders_by_unix_ms_prefix_desc() -> std::io::Result<()> {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new()?;
        fs::write(tmp.path().join("100-foreground-1.json"), "{}")?;
        fs::write(tmp.path().join("300-foreground-3.json"), "{}")?;
        fs::write(tmp.path().join("200-foreground-2.json"), "{}")?;
        let ordered = list_reports_newest_first(tmp.path())?;
        assert_eq!(ordered.len(), 3);
        assert!(ordered[0].ends_with("300-foreground-3.json"));
        assert!(ordered[1].ends_with("200-foreground-2.json"));
        assert!(ordered[2].ends_with("100-foreground-1.json"));
        Ok(())
    }

    #[test]
    fn read_report_backtrace_extracts_role_and_ts() -> std::io::Result<()> {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new()?;
        let path = tmp.path().join("500-daemon-9999.json");
        fs::write(
            &path,
            r#"{
                "version": "0.0.0",
                "role": "daemon",
                "kind": "panic",
                "pid": 9999,
                "args": [],
                "timestamp_unix_ms": 500,
                "panic_message": "boom",
                "backtrace": "   0: clud::main::h\n             at /x/main.rs:1:1\n"
            }"#,
        )?;
        let (bt, role, ts) = read_report_backtrace(&path).expect("parsed");
        assert_eq!(role, "daemon");
        assert_eq!(ts, 500);
        assert!(bt.contains("/x/main.rs:1:1"));
        assert_eq!(count_resolved_frames(&bt), 1);
        Ok(())
    }

    /// The derived URL must be the one that actually exists.
    ///
    /// Checked against release 2.7.9, whose asset list really does contain
    /// `clud-x86_64-unknown-linux-gnu.dwp`. Pinning a live example is what
    /// makes this more than a restatement of the format string: the tag is
    /// the bare version (clud tags `2.7.9`, not `v2.7.9`), and getting that
    /// wrong yields a 404 that looks like a missing sidecar.
    #[test]
    fn the_derived_url_matches_a_release_asset_that_exists() {
        assert_eq!(
            sidecar_url("2.7.9", "x86_64-unknown-linux-gnu").as_deref(),
            Some(
                "https://github.com/zackees/clud/releases/download/2.7.9/\
                 clud-x86_64-unknown-linux-gnu.dwp"
            )
        );
        assert_eq!(
            sidecar_asset_name("aarch64-unknown-linux-gnu").as_deref(),
            Some("clud-aarch64-unknown-linux-gnu.dwp"),
            "the aarch64 sidecar is published too"
        );
    }

    /// Triples that publish no sidecar must say so rather than name a 404.
    ///
    /// `collect_debuginfo` stages only the ELF `.dwp`: on MSVC and Apple,
    /// `packed` split-debuginfo is already the default and the `.pdb`/`.dSYM`
    /// was never inside the shipped wheel. A confident URL for an asset that
    /// was never uploaded is worse than "there is nothing to fetch".
    #[test]
    fn triples_without_a_published_sidecar_return_none() {
        for target in [
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ] {
            assert_eq!(
                sidecar_asset_name(target),
                None,
                "{target} publishes no .dwp"
            );
            assert_eq!(sidecar_url("2.7.9", target), None, "{target}");
        }
    }

    /// The asset name is the one `ci/xbuild.py::collect_debuginfo` stages.
    ///
    /// That code renames the `deps/clud-<hash>.dwp` it copies from precisely
    /// so this side can predict the name -- its comment says an asset name
    /// carrying a build hash "is one #1016's fetcher could not predict". If
    /// either side drifts, the fetch 404s.
    #[test]
    fn the_asset_name_carries_the_triple_and_no_build_hash() {
        let name = sidecar_asset_name("x86_64-unknown-linux-gnu").expect("linux publishes one");
        assert!(name.starts_with("clud-"), "{name}");
        assert!(name.ends_with(".dwp"), "{name}");
        assert!(name.contains("x86_64-unknown-linux-gnu"), "{name}");
        assert!(
            !name.chars().any(|c| c == '#'),
            "the staged name is fixed, not hash-suffixed: {name}"
        );
    }

    /// The base is a single constant so a fork or mirror has one thing to
    /// repoint, which is item 1's stated reason for existing.
    #[test]
    fn the_release_base_is_one_named_constant() {
        assert!(RELEASE_DOWNLOAD_BASE.starts_with("https://"));
        assert!(
            !RELEASE_DOWNLOAD_BASE.ends_with('/'),
            "the joiner adds the separator; a trailing one yields a double slash"
        );
    }

    /// Real bytes from release 2.7.9's `SHA256SUMS`, trimmed to the rows that
    /// matter. Using the published manifest rather than a synthetic one is
    /// what makes the parser's format assumptions testable -- the `./` prefix
    /// and the two-space separator are `sha256sum`'s, not ours.
    const REAL_MANIFEST: &str = concat!(
        "b586d13c8859db1c0f8bd665f83fdd3eecb60cc6fd6c6937f5e96e9c848c9648  ",
        "./clud-2.7.9-py3-none-manylinux_2_17_x86_64.manylinux2014_x86_64.whl\n",
        "18f5d3675e5cd1237f81751517eadce44dc0cd053c330c45b0691608acb3aef4  ",
        "./clud-aarch64-unknown-linux-gnu.dwp\n",
        "6d30b668c7eb96f5e0e0d3d9c3c07becddcabaac9f18312f623616e4838901c3  ",
        "./clud-x86_64-unknown-linux-gnu.dwp\n",
    );

    /// The checksum found must be the one the release actually published.
    ///
    /// `6d30b668...` was verified by downloading the 62 MB sidecar and running
    /// `sha256sum` over it, so this pins the parser against reality rather
    /// than against itself.
    #[test]
    fn finds_the_published_checksum_for_a_sidecar() {
        let asset = sidecar_asset_name("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(
            expected_sha256(REAL_MANIFEST, &asset).as_deref(),
            Some("6d30b668c7eb96f5e0e0d3d9c3c07becddcabaac9f18312f623616e4838901c3")
        );
    }

    /// The two sidecars differ by one path component; picking the wrong row
    /// would verify successfully against the wrong architecture's DWARF.
    #[test]
    fn does_not_confuse_the_two_architectures() {
        let x86 = expected_sha256(REAL_MANIFEST, "clud-x86_64-unknown-linux-gnu.dwp");
        let arm = expected_sha256(REAL_MANIFEST, "clud-aarch64-unknown-linux-gnu.dwp");
        assert!(x86.is_some() && arm.is_some());
        assert_ne!(x86, arm, "each triple has its own sidecar and its own sum");
    }

    /// An asset that is not listed has no expected sum -- callers must treat
    /// that as "cannot verify", never as "verified".
    #[test]
    fn an_unlisted_asset_has_no_checksum() {
        assert_eq!(
            expected_sha256(REAL_MANIFEST, "clud-x86_64-pc-windows-msvc.dwp"),
            None
        );
        assert_eq!(expected_sha256("", "anything"), None);
        assert_eq!(expected_sha256("garbage without a separator\n", "x"), None);
    }

    /// A manifest generated without `sha256sum`'s `./` prefix still matches,
    /// so the parser does not depend on which directory the release job ran in.
    #[test]
    fn a_bare_name_matches_too() {
        let manifest = "abc123  clud-x86_64-unknown-linux-gnu.dwp\n";
        assert_eq!(
            expected_sha256(manifest, "clud-x86_64-unknown-linux-gnu.dwp").as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn the_manifest_url_sits_beside_the_sidecar() {
        assert_eq!(
            checksum_manifest_url("2.7.9"),
            "https://github.com/zackees/clud/releases/download/2.7.9/SHA256SUMS"
        );
    }

    // -----------------------------------------------------------------
    // #1016: `clud symbols install` pairs by BUILD-IDS.txt, then fetches.
    // -----------------------------------------------------------------

    use std::cell::RefCell;

    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const BUILD_ID: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A report on disk, as `install_with` will read it.
    fn write_report(dir: &Path, version: &str, target: &str, build_id: Option<&str>) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("1700000000000-crash.json");
        let mut value = serde_json::json!({
            "version": version,
            "target": target,
            "backtrace": "0: main\n",
        });
        if let Some(id) = build_id {
            value["build_id"] = serde_json::Value::from(id);
        }
        fs::write(&path, value.to_string()).unwrap();
        path
    }

    /// Records every URL asked for, so a test can assert what was *not*
    /// fetched — "the 85 MB sidecar was never requested" is the property the
    /// skip paths exist for, not just "returned 0".
    struct FakeNet {
        /// `None` models a release with no BUILD-IDS.txt (a 404).
        build_ids: Option<String>,
        manifest: String,
        payload: Vec<u8>,
        seen: RefCell<Vec<String>>,
    }

    impl FakeNet {
        fn fetch(&self, url: &str) -> Result<Vec<u8>, FetchError> {
            self.seen.borrow_mut().push(url.to_string());
            if url.ends_with(BUILD_IDS_ASSET) {
                self.build_ids
                    .clone()
                    .map(String::into_bytes)
                    .ok_or(FetchError::NotFound)
            } else if url.ends_with(CHECKSUM_MANIFEST) {
                Ok(self.manifest.clone().into_bytes())
            } else {
                Ok(self.payload.clone())
            }
        }

        fn requested_the_sidecar(&self) -> bool {
            self.seen.borrow().iter().any(|url| url.ends_with(".dwp"))
        }
    }

    /// A consistent release: BUILD-IDS.txt names `published` for LINUX, and
    /// SHA256SUMS covers both it and the sidecar.
    fn release(published: &str, payload: &[u8]) -> FakeNet {
        let asset = sidecar_asset_name(LINUX).unwrap();
        let other = "ff".repeat(20);
        let build_ids = format!("{published}  {LINUX}\n{other}  aarch64-unknown-linux-gnu\n");
        FakeNet {
            manifest: format!(
                "{}  ./{asset}\n{}  ./{BUILD_IDS_ASSET}\n",
                sha256_hex(payload),
                sha256_hex(build_ids.as_bytes())
            ),
            build_ids: Some(build_ids),
            payload: payload.to_vec(),
            seen: RefCell::new(Vec::new()),
        }
    }

    fn report_in(tmp: &Path, version: &str, build_id: Option<&str>) -> PathBuf {
        write_report(&tmp.join("crashes"), version, LINUX, build_id)
    }

    #[test]
    fn published_build_id_reads_the_entry_for_the_triple() {
        let body = format!(
            "{}  aarch64-unknown-linux-gnu\n{}  {LINUX}\n",
            "ab".repeat(20),
            BUILD_ID.to_uppercase()
        );
        assert_eq!(published_build_id(&body, LINUX).as_deref(), Some(BUILD_ID));
        assert_eq!(published_build_id(&body, "x86_64-unknown-linux-musl"), None);
        assert_eq!(published_build_id("", LINUX), None);
        assert_eq!(published_build_id("garbage\n", LINUX), None);
    }

    #[test]
    fn the_build_ids_url_sits_beside_the_sidecar() {
        assert_eq!(
            build_ids_url("2.7.9"),
            "https://github.com/zackees/clud/releases/download/2.7.9/BUILD-IDS.txt"
        );
    }

    /// Match: the report's build-id is the published one, so the sidecar is
    /// fetched, verified and cached under that build-id.
    #[test]
    fn a_matching_build_id_installs_the_sidecar_under_its_build_id() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let net = release(BUILD_ID, b"dwarf package bytes");

        let code = install_with(&[report], &|url| net.fetch(url), &cache);

        assert_eq!(code, 0);
        let installed = cache
            .join(BUILD_ID)
            .join(sidecar_asset_name(LINUX).unwrap());
        assert!(installed.is_file(), "sidecar was not cached");
        assert_eq!(fs::read(&installed).unwrap(), b"dwarf package bytes");
        assert!(
            !cache.join("2.8.0").exists(),
            "the cache is keyed by build-id, not version"
        );
        let seen = net.seen.borrow();
        assert_eq!(seen[0], build_ids_url("2.8.0"), "pairing is checked first");
        assert_eq!(seen[1], checksum_manifest_url("2.8.0"));
        assert_eq!(seen[2], sidecar_url("2.8.0", LINUX).unwrap());
    }

    /// Mismatch: a local build or a same-version rebuild. Skip, not an error,
    /// and the sidecar URL is never requested.
    #[test]
    fn a_different_build_id_skips_without_requesting_the_sidecar() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let net = release(&"ee".repeat(20), b"dwarf package bytes");

        let code = install_with(&[report], &|url| net.fetch(url), &cache);

        assert_eq!(code, 0, "a local build is not an error");
        assert!(!net.requested_the_sidecar(), "{:?}", net.seen.borrow());
        assert!(
            !cache.exists(),
            "nothing may be cached for an unpaired build"
        );
    }

    /// No entry for the triple: same skip.
    #[test]
    fn a_missing_entry_skips_without_requesting_the_sidecar() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let mut net = release(BUILD_ID, b"dwarf package bytes");
        net.build_ids = Some(format!("{}  aarch64-unknown-linux-gnu\n", "ab".repeat(20)));

        assert_eq!(install_with(&[report], &|url| net.fetch(url), &cache), 0);
        assert!(!net.requested_the_sidecar(), "{:?}", net.seen.borrow());
    }

    /// A release with no BUILD-IDS.txt at all (never released, or older than
    /// the pairing) is the same "not a published build" answer.
    #[test]
    fn a_release_without_build_ids_skips_without_requesting_the_sidecar() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let mut net = release(BUILD_ID, b"dwarf package bytes");
        net.build_ids = None;

        assert_eq!(install_with(&[report], &|url| net.fetch(url), &cache), 0);
        assert!(!net.requested_the_sidecar(), "{:?}", net.seen.borrow());
    }

    /// A report written before #1129 has no build-id: nothing can be paired,
    /// so nothing is fetched at all.
    #[test]
    fn a_report_without_a_build_id_skips_without_any_network() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", None);
        let cache = tmp.path().join("symbols");
        let net = release(BUILD_ID, b"unused");

        assert_eq!(install_with(&[report], &|url| net.fetch(url), &cache), 0);
        assert!(net.seen.borrow().is_empty(), "{:?}", net.seen.borrow());
    }

    /// The build-id becomes a path component; a crafted report must not steer
    /// the cache write outside the cache.
    #[test]
    fn a_non_hex_build_id_is_treated_as_absent() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some("../../escape"));
        let cache = tmp.path().join("symbols");
        let net = release("../../escape", b"payload");

        assert_eq!(install_with(&[report], &|url| net.fetch(url), &cache), 0);
        assert!(net.seen.borrow().is_empty());
        assert!(!tmp.path().join("escape").exists());
    }

    /// The refusal that matters. A sidecar that is not the published one
    /// symbolicates to confident, wrong line numbers — worse than not
    /// symbolicating at all, which is #1016's own argument.
    #[test]
    fn a_checksum_mismatch_installs_nothing() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let mut net = release(BUILD_ID, b"the published bytes");
        net.payload = b"something else entirely".to_vec();

        let code = install_with(&[report], &|url| net.fetch(url), &cache);

        assert_eq!(code, 1);
        let asset = sidecar_asset_name(LINUX).unwrap();
        assert!(
            !cache.join(BUILD_ID).join(&asset).exists(),
            "a file that failed verification was left on disk"
        );
        assert!(
            !cache.join(BUILD_ID).join(format!("{asset}.part")).exists(),
            "the staging file was left behind"
        );
    }

    /// BUILD-IDS.txt is itself vouched for by SHA256SUMS; a tampered pairing
    /// is refused before the sidecar is requested.
    #[test]
    fn a_build_ids_file_that_fails_its_checksum_is_refused_before_download() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let mut net = release(BUILD_ID, b"payload");
        net.build_ids = Some(format!("{BUILD_ID}  {LINUX}\n"));

        assert_eq!(install_with(&[report], &|url| net.fetch(url), &cache), 1);
        assert!(!net.requested_the_sidecar(), "{:?}", net.seen.borrow());
    }

    /// A release that does not list the asset vouches for nothing, so there is
    /// nothing to check the bytes against. Refuse before downloading them.
    #[test]
    fn an_asset_absent_from_the_manifest_is_refused_before_download() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let mut net = release(BUILD_ID, b"never requested");
        let ids = net.build_ids.clone().unwrap();
        net.manifest = format!("{}  ./{BUILD_IDS_ASSET}\n", sha256_hex(ids.as_bytes()));

        assert_eq!(install_with(&[report], &|url| net.fetch(url), &cache), 1);
        assert!(
            !net.requested_the_sidecar(),
            "the payload must not be fetched when nothing can verify it: {:?}",
            net.seen.borrow()
        );
    }

    /// A network failure (not a 404) on BUILD-IDS.txt is a real error.
    #[test]
    fn a_failed_build_ids_fetch_is_an_error() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let code = install_with(
            &[report],
            &|_url| Err(FetchError::Failed("connection reset".into())),
            &cache,
        );
        assert_eq!(code, 1);
    }

    /// Windows and macOS publish no `.dwp`. Reporting that is the correct
    /// answer, and it must not cost a request that would 404.
    #[test]
    fn a_target_with_no_published_sidecar_touches_no_network() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = write_report(
            &tmp.path().join("crashes"),
            "2.8.0",
            "x86_64-pc-windows-msvc",
            None,
        );
        let cache = tmp.path().join("symbols");
        let net = release(BUILD_ID, b"unused");

        let code = install_with(&[report], &|url| net.fetch(url), &cache);

        assert_eq!(code, 0, "not an error: no such asset is published");
        assert!(net.seen.borrow().is_empty(), "{:?}", net.seen.borrow());
    }

    /// Second run is free and offline. Re-downloading 85 MB because the user
    /// typed the command twice would be its own bug.
    #[test]
    fn an_already_installed_sidecar_is_not_fetched_again() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "2.8.0", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let net = release(BUILD_ID, b"dwarf package bytes");

        assert_eq!(
            install_with(std::slice::from_ref(&report), &|url| net.fetch(url), &cache),
            0
        );
        let after_first = net.seen.borrow().len();

        assert_eq!(install_with(&[report], &|url| net.fetch(url), &cache), 0);

        assert_eq!(
            net.seen.borrow().len(),
            after_first,
            "the cached sidecar was fetched a second time"
        );
    }

    /// Reports written before #1016 carry no version/target, so no asset can
    /// be named. That is a skip with a reason, not an error, and not a guess.
    #[test]
    fn a_report_without_version_or_target_is_not_guessed_at() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let dir = tmp.path().join("crashes");
        fs::create_dir_all(&dir).unwrap();
        let report = dir.join("1700000000000-crash.json");
        fs::write(&report, r#"{"backtrace":"0: main\n"}"#).unwrap();
        let cache = tmp.path().join("symbols");
        let net = release(BUILD_ID, b"unused");

        assert_eq!(install_with(&[report], &|url| net.fetch(url), &cache), 0);
        assert!(net.seen.borrow().is_empty());
    }

    /// No reports is a normal state, not a failure, and not a reason to fetch.
    #[test]
    fn no_reports_is_not_an_error() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let net = release(BUILD_ID, b"unused");

        assert_eq!(install_with(&[], &|url| net.fetch(url), tmp.path()), 0);
        assert!(net.seen.borrow().is_empty());
    }

    /// The URLs are built from the report, not from whatever this binary
    /// happens to be — a report copied from another machine still resolves to
    /// its own release.
    #[test]
    fn the_fetched_urls_name_the_reports_own_release() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "1.2.3", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let net = release(BUILD_ID, b"payload");

        install_with(&[report], &|url| net.fetch(url), &cache);

        let seen = net.seen.borrow();
        assert_eq!(
            *seen,
            vec![
                build_ids_url("1.2.3"),
                checksum_manifest_url("1.2.3"),
                sidecar_url("1.2.3", LINUX).unwrap(),
            ]
        );
    }

    // -----------------------------------------------------------------
    // #1016: the cache is bounded (#1014's lesson).
    // -----------------------------------------------------------------

    /// Make one build-id dir per name, oldest first, each holding a file.
    /// Returns them in creation order.
    fn seed_builds(cache: &Path, names: &[&str]) -> Vec<PathBuf> {
        let mut made = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let dir = cache.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("clud-x.dwp"), b"payload").unwrap();
            // Explicit, increasing mtimes: creation order alone is not a
            // guarantee at filesystem timestamp granularity, and a test that
            // depends on it fails on a fast machine rather than a slow one.
            let when = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000 + index as u64 * 60);
            filetime::set_file_mtime(&dir, filetime::FileTime::from_system_time(when)).unwrap();
            made.push(dir);
        }
        made
    }

    #[test]
    fn the_cache_keeps_only_the_newest_builds() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cache = tmp.path();
        let dirs = seed_builds(cache, &["aaaa0001", "ffff0002", "0000cafe", "bbbb0004"]);

        let removed = prune_cache(cache);

        assert_eq!(removed, dirs.len() - MAX_CACHED_BUILDS);
        assert!(!dirs[0].exists(), "oldest should have gone");
        assert!(!dirs[1].exists());
        assert!(dirs[2].exists(), "newest {MAX_CACHED_BUILDS} must survive");
        assert!(dirs[3].exists());
    }

    /// Ordering is by mtime, not by name: a build-id has no order, and the
    /// version-keyed dirs an older clud left behind age out by the same rule.
    #[test]
    fn pruning_is_by_recency_not_by_name() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cache = tmp.path();
        // Created oldest-to-newest; the names sort in the opposite order.
        let dirs = seed_builds(cache, &["2.9.0", "ffff", "0000"]);

        prune_cache(cache);

        assert!(!dirs[0].exists(), "the legacy version dir is least recent");
        assert!(dirs[1].exists());
        assert!(
            dirs[2].exists(),
            "newest by mtime must survive a lexical sort"
        );
    }

    #[test]
    fn a_cache_within_the_bound_is_left_alone() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cache = tmp.path();
        let dirs = seed_builds(cache, &["aaaa", "bbbb"]);

        assert_eq!(prune_cache(cache), 0);
        assert!(dirs.iter().all(|d| d.exists()));
    }

    #[test]
    fn pruning_a_missing_cache_is_not_an_error() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        assert_eq!(prune_cache(&tmp.path().join("never-created")), 0);
    }

    /// End to end: installing into a cache that is already at the bound
    /// evicts, so exactly the 2 newest build-ids remain.
    #[test]
    fn installing_prunes_the_cache_to_two_builds() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let report = report_in(tmp.path(), "9.9.9", Some(BUILD_ID));
        let cache = tmp.path().join("symbols");
        let old = seed_builds(&cache, &["aaaa0001", "bbbb0002"]);
        let net = release(BUILD_ID, b"dwarf package bytes");

        assert_eq!(install_with(&[report], &|url| net.fetch(url), &cache), 0);

        assert!(
            cache
                .join(BUILD_ID)
                .join(sidecar_asset_name(LINUX).unwrap())
                .is_file(),
            "the new sidecar must be installed"
        );
        assert!(!old[0].exists(), "the oldest build was not evicted");
        assert!(old[1].exists(), "the second-newest build must be kept");
        let kept = fs::read_dir(&cache).unwrap().count();
        assert_eq!(kept, MAX_CACHED_BUILDS, "cache exceeded its bound");
    }
}
