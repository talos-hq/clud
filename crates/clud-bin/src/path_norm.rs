//! Cross-platform path normalization helpers.
//!
//! This is a small port of fbuild/zccache's `NormalizedPath` pattern: keep a
//! path together with a precomputed, slash-normalized comparison key so hashing
//! and ordering do not depend on the host platform's separator spelling.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone)]
pub struct NormalizedPath {
    path: Arc<Path>,
    key: Arc<str>,
}

impl NormalizedPath {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = normalize(path.as_ref());
        let key: Arc<str> = Arc::from(normalize_for_key(&path));
        let path: Arc<Path> = Arc::from(path);
        Self { path, key }
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.path.to_path_buf()
    }

    #[must_use]
    pub fn join(&self, path: impl AsRef<Path>) -> Self {
        Self::new(self.path.join(path))
    }

    /// Render the path with forward slashes for interchange strings.
    ///
    /// Use this for JSON, command-line arguments, log records, or tests that
    /// compare byte-for-byte across platforms. It is intentionally centralized
    /// so callers do not hand-roll `.replace('\\', "/")` and forget about
    /// `\\?\` prefixes or platform case rules.
    #[must_use]
    pub fn display_slash(&self) -> String {
        display_slash(self.as_path())
    }
}

impl PartialEq for NormalizedPath {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for NormalizedPath {}

impl Hash for NormalizedPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl PartialOrd for NormalizedPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NormalizedPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

impl std::fmt::Display for NormalizedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.display().fmt(f)
    }
}

impl AsRef<Path> for NormalizedPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl AsRef<OsStr> for NormalizedPath {
    fn as_ref(&self) -> &OsStr {
        self.as_path().as_os_str()
    }
}

impl Deref for NormalizedPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

impl From<PathBuf> for NormalizedPath {
    fn from(path: PathBuf) -> Self {
        Self::new(path)
    }
}

impl From<&Path> for NormalizedPath {
    fn from(path: &Path) -> Self {
        Self::new(path)
    }
}

impl From<String> for NormalizedPath {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

impl From<&str> for NormalizedPath {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl From<&String> for NormalizedPath {
    fn from(path: &String) -> Self {
        Self::new(path)
    }
}

impl Serialize for NormalizedPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.display_slash().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for NormalizedPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        PathBuf::deserialize(deserializer).map(Self::new)
    }
}

#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(Component::Normal(_)) = components.last() {
                    components.pop();
                } else {
                    components.push(component);
                }
            }
            _ => components.push(component),
        }
    }
    components.iter().collect()
}

#[must_use]
pub fn normalize_for_key(path: &Path) -> String {
    let mut s = display_slash(&normalize(path));
    if crate::platform::host::WINDOWS || crate::platform::host::MACOS {
        s.make_ascii_lowercase();
    }
    s
}

#[must_use]
pub fn display_slash(path: &Path) -> String {
    let mut s = path.to_string_lossy().into_owned();
    if let Some(stripped) = s.strip_prefix(r"\\?\UNC\") {
        s = format!(r"\\{stripped}");
    } else if let Some(stripped) = s.strip_prefix(r"\\?\") {
        s = stripped.to_string();
    }
    slash_separators(&s)
}

