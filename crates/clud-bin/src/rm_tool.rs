//! `safe-rm`: the one deletion path clud gives agents (#1461).
//!
//! `safe-rm` accepts familiar options, requiring recursion for nonempty
//! directories and `-d` for empty directories. It **trashes by default**: every call
//! moves its paths into one entry under `~/.clud/trash/`, recoverable until
//! the daemon reaps it after [`TRASH_KEEP`]. `--purge` deletes for real,
//! `--tracked` allows git-tracked paths (normally `git rm`'s job), and
//! `--dry-run` only reports.
//!
//! Every path must resolve inside one of the caller's **roots**: the
//! `CLUD_RM_ROOTS` path list clud sets for a session, or, outside a session,
//! the current git checkout (the cwd outside a repo). A session's list also
//! holds its sister checkouts: sibling git checkouts and `../<repo>-extern`
//! (#1696). A root that is a git
//! checkout also covers that repository's linked worktrees, so a `/grind`
//! integrator can clean its sibling worktree. A separate clone of an allowed
//! repo qualifies too when nothing in it would be lost (see
//! `rm_tool_clone.rs`). Entries strictly under the system temp directories
//! (`/tmp`, `/var/tmp`, `/dev/shm`, `$TMPDIR`; `%TEMP%`/`%TMP%` on Windows)
//! are always allowed, on Unix only when the caller owns them (#1622,
//! #1659); one on tmpfs/ramfs is purged, never trashed. Filesystem roots, `$HOME` and
//! its ancestors, a root itself, `.git`, and anything reached through a
//! symlinked parent that leaves the roots are always refused. Each call
//! writes one JSONL audit record under `~/.clud/state/logs/rm/`.
//!
//! The binaries are argv\[0\] aliases of `clud` (see
//! `shim_main.rs`), installed next to the session's `rm` shim; `clud
//! safe-rm` / `clud safe-rm` are the same code. The child `rm` shim
//! ([`crate::rm_guard`]) enforces only the catastrophe floor. See
//! [`docs/architecture/rm-tools.md`](../../../docs/architecture/rm-tools.md).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Path list (`std::env::split_paths` syntax) of the directories a session
/// may delete under. Set by clud for every session child.
pub const ROOTS_ENV: &str = "CLUD_RM_ROOTS";
/// Optional caller role recorded in the audit log.
pub const ROLE_ENV: &str = "CLUD_RM_ROLE";
/// Appended to every outside-the-roots refusal (#1659): `--purge` changes
/// only trash-versus-delete, never where a call may delete.
const OUTSIDE_HINT: &str = "; --purge does not relax this check. To quarantine it reversibly \
     instead, run `clud trash <PATH>` (recoverable from ~/.clud/trash until GC). Write scratch \
     data under an allowed root (the session temp dir) or a system temp dir instead, or ask the \
     user to remove this path. A trash entry this session created may be erased with \
     `safe-rm --purge <entry>`";

/// Marks a trash entry as written by `safe-rm`, and records what
/// it holds so it can be restored by hand.
pub const TRASH_MANIFEST: &str = ".clud-rm.json";
/// How long a trashed entry is kept before the daemon reaps it: the same 72
/// hours as `~/.clud/tmp` (see `gc::session_tmp::STALE_THRESHOLD`).
pub const TRASH_KEEP: Duration = crate::gc::session_tmp::STALE_THRESHOLD;

/// Default `trash.max_bytes` (#1672): past 20 GiB the daemon evicts the
/// oldest trash entries even inside [`TRASH_KEEP`]. `0` disables.
pub const TRASH_MAX_BYTES_DEFAULT: u64 = 20 * 1024 * 1024 * 1024;

pub const COMMAND: &str = "safe-rm";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Safe,
    File,
    Dir,
}

impl Kind {
    pub fn command(self) -> &'static str {
        COMMAND
    }
    pub fn from_program_name(name: &str) -> Option<Self> {
        is_program_name(name).then_some(Self::Safe)
    }
}

pub fn is_program_name(name: &str) -> bool {
    name.strip_suffix(".exe").unwrap_or(name) == COMMAND
}

/// Parsed command line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Options {
    pub recursive: bool,
    pub force: bool,
    pub verbose: bool,
    pub dir: bool,
    pub purge: bool,
    pub tracked: bool,
    pub dry_run: bool,
    pub paths: Vec<String>,
}

fn usage() -> String {
    format!(
        "usage: {COMMAND} [-rRfvd] [--purge] [--tracked] [--dry-run] [--] <path>...\n\
         Remove files, or directories with -r/-R (empty directories with -d).\n\
         Paths go to the clud trash (~/.clud/trash, kept {keep}h)\n\
         unless --purge is given, and must lie inside this session's roots\n\
         (${ROOTS_ENV}; outside a session, the current git checkout).\n\
         \n\
         \x20 --purge    delete for real instead of trashing\n\
         \x20 --tracked  allow git-tracked paths (otherwise use `git rm`)\n\
         \x20 --dry-run  report what would happen; change nothing\n\
         \x20 --grant-root <checkout> --reason <reason>  grant a sibling Git checkout for this session",
        keep = TRASH_KEEP.as_secs() / 3_600,
    )
}

/// `Ok(None)` for `--help`.
pub fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut options = Options::default();
    let mut flags = true;
    for arg in args {
        // Options come first: after the first path everything is a path, so
        // a file named `--purge` from a glob or `xargs` cannot switch modes.
        if flags && arg.starts_with('-') && arg != "-" {
            match arg.as_str() {
                "--" => flags = false,
                "--purge" => options.purge = true,
                "--tracked" => options.tracked = true,
                "--dry-run" | "-n" => options.dry_run = true,
                "--recursive" => options.recursive = true,
                "--force" => options.force = true,
                "--verbose" => options.verbose = true,
                "--help" | "-h" => return Ok(None),
                other if !other.starts_with("--") => {
                    for flag in other[1..].chars() {
                        match flag {
                            'r' | 'R' => options.recursive = true,
                            'f' => options.force = true,
                            'v' => options.verbose = true,
                            'd' => options.dir = true,
                            _ => return Err(format!("unknown option {other:?}")),
                        }
                    }
                }
                other => {
                    return Err(format!(
                        "unknown option {other:?}; put `--` before a path that starts with `-`"
                    ))
                }
            }
            continue;
        }
        flags = false;
        options.paths.push(arg.clone());
    }
    if options.paths.is_empty() && !options.force {
        return Err("missing operand".into());
    }
    Ok(Some(options))
}

/// Where a call may delete, resolved once per call.
#[derive(Debug, Clone)]
pub struct Roots {
    /// Canonical roots.
    pub roots: Vec<PathBuf>,
    /// Whether they came from [`ROOTS_ENV`] (a clud session).
    pub from_env: bool,
    /// Linked worktrees of the roots' repositories, found on first need.
    worktrees: Option<Vec<PathBuf>>,
    /// The `.git` directories (common dirs) of the roots that are checkouts.
    git_dirs: Option<Vec<PathBuf>>,
    /// `remote.origin.url` of the roots that are checkouts.
    origins: Option<Vec<String>>,
    /// Canonical system temp directories (#1622): entries strictly under
    /// them may be deleted, the directories themselves never.
    pub temp_roots: Vec<PathBuf>,
    /// The uid a temp entry must be owned by on Unix; `None` means the
    /// effective uid. Injectable so tests can simulate another user's file.
    temp_owner: Option<u32>,
    /// The temp roots on a RAM-backed filesystem (tmpfs/ramfs, #1659).
    /// Entries under them are purged, never trashed: the trash is on disk,
    /// so trashing would copy RAM-held data there and keep it for 72h.
    pub ram_roots: Vec<PathBuf>,
    /// The creation ledger consulted for a path outside every root and temp
    /// root (#1666); `None` keeps the strict roots with no ledger clause.
    ledger: Option<Arc<dyn ledger::CreationLedger>>,
    /// Paths the ledger or an extra root allowed, with the reason for the
    /// audit record.
    ledger_grants: BTreeMap<PathBuf, String>,
    /// Human-set extra roots from `safe_rm.extra_roots` (#1668), vetted.
    pub extra_roots: Vec<extra_roots::ExtraRoot>,
    /// Messages for `safe_rm.extra_roots` entries dropped at load.
    pub extra_root_warnings: Vec<String>,
    /// Paths allowed only by an extra root (a subset of `ledger_grants`).
    extra_grants: BTreeMap<PathBuf, String>,
    pub(crate) session_grants: Vec<session_grants::SessionGrant>,
    /// This session's own trash entries (#2096), canonical.
    own_trash: Vec<PathBuf>,
    /// Whether the call purges, which alone may touch [`Self::own_trash`].
    pub purge_own_trash: bool,
}

impl Roots {
    /// Roots from `env_value` (the [`ROOTS_ENV`] value) or, when it is unset
    /// or empty, the nearest git checkout of `cwd` (the cwd outside a repo).
    /// Relative and unresolvable entries are dropped: a root must exist.
    pub fn resolve(env_value: Option<&OsStr>, cwd: &Path) -> Self {
        Self::resolve_with_home(env_value, cwd, crate::home::user_home().as_deref())
    }

    /// [`Roots::resolve`] with an explicit home: `$HOME` and its ancestors
    /// are never roots, whatever launched the session there.
    pub fn resolve_with_home(env_value: Option<&OsStr>, cwd: &Path, home: Option<&Path>) -> Self {
        let home = home.and_then(|h| crate::path_norm::canonicalize_plain(h).ok());
        let usable = |root: &PathBuf| home.as_ref().is_none_or(|home| !home.starts_with(root));
        if let Some(value) = env_value.filter(|v| !v.is_empty()) {
            let mut roots: Vec<PathBuf> = std::env::split_paths(value)
                .filter(|p| p.is_absolute())
                .filter_map(|p| crate::path_norm::canonicalize_plain(p).ok())
                .filter(usable)
                .collect();
            roots.dedup();
            return Self::fixed(roots, true).with_system_temp_roots(home.as_deref());
        }
        let base = crate::block_bad_cmd::nearest_repo_root_public(cwd)
            .unwrap_or_else(|| cwd.to_path_buf());
        Self::fixed(
            crate::path_norm::canonicalize_plain(base)
                .into_iter()
                .filter(usable)
                .collect(),
            false,
        )
        .with_system_temp_roots(home.as_deref())
    }

    /// This host's temp roots, with the RAM-backed ones marked (#1659).
    fn with_system_temp_roots(self, home: Option<&Path>) -> Self {
        let temp = system_temp_roots(home);
        let ram = temp.iter().filter(|r| is_ram_backed(r)).cloned().collect();
        self.with_temp_roots(temp).with_ram_roots(ram)
    }

    /// Exactly `roots` (assumed canonical), plus their repos' worktrees.
    pub fn fixed(roots: Vec<PathBuf>, from_env: bool) -> Self {
        Self {
            roots,
            from_env,
            worktrees: None,
            git_dirs: None,
            origins: None,
            temp_roots: Vec::new(),
            temp_owner: None,
            ram_roots: Vec::new(),
            ledger: None,
            ledger_grants: BTreeMap::new(),
            extra_roots: Vec::new(),
            extra_root_warnings: Vec::new(),
            extra_grants: BTreeMap::new(),
            session_grants: Vec::new(),
            own_trash: Vec::new(),
            purge_own_trash: false,
        }
    }

    /// Consult `ledger` for paths outside every root and temp root (#1666).
    #[must_use]
    pub fn with_ledger(mut self, ledger: Arc<dyn ledger::CreationLedger>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Honor `roots` as human-set extra roots (#1668); `warnings` are the
    /// messages for dropped settings entries, printed by every call.
    #[must_use]
    pub fn with_extra_roots(
        mut self,
        roots: Vec<extra_roots::ExtraRoot>,
        warnings: Vec<String>,
    ) -> Self {
        self.extra_roots = roots;
        self.extra_root_warnings = warnings;
        self
    }

    #[must_use]
    pub fn with_session_grants(mut self, grants: Vec<session_grants::SessionGrant>) -> Self {
        self.session_grants = grants;
        self
    }

    /// Let `--purge` erase `entries`, this session's trash entries (#2096).
    #[must_use]
    pub fn with_own_trash(mut self, entries: Vec<PathBuf>) -> Self {
        self.own_trash = entries;
        self
    }

    /// The override reason that allowed `path`, if only an extra root did.
    pub fn extra_grant(&self, path: &Path) -> Option<&str> {
        self.extra_grants.get(path).map(String::as_str)
    }

    /// The ledger reason that allowed `path`, if the ledger did.
    pub fn ledger_grant(&self, path: &Path) -> Option<&str> {
        self.ledger_grants.get(path).map(String::as_str)
    }

    /// The uid entries must belong to on Unix; `None` elsewhere.
    fn owner_uid(&self) -> Option<u32> {
        if crate::platform::host::UNIX {
            self.temp_owner.or_else(effective_uid)
        } else {
            None
        }
    }

    /// These roots plus `temp` as system temp directories (#1622).
    #[must_use]
    pub fn with_temp_roots(mut self, temp: Vec<PathBuf>) -> Self {
        self.temp_roots = temp;
        self
    }

    /// Treat `ram` (a subset of the temp roots) as RAM-backed (#1659).
    #[must_use]
    pub fn with_ram_roots(mut self, ram: Vec<PathBuf>) -> Self {
        self.ram_roots = ram;
        self
    }

    /// Whether a target under `root` must be purged rather than trashed.
    pub fn is_ram_root(&self, root: &Path) -> bool {
        self.ram_roots.iter().any(|r| r == root)
    }

    /// Require temp entries to be owned by `uid` instead of the effective uid.
    #[must_use]
    pub fn with_temp_owner(mut self, uid: u32) -> Self {
        self.temp_owner = Some(uid);
        self
    }

    /// The deepest root strictly containing `path`, or why there is none.
    fn containing(&mut self, path: &Path) -> Result<PathBuf, String> {
        if let Some(result) = self.own_trash_for(path) {
            return result;
        }
        if let Some(root) = deepest_containing(&self.roots, path) {
            return Ok(root);
        }
        if let Some(root) = deepest_containing(self.worktrees(), path) {
            return Ok(root);
        }
        if self.roots.iter().any(|r| r == path) {
            return Err("is an allowed root itself".into());
        }
        // Strictly under a system temp directory (#1622): allowed when every
        // existing entry from the temp root down is the caller's own.
        if let Some(temp) = deepest_containing(&self.temp_roots, path) {
            self.check_owned_under("the system temp directory", &temp, path)?;
            return Ok(temp);
        }
        if self.temp_roots.iter().any(|r| r == path) {
            return Err("is a system temp directory itself".into());
        }
        if let Some(result) = self.session_grant_for(path) {
            return result;
        }
        // Strictly under a human-set extra root (#1668): the temp-root rules,
        // and the override's reason goes into the audit records.
        let extra = self
            .extra_roots
            .iter()
            .filter(|e| path.starts_with(&e.root) && path != e.root.as_path())
            .max_by_key(|e| e.root.components().count())
            .cloned();
        if let Some(extra) = extra {
            self.check_owned_under("the safe_rm.extra_roots override", &extra.root, path)?;
            let reason = extra_roots::grant_reason(&extra);
            self.ledger_grants
                .insert(path.to_path_buf(), reason.clone());
            self.extra_grants.insert(path.to_path_buf(), reason);
            return Ok(extra.root);
        }
        if self.extra_roots.iter().any(|e| e.root == path) {
            return Err("is a safe_rm.extra_roots extra root itself".into());
        }
        // A Python virtualenv is disposable wherever it lives: any path at or
        // under a component named exactly `.venv` may be trashed, rooted at
        // that `.venv`'s parent. The git, home and mount checks still apply.
        if let Some(venv) = venv_ancestor(path) {
            self.ledger_grants
                .insert(path.to_path_buf(), VENV_REASON.to_string());
            return Ok(venv.parent().map_or(venv.clone(), Path::to_path_buf));
        }
        // A linked worktree of an allowed repo may itself be deleted (#1573):
        // git created it for that repo. It stays refused while it is a root.
        if self.worktrees().iter().any(|r| r == path) {
            return path
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| "is an allowed root itself".to_string());
        }
        // So may one git already dropped from `git worktree list` (a removal
        // that failed partway leaves the directory): its `.git` file still
        // names the allowed repo's worktree metadata.
        if let Some(dir) = self.half_removed_worktree(path) {
            return Ok(dir.parent().map_or(dir.clone(), Path::to_path_buf));
        }
        // And so may a separate clone of an allowed repo, but only when
        // nothing in it would be lost (#1573).
        let clone_refusal = match self.clone_of_allowed_repo(path) {
            Some(Ok(dir)) => return Ok(dir.parent().map_or(dir.clone(), Path::to_path_buf)),
            Some(Err(reason)) => Some(reason),
            None => None,
        };
        let shown: Vec<String> = self
            .roots
            .iter()
            .map(|r| r.display().to_string())
            .chain(
                self.temp_roots
                    .iter()
                    .map(|r| format!("{} (temp)", r.display())),
            )
            .chain(
                self.extra_roots
                    .iter()
                    .map(|e| format!("{} (override)", e.root.display())),
            )
            .collect();
        let shown = if shown.is_empty() {
            "none".to_string()
        } else {
            shown.join(", ")
        };
        // Outside every root and temp root: the creation ledger (#1666).
        let ledger_clause = match &self.ledger {
            Some(source) => {
                let facts = ledger::probe(path, source.as_ref(), self.owner_uid());
                match ledger::verdict(&facts) {
                    Ok(reason) => {
                        self.ledger_grants.insert(path.to_path_buf(), reason);
                        return Ok(path.parent().map_or(path.to_path_buf(), Path::to_path_buf));
                    }
                    Err(clause) => format!("; {clause}"),
                }
            }
            None => String::new(),
        };
        let refusal = match clone_refusal {
            Some(reason) => format!(
                "is outside the allowed roots ({shown}), and is not a clone of an allowed repo \
                 with nothing to lose: {reason}"
            ),
            None => format!("is outside the allowed roots ({shown})"),
        };
        Err(refusal + &ledger_clause + OUTSIDE_HINT)
    }

    /// A path under `home` outside every root (#1956, #1957): a file or
    /// symlink, or a directory inside a tool data dir. `Some(Ok(parent))`
    /// allows it, `Some(Err)` is a denylist refusal and `None` leaves the
    /// caller's refusal standing.
    fn home_file_grant(
        &mut self,
        path: &Path,
        home: Option<&Path>,
    ) -> Option<Result<PathBuf, String>> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        let is_dir = !meta.file_type().is_symlink() && meta.is_dir();
        match home_files::check(path, is_dir, home) {
            Ok(false) => None,
            Err(refusal) => Some(Err(refusal)),
            Ok(true) => {
                let home = crate::path_norm::canonicalize_plain(home?).ok()?;
                self.check_owned_under("your home directory", &home, path)
                    .ok()?;
                self.ledger_grants
                    .insert(path.to_path_buf(), home_files::GRANT_REASON.into());
                Some(Ok(path
                    .parent()
                    .map_or(path.to_path_buf(), Path::to_path_buf)))
            }
        }
    }