/// Drop a Windows verbatim (`\\?\`) prefix when the path has an ordinary
/// spelling: `\\?\C:\x` becomes `C:\x` and `\\?\UNC\srv\share` becomes
/// `\\srv\share`. Other verbatim forms (`\\?\Volume{…}`) are returned
/// unchanged, because they have no non-verbatim equivalent.
///
/// `std::fs::canonicalize` returns verbatim paths on Windows. Git (and many
/// other tools) cannot parse them — `git init \\?\C:\…` fails with "cannot
/// mkdir … Invalid argument" and a clone URL of that shape is read as a
/// hostname — so strip it before a path leaves the process (#1628). This is a
/// pure string operation, so it behaves the same on every host.
#[must_use]
pub fn strip_verbatim(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if drive && (bytes.len() == 2 || bytes[2] == b'\\') {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// `std::fs::canonicalize` without the Windows verbatim prefix; see
/// [`strip_verbatim`]. Use it whenever the result may be handed to git or
/// another process.
pub fn canonicalize_plain(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(|p| strip_verbatim(&p))
}

/// Canonicalize `path` even when it (or a tail of it) no longer exists: the
/// deepest existing ancestor is canonicalized and the missing components are
/// appended. The result carries no verbatim prefix.
///
/// Plain `canonicalize` fails on a deleted path, so a comparison that falls
/// back to the raw spelling misses Windows 8.3 short names
/// (`RUNNER~1` vs `runneradmin`) and symlinked parents (#1628).
#[must_use]
pub fn canonicalize_lenient(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut ancestor = path;
    loop {
        if let Ok(canonical) = canonicalize_plain(ancestor) {
            let mut out = canonical;
            out.extend(missing.iter().rev());
            return normalize(&out);
        }
        match (ancestor.parent(), ancestor.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                ancestor = parent;
            }
            _ => return normalize(&strip_verbatim(path)),
        }
    }
}

/// Whether two paths name the same location, robust to either side having
/// been deleted, to separator spelling, to verbatim prefixes and (on Windows
/// and macOS) to case.
#[must_use]
pub fn same_path(a: &Path, b: &Path) -> bool {
    normalize_for_key(&canonicalize_lenient(a)) == normalize_for_key(&canonicalize_lenient(b))
}

/// Normalize path separators in a path-shaped string.
///
/// This works on strings received from another platform, unlike
/// `std::path::Path` parsing, which treats `\` as a normal filename byte on
/// Unix. Use it for command names, serialized paths, and log strings, not for
/// arbitrary user text.
#[must_use]
pub fn slash_separators(raw: &str) -> String {
    raw.chars()
        .map(|ch| if ch == '\\' { '/' } else { ch })
        .collect()
}

/// Return a file stem using either `/` or `\` as a path separator.
///
/// This is for executable/path strings that may have been produced on a
/// different platform. `Path::new(r"C:\Tools\python.exe").file_stem()` returns
/// the whole string on Unix; this helper consistently returns `python`.
#[must_use]
pub fn file_stem_any_separator(raw: &str) -> Option<String> {
    let file_name = raw
        .rsplit(|ch| ['/', '\\'].contains(&ch))
        .find(|part| !part.is_empty())?;
    Path::new(file_name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

/// Strips the spellings Win32 ignores from the last component of `path`: an
/// NTFS stream suffix (everything from the first `:`, such as `::$DATA`) and
/// trailing dots and spaces. A leading drive prefix (`C:`) is left alone, and
/// a component made only of dots and spaces (`.`, `..`) is returned as is.
/// Pure string logic so it is testable on any host; [`win32_file_name`]
/// applies it on Windows only.
fn strip_win32_name_quirks(path: &str) -> String {
    let split = path.rfind(['/', '\\']).map_or(0, |i| i + 1);
    let (dir, name) = path.split_at(split);
    let (dir, name) = match name.as_bytes() {
        [drive, b':', ..] if split == 0 && drive.is_ascii_alphabetic() => (&path[..2], &path[2..]),
        _ => (dir, name),
    };
    let stem = name.split(':').next().unwrap_or(name);
    let trimmed = stem.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return path.to_string();
    }
    format!("{dir}{trimmed}")
}

/// `path` with its last component reduced to the name the host would open.
/// Win32 ignores trailing dots/spaces and NTFS stream suffixes (`::$DATA`),
/// so a path-equality guard must too; Unix takes names literally.
pub(crate) fn win32_file_name(path: &Path) -> PathBuf {
    if crate::platform::host::WINDOWS {
        strip_win32_name_quirks(&path.to_string_lossy()).into()
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod win32_name_tests {
    use super::strip_win32_name_quirks as strip;

    #[test]
    fn strips_trailing_dot_and_space_dot() {
        assert_eq!(strip("a/b/guarded.json."), "a/b/guarded.json");
        assert_eq!(strip("a\\b\\guarded.json ."), "a\\b\\guarded.json");
        assert_eq!(strip("a/guarded.json.. "), "a/guarded.json");
    }

    #[test]
    fn strips_ntfs_stream_suffixes() {
        assert_eq!(strip("a/guarded.json::$DATA"), "a/guarded.json");
        assert_eq!(strip("a/guarded.json:stream"), "a/guarded.json");
        assert_eq!(strip("a/guarded.json:stream:$DATA"), "a/guarded.json");
        assert_eq!(strip("a/guarded.json::$DATA."), "a/guarded.json");
    }

    #[test]
    fn drive_prefix_survives() {
        assert_eq!(strip("C:guarded.json::$DATA"), "C:guarded.json");
        assert_eq!(strip("C:\\x\\guarded.json:s"), "C:\\x\\guarded.json");
        assert_eq!(strip("C:"), "C:");
        assert_eq!(strip("C:\\"), "C:\\");
    }

    #[test]
    fn ordinary_names_are_unchanged() {
        for name in [
            "a/b/guarded.json",
            ".clud",
            "a/.hidden",
            ".",
            "..",
            "a/..",
            "a/b/",
            "",
        ] {
            assert_eq!(strip(name), name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn normalize_strips_dots_and_resolves_parents() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn key_never_contains_backslash() {
        let path = NormalizedPath::new(r"C:\Tools\python.exe");
        assert!(!path.key().contains('\\'), "key={}", path.key());
    }

    #[test]
    fn display_slash_strips_extended_length_prefix() {
        assert_eq!(
            display_slash(Path::new(r"\\?\C:\Users\test")),
            "C:/Users/test"
        );
        assert_eq!(
            display_slash(Path::new(r"\\?\UNC\server\share\dir")),
            "//server/share/dir"
        );
    }

    #[test]
    fn strip_verbatim_yields_a_path_git_can_parse() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\Users\t\origin.git")),
            PathBuf::from(r"C:\Users\t\origin.git")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\srv\share\d")),
            PathBuf::from(r"\\srv\share\d")
        );
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:")), PathBuf::from(r"C:"));
        // No ordinary spelling exists: left alone.
        let volume = r"\\?\Volume{0b1c}\x";
        assert_eq!(strip_verbatim(Path::new(volume)), PathBuf::from(volume));
        assert_eq!(strip_verbatim(Path::new("/tmp/x")), PathBuf::from("/tmp/x"));
    }

    #[test]
    fn same_path_holds_after_one_side_is_deleted() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("wt")).unwrap();
        // Windows runners may lack the symlink privilege; the deleted-path and
        // separator checks below still run there against the real spelling.
        let alias = crate::platform::path_norm::test_alias(&real, &tmp.path().join("alias"));
        let via_real = real.join("wt");
        let via_alias = alias.join("wt");
        assert!(same_path(&via_real, &via_alias));
        std::fs::remove_dir(&via_real).unwrap();
        // Plain canonicalize now fails on both; the parent still resolves.
        assert!(same_path(&via_real, &via_alias));
        assert!(!same_path(&via_real, &real.join("other")));
        let slashed = via_real.to_string_lossy().replace('\\', "/");
        assert!(same_path(Path::new(&slashed), &via_real));
    }

    #[test]
    fn hash_agrees_with_eq() {
        let a = NormalizedPath::new("/usr/bin/python");
        let b = NormalizedPath::new("/usr/bin/python");
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn serialize_emits_slash_form() {
        let p = NormalizedPath::new(r"C:\Tools\python.exe");
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"C:/Tools/python.exe\"");
    }

    #[test]
    fn file_stem_handles_native_and_foreign_separators() {
        assert_eq!(
            file_stem_any_separator("/usr/bin/python3").as_deref(),
            Some("python3")
        );
        assert_eq!(
            file_stem_any_separator(r"C:\Tools\python.exe").as_deref(),
            Some("python")
        );
        assert_eq!(file_stem_any_separator("").as_deref(), None);
    }
}