    /// `path` inside one of this session's trash entries (#2096): allowed
    /// only for `--purge`, since trashing it again just makes a new entry.
    fn own_trash_for(&mut self, path: &Path) -> Option<Result<PathBuf, String>> {
        let entry = self.own_trash.iter().find(|e| path.starts_with(e))?.clone();
        if !self.purge_own_trash {
            return Some(Err(own_trash::NEEDS_PURGE.into()));
        }
        self.ledger_grants
            .insert(path.to_path_buf(), own_trash::REASON.into());
        Some(Ok(entry.parent().map_or(entry.clone(), Path::to_path_buf)))
    }

    fn session_grant_for(&mut self, path: &Path) -> Option<Result<PathBuf, String>> {
        if let Some(grant) = self
            .session_grants
            .iter()
            .find(|grant| path.starts_with(&grant.root) && path != grant.root)
            .cloned()
        {
            if let Err(error) = self.check_owned_under("the session root grant", &grant.root, path)
            {
                return Some(Err(error));
            }
            self.ledger_grants
                .insert(path.to_path_buf(), session_grants::reason(&grant));
            return Some(Ok(grant.root));
        }
        self.session_grants
            .iter()
            .any(|grant| grant.root == path)
            .then(|| Err("is a granted checkout root itself".into()))
    }

    /// Refuse `path` under the temp root `temp` when an existing entry
    /// between them (the target included) belongs to another user. `/tmp` is
    /// shared and world-writable; its sticky bit already stops unlinking
    /// another user's entry directly in it, but not a file inside a directory
    /// someone left writable, so safe-rm checks ownership itself. Windows has
    /// no equivalent: `%TEMP%` is per-user under the profile, so no check.
    fn check_owned_under(&self, what: &str, temp: &Path, path: &Path) -> Result<(), String> {
        let Some(me) = self.owner_uid() else {
            return Ok(());
        };
        let Ok(rest) = path.strip_prefix(temp) else {
            return Ok(());
        };
        let mut current = temp.to_path_buf();
        for part in rest.components() {
            current.push(part);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if crate::platform::rm_tool::file_owner(&meta) != Some(me) => {
                    return Err(format!(
                        "is under {what} {} but {} is not owned by you",
                        temp.display(),
                        current.display()
                    ));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// Whether `root` is itself the top of a git checkout.
    ///
    /// A `.git` entry is not enough: when it is not a valid repository git
    /// walks up to an enclosing one, and that repo's worktrees and git dir
    /// would then widen the roots. Git's own top level must be `root`.
    fn is_checkout(root: &Path) -> bool {
        let marker = crate::block_bad_cmd::nearest_repo_root_public(root)
            .and_then(|r| crate::path_norm::canonicalize_plain(r).ok())
            .is_some_and(|r| r == root);
        marker
            && crate::worktrees::run_git(root, &["rev-parse", "--show-toplevel"])
                .ok()
                .and_then(|top| crate::path_norm::canonicalize_plain(top.trim()).ok())
                .is_some_and(|top| top == root)
    }

    /// The `origin` URLs of the roots that are checkouts.
    fn origins(&mut self) -> &[String] {
        if self.origins.is_none() {
            let mut found: Vec<String> = Vec::new();
            for root in self.roots.iter().filter(|r| Self::is_checkout(r)) {
                let Ok(url) =
                    crate::worktrees::run_git(root, &["config", "--get", "remote.origin.url"])
                else {
                    continue;
                };
                let url = url.trim().to_string();
                if !url.is_empty() && !found.contains(&url) {
                    found.push(url);
                }
            }
            self.origins = Some(found);
        }
        self.origins.as_deref().unwrap_or_default()
    }

    /// For `path` in (or equal to) a clone outside the roots: `Ok(clone)` when
    /// it is a clone of an allowed repo with nothing to lose, `Err(reason)`
    /// naming the failing check otherwise. `None` when `path` is in no clone.
    fn clone_of_allowed_repo(&mut self, path: &Path) -> Option<Result<PathBuf, String>> {
        // The nearest enclosing checkout, which must be a standalone clone
        // (a `.git` directory, not a worktree's or submodule's `.git` file).
        let dir = path
            .ancestors()
            .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())?
            .to_path_buf();
        if !std::fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| m.is_dir()) {
            return None;
        }
        let home =
            crate::home::user_home().and_then(|h| crate::path_norm::canonicalize_plain(h).ok());
        if home.as_ref().is_some_and(|home| home.starts_with(&dir)) {
            return None;
        }
        if self.roots.iter().any(|root| root.starts_with(&dir)) {
            return Some(Err("it contains an allowed root".into()));
        }
        let origins = self.origins().to_vec();
        let verdict = clone::probe(&dir, &origins)
            .map_err(|error| format!("cannot inspect it: {error}"))
            .and_then(|facts| clone::verdict(&facts));
        Some(verdict.map(|()| dir))
    }

    /// The `.git` directories of the roots that are themselves checkouts.
    fn git_dirs(&mut self) -> &[PathBuf] {
        if self.git_dirs.is_none() {
            let mut found: Vec<PathBuf> = Vec::new();
            for root in self.roots.iter().filter(|r| Self::is_checkout(r)) {
                let Ok(text) = crate::worktrees::run_git(root, &["rev-parse", "--git-common-dir"])
                else {
                    continue;
                };
                let dir = root.join(text.trim());
                if let Ok(dir) = crate::path_norm::canonicalize_plain(dir) {
                    if !found.contains(&dir) {
                        found.push(dir);
                    }
                }
            }
            self.git_dirs = Some(found);
        }
        self.git_dirs.as_deref().unwrap_or_default()
    }

    /// The worktree directory containing (or equal to) `path` whose `.git`
    /// file points into an allowed repo's `.git/worktrees/`, or `None`.
    fn half_removed_worktree(&mut self, path: &Path) -> Option<PathBuf> {
        let git_dirs = self.git_dirs().to_vec();
        if git_dirs.is_empty() {
            return None;
        }
        path.ancestors()
            .take_while(|dir| !self.roots.iter().any(|r| r == dir))
            .find(|dir| {
                let Ok(text) = std::fs::read_to_string(dir.join(".git")) else {
                    return false;
                };
                let Some(target) = text.trim().strip_prefix("gitdir:") else {
                    return false;
                };
                let target = dir.join(target.trim());
                let Some(name) = target.file_name() else {
                    return false;
                };
                let Some(metadata_dir) = target.parent() else {
                    return false;
                };
                let metadata_dir = crate::path_norm::canonicalize_plain(metadata_dir)
                    .unwrap_or_else(|_| metadata_dir.to_path_buf());
                !name.is_empty()
                    && git_dirs
                        .iter()
                        .any(|git_dir| metadata_dir == git_dir.join("worktrees"))
            })
            .map(Path::to_path_buf)
    }

    fn worktrees(&mut self) -> &[PathBuf] {
        if self.worktrees.is_none() {
            let mut found = Vec::new();
            // Only a root that is itself a checkout brings its worktrees; a
            // directory inside some repo (say, a dotfiles `$HOME`) must not
            // widen to that repo.
            for root in self.roots.iter().filter(|r| Self::is_checkout(r)) {
                let Ok(text) =
                    crate::worktrees::run_git(root, &["worktree", "list", "--porcelain"])
                else {
                    continue;
                };
                for line in text.lines() {
                    if let Some(path) = line.strip_prefix("worktree ") {
                        if let Ok(path) = crate::path_norm::canonicalize_plain(path.trim()) {
                            let home = crate::home::user_home()
                                .and_then(|h| crate::path_norm::canonicalize_plain(h).ok());
                            if home.as_ref().is_some_and(|home| home.starts_with(&path)) {
                                continue;
                            }
                            if !found.contains(&path) {
                                found.push(path);
                            }
                        }
                    }
                }
            }
            self.worktrees = Some(found);
        }
        self.worktrees.as_deref().unwrap_or_default()
    }
}

/// Audit reason recorded when a path is allowed only because it is in a `.venv`.
const VENV_REASON: &str = "inside a .venv virtualenv, which is disposable anywhere";

/// The outermost ancestor-or-self of `path` named exactly `.venv`.
fn venv_ancestor(path: &Path) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        if component.as_os_str() == ".venv" {
            return Some(prefix);
        }
    }
    None
}

fn deepest_containing(roots: &[PathBuf], path: &Path) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| path.starts_with(root) && path != root.as_path())
        .max_by_key(|root| root.components().count())
        .cloned()
}

/// A path that exists and may be deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Canonical parent joined with the final name (not followed).
    pub path: PathBuf,
    /// The root that contains it.
    pub root: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// Why the creation ledger allowed it, when it lies outside every root
    /// (#1666), or which `safe_rm.extra_roots` entry did (#1668); recorded
    /// in the audit log.
    pub ledger: Option<String>,
    /// Set only when a `safe_rm.extra_roots` override alone allowed it
    /// (#1668): its reason also goes into the GC delete audit.
    pub override_reason: Option<String>,
}

/// What [`resolve`] found for one operand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Present(Target),
    /// Nothing there; carries the absolute (lexical) path.
    Missing(PathBuf),
}

/// Resolve one operand against `cwd` and check it may be deleted.
///
/// The parent is canonicalized (so a symlinked directory on the way is
/// followed and must itself land inside the roots); the final name is never
/// followed, so deleting a symlink removes the link, not its target.
pub fn resolve(
    raw: &str,
    cwd: &Path,
    home: Option<&Path>,
    roots: &mut Roots,
) -> Result<Resolved, String> {
    if raw.is_empty() {
        return Err("empty path".into());
    }
    // `Path::components` drops a trailing `.`, so check the operand as typed.
    let last = raw.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next();
    if matches!(last, Some("." | "..")) {
        return Err("does not name a file or directory (`.` or `..`); name it directly".into());
    }
    let native = if crate::platform::host::WINDOWS {
        msys_drive_path(raw).unwrap_or_else(|| raw.to_owned())
    } else {
        raw.to_owned()
    };
    let absolute = if Path::new(&native).is_absolute() {
        PathBuf::from(&native)
    } else {
        cwd.join(&native)
    };
    let Some(Component::Normal(name)) = absolute.components().next_back() else {
        return Err(
            "does not name a file or directory (a root, `.` or `..`); name it directly".into(),
        );
    };
    let name = name.to_os_string();
    if absolute
        .components()
        .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git"))
    {
        return Err("is git metadata".into());
    }
    let parent = absolute.parent().ok_or("has no parent directory")?;
    let parent = match crate::path_norm::canonicalize_plain(parent) {
        Ok(parent) => parent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Still hold a missing path to the roots: on Windows `/etc/x`
            // becomes `C:\etc\x`, whose parent is usually absent.
            let path = canonical_with_missing_tail(&absolute)?;
            roots.containing(&path)?;
            return Ok(Resolved::Missing(path));
        }
        Err(error) => return Err(format!("cannot resolve its directory: {error}")),
    };
    let path = parent.join(&name);
    if path
        .components()
        .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git"))
    {
        return Err("is git metadata".into());
    }
    if let Some(home) = home.and_then(|h| crate::path_norm::canonicalize_plain(h).ok()) {
        if home.starts_with(&path) {
            return Err("is your home directory or one of its ancestors".into());
        }
    }
    let root = match roots.containing(&path) {
        Ok(root) => root,
        Err(refusal) => roots.home_file_grant(&path, home).unwrap_or(Err(refusal))?,
    };
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Resolved::Missing(path));
        }
        Err(error) => return Err(format!("cannot stat: {error}")),
    };
    let is_symlink = meta.file_type().is_symlink();
    let is_dir = !is_symlink && meta.is_dir();
    if is_dir {
        reject_mounts(&path)?;
    }
    let ledger = roots.ledger_grant(&path).map(str::to_string);
    let override_reason = roots.extra_grant(&path).map(str::to_string);
    Ok(Resolved::Present(Target {
        path,
        root,
        is_dir,
        is_symlink,
        ledger,
        override_reason,
    }))
}

/// `path` with its deepest existing ancestor canonicalized and the missing
/// components re-appended, so it compares against the canonical roots.
pub(crate) fn canonical_with_missing_tail(path: &Path) -> Result<PathBuf, String> {
    let mut tail = Vec::new();
    let mut ancestor = path;
    loop {
        match crate::path_norm::canonicalize_plain(ancestor) {
            Ok(base) => return Ok(tail.iter().rev().fold(base, |acc, part| acc.join(part))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tail.push(ancestor.file_name().ok_or("has no existing ancestor")?);
                ancestor = ancestor.parent().ok_or("has no existing ancestor")?;
            }
            Err(error) => return Err(format!("cannot resolve its directory: {error}")),
        }
    }
}

pub(crate) fn msys_drive_path(raw: &str) -> Option<String> {
    let normalized = crate::path_norm::slash_separators(raw);
    let bytes = normalized.as_bytes();
    if bytes.len() < 2
        || bytes[0] != b'/'
        || !bytes[1].is_ascii_alphabetic()
        || (bytes.len() > 2 && bytes[2] != b'/')
    {
        return None;
    }
    let drive = (bytes[1] as char).to_ascii_uppercase();
    let suffix = if bytes.len() == 2 {
        "/"
    } else {
        &normalized[2..]
    };
    Some(format!("{drive}:{suffix}"))
}

fn reject_mounts(target: &Path) -> Result<(), String> {
    crate::platform::rm_tool::reject_mounts(target)
}

/// Whether `target` is, or contains, a git-tracked path.
pub fn is_tracked(target: &Target) -> bool {
    let (Some(parent), Some(name)) = (target.path.parent(), target.path.file_name()) else {
        return false;
    };
    if crate::block_bad_cmd::nearest_repo_root_public(parent).is_none() {
        return false;
    }
    let name = name.to_string_lossy();
    crate::worktrees::run_git(parent, &["ls-files", "--error-unmatch", "--", &name]).is_ok()
}

/// Everything a call needs from its environment, injectable for tests.
#[derive(Debug, Clone)]
pub struct Context {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub trash_root: PathBuf,
    /// `~/.clud/state/logs/rm`; `None` skips the audit record.
    pub audit_dir: Option<PathBuf>,
    pub session_id: Option<String>,
    pub role: String,
    /// Register the trash entry with the daemon's GC registry.
    pub register: bool,
    pub now: SystemTime,
}

impl Context {
    pub fn from_env() -> Result<(Self, Roots), String> {
        let cwd = std::env::current_dir().map_err(|e| format!("cannot resolve cwd: {e}"))?;
        let home = crate::home::user_home();
        let trash_root = crate::daemon::default_trash_dir()
            .map_err(|e| format!("cannot resolve the clud trash: {e}"))?;
        let audit_dir = crate::daemon::default_state_dir()
            .ok()
            .map(|state| state.join("logs").join("rm"));
        let roots_env = std::env::var_os(ROOTS_ENV);
        let session_id = session_id_from_env();
        let source = ledger::DaemonLedger {
            state_dir: crate::daemon::default_state_dir().ok(),
            session_id: session_id.clone(),
        };
        // #1668: the human-set override, from the user's settings file only.
        let (extra, warnings) = match home.as_deref() {
            Some(home_path) => {
                let (entries, rejected) =
                    crate::clud_settings::peek_safe_rm_extra_roots_at(home_path);
                extra_roots::load(&entries, rejected, Some(home_path), effective_uid())
            }
            None => (Vec::new(), Vec::new()),
        };
        let mut roots = Roots::resolve(roots_env.as_deref(), &cwd)
            .with_ledger(Arc::new(source))
            .with_extra_roots(extra, warnings);
        if let (Some(session), Ok(state)) =
            (session_id.as_deref(), crate::daemon::default_state_dir())
        {
            let grants = session_grants::load(&state, session, &roots.roots);
            roots = roots.with_session_grants(grants);
        }
        if let Some(session) = session_id.as_deref() {
            roots = roots.with_own_trash(own_trash::load(&trash_root, session));
        }
        let role = std::env::var(ROLE_ENV)
            .ok()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| if roots.from_env { "agent" } else { "user" }.to_string());
        Ok((
            Self {
                cwd,
                home,
                trash_root,
                audit_dir,
                session_id,
                role,
                register: true,
                now: SystemTime::now(),
            },
            roots,
        ))
    }
}

/// The session id the creation ledger is keyed by: `CLUD_SESSION_ID`, else
/// Claude Code's own session id. Sub-agents inherit the parent's env, so they
/// resolve the same id (#1667).
pub fn session_id_from_env() -> Option<String> {
    ["CLUD_SESSION_ID", crate::grind_facts::SESSION_ENV]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
}

fn effective_uid() -> Option<u32> {
    crate::platform::rm_tool::effective_uid()
}

/// One path's outcome, for output and the audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub path: String,
    pub action: &'static str,
    pub reason: Option<String>,
    pub trash_path: Option<PathBuf>,
}

/// `safe-rm` from the process environment. Returns the exit code.
pub fn run(args: &[String]) -> i32 {
    if args.first().is_some_and(|arg| arg == "--grant-root") {
        return run_grant(args);
    }
    let options = match parse_args(args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("{}", usage());
            return 0;
        }
        Err(error) => {
            eprintln!("{COMMAND}: {error}\n{}", usage());
            return if error == "missing operand" { 1 } else { 2 };
        }
    };
    let (ctx, mut roots) = match Context::from_env() {
        Ok(found) => found,
        Err(error) => {
            eprintln!("{COMMAND}: {error}");
            return 1;
        }
    };
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    run_with(Kind::Safe, &options, &ctx, &mut roots, &mut out, &mut err)
}

fn run_grant(args: &[String]) -> i32 {
    let [_, root, flag, reason] = args else {
        eprintln!("{COMMAND}: usage: {COMMAND} --grant-root <sibling-checkout> --reason <authorization reason>");
        return 2;
    };
    if flag != "--reason" {
        eprintln!("{COMMAND}: expected --reason after sibling checkout");
        return 2;
    }
    let Some(session_id) = session_id_from_env() else {
        eprintln!("{COMMAND}: no clud session id; launch through clud, or set CLUD_SESSION_ID to a unique value before starting this session");
        return 1;
    };
    let Ok(cwd) = std::env::current_dir() else {
        eprintln!("{COMMAND}: cannot resolve current directory");
        return 1;
    };
    let roots = Roots::resolve(std::env::var_os(ROOTS_ENV).as_deref(), &cwd);
    let Ok(state) = crate::daemon::default_state_dir() else {
        eprintln!("{COMMAND}: cannot resolve clud state directory");
        return 1;
    };
    match session_grants::grant(Path::new(root), reason, &session_id, &roots.roots, &state) {
        Ok(root) => {
            println!("{COMMAND}: granted {} for this session", root.display());
            0
        }
        Err(error) => {
            eprintln!("{COMMAND}: {error}");
            1
        }
    }
}

/// The testable core of [`run`].
#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
pub fn run_with(
    kind: Kind,
    options: &Options,
    ctx: &Context,
    roots: &mut Roots,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let cmd = COMMAND;
    // #1668: a dropped `safe_rm.extra_roots` entry is said on every call, so
    // the user sees why the override they wrote is not honored.
    for warning in &roots.extra_root_warnings {
        let _ = writeln!(err, "{cmd}: {warning}");
    }
    roots.purge_own_trash = options.purge;
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut accepted: Vec<Target> = Vec::new();
    for raw in &options.paths {
        let refuse = |reason: String| Outcome {
            path: raw.clone(),
            action: "refused",
            reason: Some(reason),
            trash_path: None,
        };
        let target = match resolve(raw, &ctx.cwd, ctx.home.as_deref(), roots) {
            Ok(Resolved::Present(target)) => target,
            Ok(Resolved::Missing(path)) => {
                // `find … -exec safe-rm {} +` can list a path whose ancestor
                // this same call already took: that is not an error.
                if accepted.iter().any(|t| path.starts_with(&t.path)) {
                    outcomes.push(Outcome {
                        path: raw.clone(),
                        action: "skipped",
                        reason: Some("already inside a removed directory".into()),
                        trash_path: None,
                    });
                } else if !options.force {
                    outcomes.push(refuse("no such file or directory".into()));
                } else {
                    outcomes.push(Outcome {
                        path: raw.clone(),
                        action: "skipped",
                        reason: Some("missing with -f".into()),
                        trash_path: None,
                    });
                }
                continue;
            }
            Err(reason) => {
                outcomes.push(refuse(reason));
                continue;
            }
        };
        if accepted.iter().any(|t| target.path.starts_with(&t.path)) {
            outcomes.push(Outcome {
                path: raw.clone(),
                action: "skipped",
                reason: Some("already inside a removed directory".into()),
                trash_path: None,
            });
            continue;
        }
        let recursive = options.recursive || kind == Kind::Dir;
        if target.is_dir && !recursive {
            if !options.dir {
                outcomes.push(refuse("is a directory".into()));
                continue;
            }
            if std::fs::read_dir(&target.path).is_ok_and(|mut entries| entries.next().is_some()) {
                outcomes.push(refuse("directory not empty".into()));
                continue;
            }
        } else if kind == Kind::Dir && !target.is_dir {
            outcomes.push(refuse("is not a directory".into()));
            continue;
        }
        if !options.tracked && is_tracked(&target) {
            outcomes.push(refuse(if target.is_dir {
                "contains git-tracked files; use `git rm -r`, or pass --tracked".into()
            } else {
                "is tracked by git; use `git rm`, or pass --tracked".into()
            }));
            continue;
        }
        // `find -depth` lists children first: the ancestor takes them over.
        accepted.retain(|t| {
            let inside = t.path.starts_with(&target.path);
            if inside {
                outcomes.push(Outcome {
                    path: t.path.display().to_string(),
                    action: "skipped",
                    reason: Some("inside a directory removed by the same call".into()),
                    trash_path: None,
                });
            }
            !inside
        });
        accepted.push(target);
    }

    let mut entry: Option<PathBuf> = None;
    let mut root_dirs: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    let mut moved: Vec<(String, PathBuf)> = Vec::new();
    for target in &accepted {
        // RAM-backed temp roots are always purged (#1659).
        let purge_it = options.purge || roots.is_ram_root(&target.root);
        let shown = target.path.display().to_string();
        let shown = if crate::platform::host::WINDOWS {
            shown.strip_prefix(r"\\?\").unwrap_or(&shown).to_string()
        } else {
            shown
        };
        let outcome = if options.dry_run {
            Outcome {
                path: shown.clone(),
                action: if purge_it {
                    "would-purge"
                } else {
                    "would-trash"
                },
                reason: target.ledger.clone(),
                trash_path: None,
            }
        } else if purge_it {
            match &target.override_reason {
                Some(reason) => crate::gc::delete_audit::record(
                    "rm-tool.purge",
                    &target.path,
                    &format!("{cmd} {reason}"),
                ),
                None => crate::gc::delete_audit::record("rm-tool.purge", &target.path, cmd),
            }
            match purge(target) {
                Ok(()) => Outcome {
                    path: shown.clone(),
                    action: "purged",
                    reason: target.ledger.clone(),
                    trash_path: None,
                },
                Err(error) => Outcome {
                    path: shown.clone(),
                    action: "failed",
                    reason: Some(error),
                    trash_path: None,
                },
            }
        } else {
            // An override-only trash move is audited too: it leaves a path
            // the session roots would have refused (#1668).
            if let Some(reason) = &target.override_reason {
                crate::gc::delete_audit::record(
                    "rm-tool.trash-override",
                    &target.path,
                    &format!("{cmd} {reason}"),
                );
            }
            let result = entry_dir(&mut entry, &ctx.trash_root, target, ctx.now).and_then(|dir| {
                let dest = dir.join(root_dir_name(&mut root_dirs, &target.root)).join(
                    target
                        .path
                        .strip_prefix(&target.root)
                        .unwrap_or(&target.path),
                );
                if target.is_dir {
                    make_writable(&target.path);
                }
                move_into_trash(&target.path, &dest).map(|()| dest)
            });
            match result {
                Ok(dest) => {
                    moved.push((shown.clone(), dest.clone()));
                    Outcome {
                        path: shown.clone(),
                        action: "trashed",
                        reason: target.ledger.clone(),
                        trash_path: Some(dest),
                    }
                }
                Err(error) => Outcome {
                    path: shown.clone(),
                    action: "failed",
                    reason: Some(error),
                    trash_path: None,
                },
            }
        };
        outcomes.push(outcome);
    }

    if let Some(dir) = &entry {
        if moved.is_empty() {
            let _ = std::fs::remove_dir_all(dir);
        } else if write_manifest(dir, ctx, &moved) {
            if ctx.register {
                register_entry(dir, &moved[0].0, ctx.now);
            }
        } else {
            // Without its manifest the daemon would reap the entry at once,
            // so leave it unregistered and say where the files went.
            let _ = writeln!(
                err,
                "{cmd}: could not write {} in {}; the files are there but not kept for {}h",
                TRASH_MANIFEST,
                dir.display(),
                TRASH_KEEP.as_secs() / 3_600
            );
        }
    }

    let mut failed = false;
    for outcome in &outcomes {
        match outcome.action {
            "refused" | "failed" => {
                failed = true;
                let _ = writeln!(
                    err,
                    "{cmd}: {} {}: {}",
                    outcome.action,
                    outcome.path,
                    outcome.reason.as_deref().unwrap_or("")
                );
            }
            "trashed" if options.verbose => {
                let _ = writeln!(out, "removed '{}'", outcome.path);
            }
            "trashed" => {}
            "skipped" => {}
            action => {
                let _ = writeln!(out, "{action} {}", outcome.path);
            }
        }
    }
    let code = i32::from(failed);
    if let Some(dir) = &ctx.audit_dir {
        audit(dir, options, ctx, roots, &outcomes, code);
    }
    code
}

/// Make every directory under `path` (and `path` itself) writable, and on
/// Windows every file. A sealed build output (read-only `target/` trees)
/// otherwise fails a move to the trash, or a purge, halfway (#1573). Symlinks
/// are never followed.
pub(crate) fn make_writable(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mut permissions = meta.permissions();
    if meta.is_dir() {
        crate::platform::rm_tool::make_dir_permissions_writable(&mut permissions);
        let _ = std::fs::set_permissions(path, permissions);
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                make_writable(&entry.path());
            }
        }
    } else {
        crate::platform::rm_tool::make_file_writable(path, permissions);
    }
}

/// Remove a directory tree after making it owner-writable: sealed build output
/// is read-only (#1573). The one recursive deleter shared by every gc path that
/// owns a tree (trash reaping, `~/.clud/tmp` eviction). Files owned by another
/// user (root, from a container bind mount) still fail.
pub(crate) fn remove_dir_tree_writable(path: &Path) -> std::io::Result<()> {
    make_writable(path);
    std::fs::remove_dir_all(path)
}

/// [`remove_dir_tree_writable`] that keeps going past undeletable entries and
/// hands each failure (a leaf, never a parent left non-empty by one) to `fail`,
/// so a caller can print one summary (`gc::undeletable`) instead of an error
/// per file. Still the same single deleter: the fast path is the plain call.
pub(crate) fn remove_dir_tree_collecting(
    path: &Path,
    fail: &mut dyn FnMut(&Path, &std::io::Error),
) {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        _ => {}
    }
    if remove_dir_tree_writable(path).is_ok() {
        return;
    }
    if remove_children_collecting(path, fail) {
        if let Err(error) = std::fs::remove_dir(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                fail(path, &error);
            }
        }
    }
}

/// Remove everything under `dir`; `true` when nothing under it failed.
fn remove_children_collecting(dir: &Path, fail: &mut dyn FnMut(&Path, &std::io::Error)) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            fail(dir, &error);
            return false;
        }
    };
    let mut clean = true;
    for entry in entries.flatten() {
        let child = entry.path();
        let is_dir = std::fs::symlink_metadata(&child).is_ok_and(|m| m.is_dir());
        let result = if is_dir {
            if remove_children_collecting(&child, fail) {
                std::fs::remove_dir(&child)
            } else {
                clean = false;
                continue;
            }
        } else {
            remove_link_or_file(&child)
        };
        if let Err(error) = result {
            if error.kind() != std::io::ErrorKind::NotFound {
                fail(&child, &error);
                clean = false;
            }
        }
    }
    clean
}

fn purge(target: &Target) -> Result<(), String> {
    if target.is_dir {
        make_writable(&target.path);
    }
    let result = if target.is_dir {
        std::fs::remove_dir_all(&target.path)
    } else {
        remove_link_or_file(&target.path)
    };
    result.map_err(|e| e.to_string())
}

/// Remove a file or a symlink (on Windows a directory symlink or junction
/// needs `remove_dir`, which removes the link, never its target).
pub fn remove_link_or_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if crate::platform::host::WINDOWS => {
            std::fs::remove_dir(path).map_err(|_| error)
        }
        other => other,
    }
}

/// The call's trash entry, created on first use:
/// `<trash>/<utc>-<hex>-<first name>`.
fn entry_dir(
    entry: &mut Option<PathBuf>,
    trash_root: &Path,
    first: &Target,
    now: SystemTime,
) -> Result<PathBuf, String> {
    if let Some(dir) = entry {
        return Ok(dir.clone());
    }
    std::fs::create_dir_all(trash_root)
        .map_err(|e| format!("create {}: {e}", trash_root.display()))?;
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let name: String = first
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    for _ in 0..32 {
        let dir = trash_root.join(format!(
            "{}-{}-{name}",
            crate::trash::timestamp_utcish(since.as_secs() as i64),
            crate::trash::randomish_hex(since)
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                *entry = Some(dir.clone());
                return Ok(dir);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create {}: {error}", dir.display())),
        }
    }
    Err("could not allocate a unique trash entry".into())
}

/// A distinct directory name per root inside one entry (`repo`, `repo-2`).
fn root_dir_name(names: &mut BTreeMap<PathBuf, PathBuf>, root: &Path) -> PathBuf {
    if let Some(name) = names.get(root) {
        return name.clone();
    }
    let base = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    let mut candidate = PathBuf::from(&base);
    let mut n = 2;
    while names.values().any(|v| v == &candidate) {
        candidate = PathBuf::from(format!("{base}-{n}"));
        n += 1;
    }
    names.insert(root.to_path_buf(), candidate.clone());
    candidate
}

/// Rename `source` to `dest`; across filesystems, copy (never following
/// symlinks) and then remove the source.
fn move_into_trash(source: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    match std::fs::rename(source, dest) {
        Ok(()) => Ok(()),
        Err(error) if crate::trash::is_cross_volume_error(&error) => {
            copy_nofollow(source, dest).map_err(|e| format!("copy to the trash: {e}"))?;
            let meta = std::fs::symlink_metadata(source).map_err(|e| e.to_string())?;
            if meta.is_dir() {
                std::fs::remove_dir_all(source)
            } else {
                remove_link_or_file(source)
            }
            .map_err(|e| format!("remove after copying to the trash: {e}"))
        }
        Err(error) => Err(format!("move to the trash: {error}")),
    }
}

pub(crate) fn copy_nofollow(source: &Path, dest: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        let link = std::fs::read_link(source)?;
        return crate::platform::rm_tool::copy_symlink(source, link, dest);
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_nofollow(&entry.path(), &dest.join(entry.file_name()))?;
        }
        return Ok(());
    }
    std::fs::copy(source, dest).map(|_| ())
}

fn write_manifest(dir: &Path, ctx: &Context, moved: &[(String, PathBuf)]) -> bool {
    let items: Vec<_> = moved
        .iter()
        .map(|(origin, trashed)| serde_json::json!({"origin": origin, "trashed": trashed}))
        .collect();
    let manifest = serde_json::json!({
        "version": 1,
        "command": COMMAND,
        "ts_unix": unix_secs(ctx.now),
        "cwd": ctx.cwd,
        "session_id": ctx.session_id,
        "role": ctx.role,
        "items": items,
    });
    serde_json::to_vec_pretty(&manifest)
        .is_ok_and(|bytes| std::fs::write(dir.join(TRASH_MANIFEST), bytes).is_ok())
}

/// Best effort: never starts a daemon; the daemon's sweep also finds an
/// unregistered entry by its manifest.
fn register_entry(dir: &Path, origin: &str, now: SystemTime) {
    let Ok(state_dir) = crate::daemon::default_state_dir() else {
        return;
    };
    let input = crate::gc::InsertInput {
        kind: "trash".to_string(),
        path: dir.to_string_lossy().into_owned(),
        repo_root: None,
        branch: None,
        agent_id: Some(origin.to_string()),
        now_unix: unix_secs(now),
    };
    let _ = crate::daemon::gc_client_insert(&state_dir, &input);
}

/// Whether a trash entry written by `safe-rm` must still be kept
/// at `now`. Entries without the manifest (`clud trash` quarantine) are
/// never kept by this rule.
pub fn keep_trash_entry(dir: &Path, now: SystemTime) -> bool {
    let Ok(meta) = std::fs::metadata(dir.join(TRASH_MANIFEST)) else {
        return false;
    };
    meta.modified()
        .ok()
        .and_then(|mtime| now.duration_since(mtime).ok())
        .is_none_or(|age| age < TRASH_KEEP)
}

/// Trash entries written by `safe-rm` that are due for removal:
/// manifest-bearing directories under `trash_root` older than
/// [`TRASH_KEEP`]. The daemon reaps these even when registration failed.
pub fn expired_trash_entries(trash_root: &Path, now: SystemTime) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(trash_root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|dir| dir.join(TRASH_MANIFEST).is_file() && !keep_trash_entry(dir, now))
        .collect()
}

fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Append one JSONL record for this call to `<dir>/<YYYY-MM-DD>.jsonl`.
/// Best effort: auditing never changes the outcome.
fn audit(
    dir: &Path,
    options: &Options,
    ctx: &Context,
    roots: &Roots,
    outcomes: &[Outcome],
    code: i32,
) {
    let paths: Vec<_> = outcomes
        .iter()
        .map(|o| {
            serde_json::json!({
                "path": o.path,
                "action": o.action,
                "reason": o.reason,
                "trash_path": o.trash_path,
            })
        })
        .collect();
    let record = serde_json::json!({
        "ts_unix": unix_secs(ctx.now),
        "command": COMMAND,
        "session_id": ctx.session_id,
        "role": ctx.role,
        "cwd": ctx.cwd,
        "roots": roots.roots,
        "purge": options.purge,
        "tracked": options.tracked,
        "dry_run": options.dry_run,
        "paths": paths,
        "exit": code,
    });
    append_audit(dir, ctx.now, &record);
}

/// Append `record` to the day's audit file under `dir`.
pub fn append_audit(dir: &Path, now: SystemTime, record: &serde_json::Value) {
    let fields = crate::civil_time::civil_from_unix_secs(unix_secs(now));
    let file = dir.join(format!(
        "{:04}-{:02}-{:02}.jsonl",
        fields.0, fields.1, fields.2
    ));
    let _ = std::fs::create_dir_all(dir);
    if let Ok(mut handle) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        let _ = handle.write_all(format!("{record}\n").as_bytes());
    }
}

/// The [`ROOTS_ENV`] value for a session launched from `cwd`: its git
/// checkout (the cwd outside a repo) and clud's session temp directory,
/// which holds every agent scratchpad. `None` when neither resolves.
pub fn session_roots_value(cwd: &Path) -> Option<String> {
    let repo = crate::block_bad_cmd::nearest_repo_root_public(cwd);
    let base = repo.clone().unwrap_or_else(|| cwd.to_path_buf());
    let mut roots: Vec<PathBuf> = vec![base];
    if let Some(repo) = repo {
        roots.extend(sister_roots(&repo));
    }
    if let Some(tmp) = crate::gc::session_tmp::session_tmp_dir() {
        roots.push(tmp);
    }
    let home = crate::home::user_home().and_then(|h| crate::path_norm::canonicalize_plain(h).ok());
    let roots: Vec<PathBuf> = roots
        .into_iter()
        .filter(|p| p.is_absolute() && p.is_dir())
        .filter(|p| {
            let canonical = crate::path_norm::canonicalize_plain(p).unwrap_or_else(|_| p.clone());
            home.as_ref()
                .is_none_or(|home| !home.starts_with(&canonical))
        })
        .collect();
    if roots.is_empty() {
        return None;
    }
    std::env::join_paths(roots)
        .ok()
        .map(|v| v.to_string_lossy().into_owned())
}

/// Sister checkouts beside `repo` (#1696): every sibling directory that is a
/// git checkout (`.git` is a directory, so a main worktree; its linked
/// worktrees are covered through it), and `../<repo>-extern`, the sister
/// checkout directory of the `clud-extern-repos` skill. Sorted.
fn sister_roots(repo: &Path) -> Vec<PathBuf> {
    // A linked worktree's sisters are its main checkout's (`.git` is a file
    // naming `<main>/.git/worktrees/<id>`), so `myrepo-wt-1` finds
    // `myrepo-extern`, as the skill derives it.
    let main = std::fs::read_to_string(repo.join(".git"))
        .ok()
        .and_then(|text| {
            text.trim()
                .strip_prefix("gitdir:")
                .map(|g| PathBuf::from(g.trim()))
        })
        .and_then(|gitdir| gitdir.ancestors().nth(3).map(Path::to_path_buf))
        .filter(|main| main.is_absolute() && main.join(".git").is_dir());
    let main = main.as_deref().unwrap_or(repo);
    let (Some(parent), Some(name)) = (main.parent(), main.file_name()) else {
        return Vec::new();
    };
    let mut extern_name = name.to_os_string();
    extern_name.push("-extern");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut sisters: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.as_path() != repo && path.is_dir())
        .filter(|path| {
            path.file_name() == Some(extern_name.as_os_str()) || path.join(".git").is_dir()
        })
        .collect();
    sisters.sort();
    sisters
}

/// The system temp directories named by the platform and `env` (#1622), as
/// absolute, verbatim-free spellings, deduplicated, not yet canonicalized.
pub(crate) fn temp_root_candidates(
    windows: bool,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let mut raw: Vec<String> = Vec::new();
    if windows {
        raw.extend(
            ["TEMP", "TMP"]
                .iter()
                .filter_map(|k| env(k))
                .map(|v| v.to_string_lossy().into_owned()),
        );
    } else {
        raw.push("/tmp".into());
        raw.push("/var/tmp".into());
        // POSIX shared memory, tmpfs on Linux (#1659). Absent on macOS, where
        // canonicalization drops it. `/run/user/<uid>` is deliberately not a
        // root: it holds live session sockets (D-Bus, PipeWire, Wayland).
        raw.push("/dev/shm".into());
        raw.extend(env("TMPDIR").map(|v| v.to_string_lossy().into_owned()));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for value in raw {
        let plain = crate::path_norm::strip_verbatim(Path::new(value.trim()));
        let text = plain.to_string_lossy();
        let text = if windows {
            text.trim_end_matches(['\\', '/'])
        } else {
            text.trim_end_matches('/')
        };
        let bytes = text.as_bytes();
        let absolute = if windows {
            (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'\\' | b'/'))
                || text.starts_with(r"\\")
        } else {
            text.len() > 1 && text.starts_with('/')
        };
        let duplicate = out.iter().any(|seen| {
            let seen = seen.to_string_lossy();
            if windows {
                seen.eq_ignore_ascii_case(text)
            } else {
                seen == text
            }
        });
        if absolute && !duplicate {
            out.push(PathBuf::from(text));
        }
    }
    out
}

/// This host's system temp directories, canonical (#1622). One that is a
/// filesystem root, or is `home` or an ancestor of it, is dropped. On Unix so
/// is one inside `home` (`TMPDIR=~/Documents` must not widen the roots); on
/// Windows only the profile's `AppData\Local` may hold it.
fn system_temp_roots(home: Option<&Path>) -> Vec<PathBuf> {
    let env = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty());
    let mut out: Vec<PathBuf> = Vec::new();
    for candidate in temp_root_candidates(crate::platform::host::WINDOWS, &env) {
        let Ok(root) = crate::path_norm::canonicalize_plain(&candidate) else {
            continue;
        };
        if root.parent().is_none() || !root.is_dir() {
            continue;
        }
        if let Some(home) = home {
            if home.starts_with(&root) {
                continue;
            }
            let allowed_under_home = crate::platform::host::WINDOWS
                && root.starts_with(home.join("AppData").join("Local"));
            if root.starts_with(home) && !allowed_under_home {
                continue;
            }
        }
        if !out.contains(&root) {
            out.push(root);
        }
    }
    out
}

/// Whether `path` is on tmpfs or ramfs (#1659). Linux only; elsewhere no
/// temp root counts as RAM-backed, so its entries are trashed as before.
fn is_ram_backed(path: &Path) -> bool {
    crate::platform::rm_tool::is_ram_backed(path)
}

#[path = "rm_tool_clone.rs"]
mod clone;

/// Whether `rev` holds commits no remote-tracking ref reaches; the predicate
/// `safe-rm` applies to clones, shared with the worktree trash schedule.
pub(crate) fn has_unpushed(dir: &Path, rev: &str) -> Result<bool, String> {
    clone::has_unpushed(dir, rev)
}

#[path = "rm_tool_ledger.rs"]
pub mod ledger;

#[path = "rm_tool_extra_roots.rs"]
pub mod extra_roots;

#[path = "rm_tool_home_files.rs"]
pub mod home_files;

#[path = "rm_tool_own_trash.rs"]
pub mod own_trash;

#[path = "rm_tool_session_grants.rs"]
pub(crate) mod session_grants;

#[cfg(test)]
#[path = "rm_tool_tests.rs"]
pub(crate) mod tests;
