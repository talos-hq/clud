use std::path::Path;
use std::time::{Duration, SystemTime};

use clap::CommandFactory;

use super::registry::now_unix;
use super::{uv_cache, uv_cache_repair};
use crate::args::{Args, GcSubcommand};
use crate::gc::{EXTERN_REPO_KIND, SIBLING_CLONE_KIND, WORKTREE_KIND};
use crate::worktrees;

/// Literal value of `--kind` that routes to the filesystem-managed
/// uv-cache handlers instead of the redb-tracked daemon path.
const UV_CACHE_KIND: &str = "uv-cache";
const TRASH_KIND: &str = "trash";
const TMP_KIND: &str = "tmp";
const WORKTREE_REGISTRATIONS_KIND: &str = "worktree-registrations";
const TRACKED_PRUNE_DURATION: &str = "48h";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GcKindBackend {
    Tracked,
    UvCache,
    /// Dead `git worktree` administrative entries in every visited repo (#2397).
    WorktreeRegistrations,
    /// The `~/.clud/tmp` size cap (#2397, DD-194).
    SessionTmp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GcKindSpec {
    name: &'static str,
    summary: &'static str,
    backend: GcKindBackend,
    prune_duration: Option<&'static str>,
}

const MANAGED_KINDS: &[GcKindSpec] = &[
    GcKindSpec {
        name: WORKTREE_KIND,
        summary: "agent worktrees tracked in the daemon registry",
        backend: GcKindBackend::Tracked,
        prune_duration: Some(TRACKED_PRUNE_DURATION),
    },
    GcKindSpec {
        name: SIBLING_CLONE_KIND,
        summary: "repo sibling clones tracked in the daemon registry",
        backend: GcKindBackend::Tracked,
        prune_duration: Some(TRACKED_PRUNE_DURATION),
    },
    GcKindSpec {
        name: EXTERN_REPO_KIND,
        summary: "repo-local .extern-repos checkouts",
        backend: GcKindBackend::Tracked,
        prune_duration: None,
    },
    GcKindSpec {
        name: TRASH_KIND,
        summary: "quarantined paths under ~/.clud/trash/",
        backend: GcKindBackend::Tracked,
        prune_duration: None,
    },
    GcKindSpec {
        name: WORKTREE_REGISTRATIONS_KIND,
        summary: "dead git worktree registrations in repos clud has visited",
        backend: GcKindBackend::WorktreeRegistrations,
        prune_duration: None,
    },
    GcKindSpec {
        name: TMP_KIND,
        summary: "~/.clud/tmp session scratch, capped by tmp.max_bytes (idle sessions only)",
        backend: GcKindBackend::SessionTmp,
        prune_duration: None,
    },
    GcKindSpec {
        name: UV_CACHE_KIND,
        summary: "bundled Python tool uv environments under ~/.clud/cache/uv/",
        backend: GcKindBackend::UvCache,
        prune_duration: None,
    },
];

// ---------- CLI handlers ----------
//
// Issue #135: the CLI no longer opens the redb directly. Every subcommand
// is a thin IPC client against the always-on session daemon, which now
// owns the redb handle and serializes all reads/writes through a single
// registry worker thread (see `daemon/gc_service.rs`). `--no-daemon` on any
// `clud gc` op is an error — there is no
// read-only fallback.

/// Issue #506: pseudo-kind accepted by `prune`/`purge` (positionally or
/// via `--kind`) that routes to the every-managed-kind path, so
/// `clud gc purge all --yes` ≡ `clud gc all --purge --yes`.
const ALL_KIND: &str = "all";

/// Issue #506: `prune`/`purge` accept the kind both positionally and via
/// the legacy `--kind` flag (clap enforces mutual exclusion). Collapse the
/// parsed `GcSubcommand` into one canonical shape before validation so
/// every downstream check sees a single `kind` field.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GcAction {
    List {
        json: bool,
        kind: Option<String>,
        targets: bool,
    },
    Prune {
        dry_run: bool,
        older_than: Option<String>,
        kind: Option<String>,
    },
    Purge {
        dry_run: bool,
        yes: bool,
        older_than: Option<String>,
        kind: Option<String>,
    },
    All {
        purge: bool,
        dry_run: bool,
        yes: bool,
        older_than: Option<String>,
    },
    Reconcile,
}

fn normalize(sub: GcSubcommand) -> GcAction {
    match sub {
        GcSubcommand::List {
            json,
            kind,
            targets,
        } => GcAction::List {
            json,
            targets,
            // `--kind all` on list means "no filter".
            kind: kind.filter(|k| k != ALL_KIND),
        },
        GcSubcommand::Prune {
            dry_run,
            older_than,
            kind_pos,
            kind,
            ..
        } => GcAction::Prune {
            dry_run,
            older_than,
            kind: kind_pos.or(kind),
        },
        GcSubcommand::Purge {
            dry_run,
            yes,
            older_than,
            kind_pos,
            kind,
            ..
        } => GcAction::Purge {
            dry_run,
            yes,
            older_than,
            kind: kind_pos.or(kind),
        },
        GcSubcommand::All {
            purge,
            dry_run,
            yes,
            older_than,
            ..
        } => GcAction::All {
            purge,
            dry_run,
            yes,
            older_than,
        },
        GcSubcommand::Reconcile => GcAction::Reconcile,
        GcSubcommand::Keep { .. } => unreachable!("gc keep is dispatched before normalize"),
    }
}

/// Dispatch a `clud gc` invocation. Returns the process exit code.
pub fn run(args: &Args, sub: Option<GcSubcommand>) -> i32 {
    // Bare `clud gc` keeps printing help and does NOT contact the daemon.
    let Some(sub) = sub else {
        return print_help_and_exit_zero();
    };
    if let GcSubcommand::Keep { path, remove, list } = &sub {
        let path = match (path, list) {
            (_, true) => None,
            (Some(p), _) => Some(p.clone()),
            (None, false) => Some(std::path::PathBuf::from(".")),
        };
        return super::keep_list::run_cli(path.as_deref(), *remove, *list);
    }
    let action = normalize(sub);

    if let GcAction::List {
        json,
        targets: true,
        ..
    } = &action
    {
        return super::target_size_cache::run_report(*json);
    }

    if let Some(code) = validate_pre_daemon(&action) {
        return code;
    }

    // Issue #422: `--kind uv-cache` is filesystem-managed (not redb-tracked),
    // so it short-circuits the daemon roundtrip entirely. Handle it before
    // the daemon-required check so users without the daemon running can
    // still manage their uv cache.
    match &action {
        GcAction::List {
            json,
            kind: Some(k),
            ..
        } if k == UV_CACHE_KIND => return cmd_list_uv_cache(*json),
        GcAction::Purge {
            dry_run,
            yes,
            kind: Some(k),
            ..
        } if k == UV_CACHE_KIND => {
            return cmd_purge_uv_cache(*dry_run, *yes);
        }
        GcAction::Prune {
            dry_run,
            kind: Some(k),
            ..
        } if k == UV_CACHE_KIND => return cmd_prune_uv_cache(*dry_run),
        _ => {}
    }
    if args.no_daemon {
        eprintln!("error: gc operations require the clud daemon; remove --no-daemon");
        return 2;
    }
    let state_dir = match crate::daemon::default_state_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve clud state dir: {e}");
            return 1;
        }
    };
    match action {
        GcAction::List { json, kind, .. } => cmd_list(&state_dir, json, kind.as_deref()),
        GcAction::Prune {
            dry_run,
            older_than,
            kind,
        } => {
            let kind = kind.expect("validated kind");
            if kind == ALL_KIND {
                run_all_kinds(&state_dir, false, dry_run, false, older_than.as_deref())
            } else {
                let spec = find_kind(&kind).expect("validated kind");
                if let Some(code) = run_fs_kind(&state_dir, spec, dry_run) {
                    return code;
                }
                cmd_prune_tracked(&state_dir, spec, dry_run, older_than.as_deref())
            }
        }
        GcAction::Purge {
            dry_run,
            yes,
            older_than,
            kind,
        } => {
            let kind = kind.expect("validated kind");
            if kind == ALL_KIND {
                run_all_kinds(&state_dir, true, dry_run, yes, older_than.as_deref())
            } else {
                let spec = find_kind(&kind).expect("validated kind");
                if let Some(code) = run_fs_kind(&state_dir, spec, dry_run) {
                    return code;
                }
                cmd_purge_tracked(&state_dir, spec, dry_run, yes, older_than.as_deref())
            }
        }
        GcAction::All {
            purge,
            dry_run,
            yes,
            older_than,
        } => cmd_all(&state_dir, purge, dry_run, yes, older_than.as_deref()),
        GcAction::Reconcile => cmd_reconcile(&state_dir),
    }
}

/// Issue #506: every gc error hint must be a full copy-pasteable command,
/// including `--yes` for destructive ops. A purge invocation missing both
/// the kind and `--yes` learns both requirements from this one message.
fn missing_kind_error(action: &str) -> String {
    let yes = if action == "purge" { " --yes" } else { "" };
    format!(
        "error: {action} requires a KIND; e.g. `clud gc {action} worktree{yes}` or `clud gc {action} all{yes}` (kinds: {}, all)",
        managed_kind_names()
    )
}

fn unsupported_older_than_error(kind: &str) -> String {
    format!("error: --older-than is not supported for kind {kind}")
}

fn unknown_kind_error(kind: &str) -> String {
    format!(
        "error: unknown gc kind `{kind}`; managed kinds: {}, or `all` for every kind",
        managed_kind_names()
    )
}

fn missing_yes_error(kind: &str) -> String {
    format!("error: --yes required (destructive): `clud gc purge {kind} --yes`")
}

fn validate_pre_daemon(action: &GcAction) -> Option<i32> {
    match action {
        GcAction::Prune { kind: None, .. } => {
            eprintln!("{}", missing_kind_error("prune"));
            Some(2)
        }
        GcAction::Purge { kind: None, .. } => {
            eprintln!("{}", missing_kind_error("purge"));
            Some(2)
        }
        GcAction::Prune { kind: Some(k), .. } | GcAction::Purge { kind: Some(k), .. }
            if k != ALL_KIND && find_kind(k).is_none() =>
        {
            eprintln!("{}", unknown_kind_error(k));
            Some(2)
        }
        GcAction::List { kind: Some(k), .. } if find_kind(k).is_none() => {
            eprintln!("{}", unknown_kind_error(k));
            Some(2)
        }
        // `uv-cache` is filesystem-managed and its handlers take no
        // duration, so honouring `--older-than` here is not possible.
        // Reject it rather than accept-and-ignore — a silently dropped
        // flag is the exact defect this command already had.
        GcAction::Prune {
            older_than: Some(_),
            kind: Some(k),
            ..
        }
        | GcAction::Purge {
            older_than: Some(_),
            kind: Some(k),
            ..
        } if is_older_than_unsupported(k) => {
            eprintln!("{}", unsupported_older_than_error(k));
            Some(2)
        }
        GcAction::Purge {
            dry_run: false,
            yes: false,
            kind: Some(k),
            ..
        } => {
            eprintln!("{}", missing_yes_error(k));
            Some(2)
        }
        GcAction::All {
            purge: true,
            yes: false,
            ..
        } => {
            eprintln!("error: `clud gc all --purge` requires --yes: `clud gc all --purge --yes`");
            Some(2)
        }
        GcAction::All {
            purge: false,
            yes: true,
            ..
        } => {
            eprintln!("error: --yes only applies with `clud gc all --purge`.");
            Some(2)
        }
        _ => None,
    }
}

/// Issue #422: `clud gc list --kind uv-cache` — print env count, total
/// bytes, oldest mtime. Filesystem-only; no daemon needed.
fn cmd_list_uv_cache(json: bool) -> i32 {
    let summary = match uv_cache::list() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: uv-cache list failed: {e}");
            return 1;
        }
    };
    if json {
        let oldest_unix = summary
            .oldest_mtime
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        println!(
            "{{\"kind\":\"uv-cache\",\"root\":{:?},\"exists\":{},\"env_count\":{},\"total_bytes\":{},\"oldest_mtime_unix\":{}}}",
            summary.root.to_string_lossy(),
            summary.exists,
            summary.env_count,
            summary.total_bytes,
            oldest_unix
                .map(|s| s.to_string())
                .unwrap_or_else(|| "null".into()),
        );
        return 0;
    }
    println!("uv-cache root: {}", summary.root.display());
    if !summary.exists {
        println!("(cache directory does not exist; no envs materialized yet)");
        return 0;
    }
    println!("envs:        {}", summary.env_count);
    println!(
        "total bytes: {} ({})",
        summary.total_bytes,
        format_bytes(summary.total_bytes)
    );
    if let Some(mtime) = summary.oldest_mtime {
        let age = SystemTime::now().duration_since(mtime).unwrap_or_default();
        println!("oldest env:  {} old", worktrees::fmt_age(age));
    }
    0
}

/// `clud gc prune --kind uv-cache` runs the same stale-env sweep and
/// corrupt-wheel repair (#1711) as the daemon's daily tick. Full cache
/// deletion lives under `purge`.
fn cmd_prune_uv_cache(dry_run: bool) -> i32 {
    match uv_cache::sweep_stale(SystemTime::now(), dry_run) {
        Ok(report) => {
            print_uv_cache_sweep_report(&report);
            print_uv_cache_repair_report(&uv_cache_repair::repair_corrupt_wheels(dry_run));
            0
        }
        Err(e) => {
            eprintln!("error: uv-cache prune failed: {e}");
            1
        }
    }
}

fn cmd_purge_uv_cache(dry_run: bool, yes: bool) -> i32 {
    if !dry_run && !yes {
        eprintln!("{}", missing_yes_error(UV_CACHE_KIND));
        return 2;
    }
    if dry_run {
        println!("uv-cache: --dry-run would purge all of ~/.clud/cache/uv/.");
        return 0;
    }
    match uv_cache::purge_all() {
        Ok(()) => {
            println!("uv-cache: purged.");
            0
        }
        Err(e) => {
            eprintln!("error: uv-cache purge failed: {e}");
            1
        }
    }
}

fn print_uv_cache_sweep_report(report: &uv_cache::SweepReport) {
    let stale_word = if report.stale_envs_removed == 1 {
        ""
    } else {
        "s"
    };
    if report.dry_run {
        println!(
            "uv-cache: --dry-run would remove {} stale env{stale_word}, skip {} locked.",
            report.stale_envs_removed, report.locked_envs_skipped,
        );
    } else {
        println!(
            "uv-cache: pruned {} stale env{stale_word}, {} locked-skipped.",
            report.stale_envs_removed, report.locked_envs_skipped,
        );
    }
}

fn print_uv_cache_repair_report(report: &uv_cache_repair::RepairReport) {
    let (found, repaired, skipped) = (report.corrupt_found, report.repaired, report.skipped);
    if report.dry_run {
        println!("uv-cache: --dry-run would invalidate {repaired} corrupt wheel entries.");
    } else {
        println!(
            "uv-cache: {found} corrupt wheel entries, {repaired} invalidated, {skipped} skipped."
        );
    }
}

pub(super) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn print_help_and_exit_zero() -> i32 {
    let mut top = Args::command();
    match top.find_subcommand_mut("gc") {
        Some(gc) => {
            let _ = gc.print_help();
            println!();
            print!("{}", managed_kinds_help());
            0
        }
        None => {
            eprintln!("error: gc subcommand definition missing (internal bug)");
            2
        }
    }
}

fn find_kind(name: &str) -> Option<GcKindSpec> {
    MANAGED_KINDS.iter().copied().find(|kind| kind.name == name)
}

fn managed_kind_names() -> String {
    MANAGED_KINDS
        .iter()
        .map(|kind| kind.name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn managed_kinds_help() -> String {
    let mut out = String::from("\nKINDS:\n");
    for kind in MANAGED_KINDS {
        out.push_str(&format!("  {:<14} {}\n", kind.name, kind.summary));
    }
    out.push_str(&format!(
        "  {:<14} pseudo-kind: every managed kind above (e.g. `clud gc purge all --yes`)\n",
        ALL_KIND
    ));
    out
}

fn cmd_list(state_dir: &Path, json: bool, kind_filter: Option<&str>) -> i32 {
    let rows = match crate::daemon::gc_client_list(state_dir, kind_filter) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: list failed: {e}");
            return 1;
        }
    };
    if json {
        match serde_json::to_string(&rows) {
            Ok(s) => println!("{}", s),
            Err(e) => {
                eprintln!("error: serialize failed: {e}");
                return 1;
            }
        }
        warn_if_worktree_root_oversized();
        warn_if_session_tmp_oversized();
        warn_if_session_tmp_foreign_owned();
        warn_if_clud_cache_oversized();
        warn_if_dead_worktree_registrations(state_dir);
        return 0;
    }
    print_table_from_rows(&rows);
    warn_if_worktree_root_oversized();
    warn_if_session_tmp_oversized();
    warn_if_session_tmp_foreign_owned();
    warn_if_clud_cache_oversized();
    warn_if_dead_worktree_registrations(state_dir);
    0
}

/// #1327: warn (stderr) when `~/.clud/tmp` exceeds `tmp.warn_bytes`.
/// Warn-only: the 72 h age sweep stays the only deleter of session temp.
fn warn_if_session_tmp_oversized() {
    use crate::gc::worktree_root::{check_tree_size, SIZE_SCAN_ENTRY_BUDGET};
    use crate::gc::worktree_size_cache::{tmp_list_warning, DEFAULT_TMP_WARN_BYTES};
    let Some(root) = crate::gc::session_tmp::session_tmp_dir() else {
        return;
    };
    let warn_bytes = crate::clud_settings::load_tmp_warn_bytes().unwrap_or(DEFAULT_TMP_WARN_BYTES);
    if warn_bytes == 0 {
        return;
    }
    let check = check_tree_size(&root, warn_bytes, SIZE_SCAN_ENTRY_BUDGET);
    if let Some(line) = tmp_list_warning(&root, warn_bytes, check) {
        eprintln!("{line}");
    }
}

/// #2486: list the subtrees the session-tmp sweep skipped as owned by another
/// user, with the command that removes each. The daemon records them.
fn warn_if_session_tmp_foreign_owned() {
    let Some(state_dir) = crate::home::user_home().map(|home| home.join(".clud/state")) else {
        return;
    };
    let paths = crate::gc::session_tmp::read_foreign_owned(&state_dir);
    for line in crate::gc::session_tmp::foreign_owned_lines(&paths) {
        eprintln!("{line}");
    }
}

/// #1691: warn (stderr) when `~/.clud/cache` exceeds `cache.warn_bytes`.
/// Warn-only: clud never deletes inside uv's cache for size (DD-144).
fn warn_if_clud_cache_oversized() {
    use crate::gc::worktree_root::{check_tree_size, SIZE_SCAN_ENTRY_BUDGET};
    use crate::gc::worktree_size_cache::{
        clud_cache_dir_for, clud_cache_list_warning, DEFAULT_CACHE_WARN_BYTES,
    };
    let Some(home) = crate::home::user_home() else {
        return;
    };
    let warn_bytes =
        crate::clud_settings::load_cache_warn_bytes().unwrap_or(DEFAULT_CACHE_WARN_BYTES);
    if warn_bytes == 0 {
        return;
    }
    let root = clud_cache_dir_for(&home);
    let check = check_tree_size(&root, warn_bytes, SIZE_SCAN_ENTRY_BUDGET);
    if let Some(line) = clud_cache_list_warning(&root, warn_bytes, check) {
        eprintln!("{line}");
    }
}

/// #1485: warn (stderr, so `--json` stays parseable) when `~/.clud/worktrees`
/// exceeds `worktrees.warn_bytes`. Warn-only: nothing is deleted for size.
fn warn_if_worktree_root_oversized() {
    use crate::gc::worktree_root::{
        check_tree_size, size_warning, worktree_root, DEFAULT_WARN_BYTES, SIZE_SCAN_ENTRY_BUDGET,
    };
    let Some(root) = worktree_root() else {
        return;
    };
    let warn_bytes =
        crate::clud_settings::load_worktrees_warn_bytes().unwrap_or(DEFAULT_WARN_BYTES);
    if warn_bytes == 0 {
        return;
    }
    let check = check_tree_size(&root, warn_bytes, SIZE_SCAN_ENTRY_BUDGET);
    if let Some(line) = size_warning(&root, warn_bytes, check) {
        eprintln!("{line}");
    }
}

/// Kinds whose handlers take no duration (filesystem-managed, not registry rows).
fn is_older_than_unsupported(kind: &str) -> bool {
    kind == UV_CACHE_KIND || kind == WORKTREE_REGISTRATIONS_KIND || kind == TMP_KIND
}

/// Run a filesystem-managed kind that is neither tracked nor uv-cache.
/// `None` means `spec` is an ordinary tracked kind. These kinds only ever
/// remove metadata or already-idle data, so `purge` runs the same pass as
/// `prune` (it never widens what is deleted).
fn run_fs_kind(state_dir: &Path, spec: GcKindSpec, dry_run: bool) -> Option<i32> {
    match spec.backend {
        GcKindBackend::WorktreeRegistrations => {
            Some(cmd_prune_worktree_registrations(state_dir, dry_run))
        }
        GcKindBackend::SessionTmp => Some(cmd_prune_tmp(dry_run)),
        _ => None,
    }
}

/// #2397: evict the oldest idle `~/.clud/tmp` units while over `tmp.max_bytes`.
/// A live session's directory is never touched (see `gc::session_tmp_cap`).
/// The age sweep stays the daemon's job: it owns the resumable work queue.
fn cmd_prune_tmp(dry_run: bool) -> i32 {
    let report = super::session_tmp_cap::enforce_cap(dry_run);
    println!("{}", report.summary_line());
    if let Some(line) = report.undeletable.summary_line() {
        eprintln!("tmp: {line}");
    }
    i32::from(report.failed > 0)
}

/// Repo roots clud has run in (the registry's `repo_visits`).
fn visited_repos(state_dir: &Path) -> Result<Vec<String>, String> {
    crate::daemon::gc_client_list_repo_visits(state_dir)
        .map(|rows| rows.into_iter().map(|row| row.repo_root).collect())
        .map_err(|e| e.to_string())
}

/// #2397: `git worktree prune` every visited repo and report the count.
fn cmd_prune_worktree_registrations(state_dir: &Path, dry_run: bool) -> i32 {
    let repos = match visited_repos(state_dir) {
        Ok(repos) => repos,
        Err(e) => {
            eprintln!("error: worktree-registrations: cannot list visited repos: {e}");
            return 1;
        }
    };
    let report = super::worktree_registrations::prune_repos(&repos, dry_run);
    println!("{}", report.summary_line());
    let mut status = 0;
    for failed in report.repos.iter().filter(|r| r.error.is_some()) {
        eprintln!(
            "worktree-registrations: {}: {}",
            failed.repo,
            failed.error.as_deref().unwrap_or_default()
        );
        if failed.dead > 0 {
            status = 1;
        }
    }
    status
}

/// #2397: `clud gc list` hint (stderr, so `--json` stays parseable) when
/// visited repos hold dead worktree registrations.
fn warn_if_dead_worktree_registrations(state_dir: &Path) {
    let Ok(repos) = visited_repos(state_dir) else {
        return;
    };
    let dead = super::worktree_registrations::count_dead(&repos);
    if dead > 0 {
        eprintln!(
            "worktree-registrations: {dead} dead registration(s); run `clud gc prune worktree-registrations` to drop them"
        );
    }
}

fn cmd_reconcile(state_dir: &Path) -> i32 {
    let main_root = match worktrees::locate_main_repo_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: reconcile requires a git repo: {e}");
            return 1;
        }
    };
    match crate::daemon::gc_client_reconcile(state_dir, &main_root) {
        Ok(n) => {
            println!(
                "reconcile: {n} new entr{}",
                if n == 1 { "y" } else { "ies" }
            );
            0
        }
        Err(e) => {
            eprintln!("error: reconcile failed: {e}");
            1
        }
    }
}

fn cmd_prune_tracked(
    state_dir: &Path,
    spec: GcKindSpec,
    dry_run: bool,
    older_than: Option<&str>,
) -> i32 {
    debug_assert_eq!(spec.backend, GcKindBackend::Tracked);
    maybe_reconcile_current_repo(state_dir);
    // `--older-than` overrides the per-kind default prune window.
    let duration = older_than.or(spec.prune_duration);
    run_tracked_gc(state_dir, spec, "prune", duration, dry_run)
}

fn cmd_purge_tracked(
    state_dir: &Path,
    spec: GcKindSpec,
    dry_run: bool,
    yes: bool,
    older_than: Option<&str>,
) -> i32 {
    debug_assert_eq!(spec.backend, GcKindBackend::Tracked);
    if !dry_run && !yes {
        eprintln!("{}", missing_yes_error(spec.name));
        return 2;
    }
    maybe_reconcile_current_repo(state_dir);
    // Purge removes everything by default; `--older-than` restricts to a window.
    run_tracked_gc(state_dir, spec, "purge", older_than, dry_run)
}

fn cmd_all(
    state_dir: &Path,
    purge: bool,
    dry_run: bool,
    yes: bool,
    older_than: Option<&str>,
) -> i32 {
    if purge && !yes {
        eprintln!("error: `clud gc all --purge` requires --yes: `clud gc all --purge --yes`");
        return 2;
    }
    if !purge && yes {
        eprintln!("error: --yes only applies with `clud gc all --purge`.");
        return 2;
    }
    run_all_kinds(state_dir, purge, dry_run, yes, older_than)
}

/// Shared every-managed-kind sweep, reached via `clud gc all` or the
/// `all` pseudo-kind on `prune`/`purge` (issue #506). Callers validate
/// `--yes` under their own subcommand's rules before getting here.
fn run_all_kinds(
    state_dir: &Path,
    purge: bool,
    dry_run: bool,
    yes: bool,
    older_than: Option<&str>,
) -> i32 {
    maybe_reconcile_current_repo(state_dir);
    let mut status = 0;
    for spec in MANAGED_KINDS {
        let code = match (spec.backend, purge) {
            (GcKindBackend::UvCache, false) => cmd_prune_uv_cache(dry_run),
            (GcKindBackend::UvCache, true) => cmd_purge_uv_cache(dry_run, yes),
            (GcKindBackend::SessionTmp, _) => cmd_prune_tmp(dry_run),
            (GcKindBackend::WorktreeRegistrations, _) => {
                cmd_prune_worktree_registrations(state_dir, dry_run)
            }
            (GcKindBackend::Tracked, false) => {
                let duration = older_than.or(spec.prune_duration);
                run_tracked_gc(state_dir, *spec, "prune", duration, dry_run)
            }
            (GcKindBackend::Tracked, true) => {
                run_tracked_gc(state_dir, *spec, "purge", older_than, dry_run)
            }
        };
        if code != 0 {
            status = 1;
        }
    }
    status
}

fn run_tracked_gc(
    state_dir: &Path,
    spec: GcKindSpec,
    action: &str,
    duration: Option<&str>,
    dry_run: bool,
) -> i32 {
    if let Some(d) = duration {
        if let Err(e) = worktrees::parse_duration(d) {
            eprintln!("error: invalid {action} duration for {}: {e}", spec.name);
            return 2;
        }
    }

    match crate::daemon::gc_client_purge(state_dir, duration, Some(spec.name), dry_run) {
        Ok(outcome) => {
            print_tracked_gc_outcome(spec.name, action, dry_run, outcome);
            0
        }
        Err(e) => {
            eprintln!("error: {action} failed for {}: {e}", spec.name);
            1
        }
    }
}

fn print_tracked_gc_outcome(
    kind: &str,
    action: &str,
    dry_run: bool,
    outcome: crate::daemon::GcPurgeOutcome,
) {
    match outcome {
        crate::daemon::GcPurgeOutcome::Completed { removed, skipped } => {
            if dry_run {
                println!("{kind}: --dry-run would {action} {removed}, skip {skipped}.");
            } else {
                println!("{kind}: {action} removed {removed}, skipped {skipped}.");
            }
        }
        crate::daemon::GcPurgeOutcome::Started {
            dispatched,
            skipped,
        } => {
            println!(
                "{kind}: {action} dispatched {dispatched} delete{} in background, skipped {skipped}.",
                if dispatched == 1 { "" } else { "s" }
            );
            println!(
                "{kind}: deletes happen asynchronously; re-run `clud gc list --kind {kind}` to watch the registry shrink."
            );
        }
    }
}

fn maybe_reconcile_current_repo(state_dir: &Path) {
    if let Ok(main_root) = worktrees::locate_main_repo_root() {
        let _ = crate::daemon::gc_client_reconcile(state_dir, &main_root);
    }
}

fn print_table_from_rows(rows: &[crate::daemon::ListRow]) {
    if rows.is_empty() {
        println!("(no tracked entries)");
        return;
    }
    let now = now_unix();
    let color_mode = ListColor::for_stdout();
    let kind_w = rows.iter().map(|r| r.kind.len()).max().unwrap_or(0).max(4);
    let agent_w = rows
        .iter()
        .map(|r| r.agent_id.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(0)
        .max(5);
    let state_w = rows.iter().map(|r| r.state.len()).max().unwrap_or(0).max(5);
    println!(
        "{:<kind_w$}  {:>6}  {:<agent_w$}  {:<20}  {:<state_w$}  PATH",
        "KIND",
        "AGE",
        "AGENT",
        "BRANCH",
        "STATE",
        kind_w = kind_w,
        agent_w = agent_w,
        state_w = state_w,
    );
    for r in rows {
        let age = Duration::from_secs((now - r.created_unix).max(0) as u64);
        // Issue #896: yellow marks rows deliberately retained, red marks
        // rows that are a problem. Machine consumers use `--json`, which
        // carries `state`/`reason` without escapes.
        let (color, reset) = match color_mode.color_for(&r.state) {
            Some(c) => (c, ANSI_RESET),
            None => ("", ""),
        };
        // The reason trails the path so the columns stay aligned when it
        // is absent, which is the common case.
        let reason = r
            .reason
            .as_deref()
            .map(|s| format!("  — {s}"))
            .unwrap_or_default();
        println!(
            "{color}{:<kind_w$}  {:>6}  {:<agent_w$}  {:<20}  {:<state_w$}  {}{reason}{reset}",
            r.kind,
            worktrees::fmt_age(age),
            r.agent_id.as_deref().unwrap_or("-"),
            r.branch.as_deref().unwrap_or("-"),
            r.state,
            r.path,
            kind_w = kind_w,
            agent_w = agent_w,
            state_w = state_w,
        );
    }
}

const ANSI_RESET: &str = "\x1b[0m";

/// Whether `gc list` colours its table. Mirrors
/// `large_file_guard::ColorMode`: colour only for an interactive terminal
/// with `NO_COLOR` unset, so `clud gc list > rows.txt` and
/// `clud gc list | grep ...` do not get escape codes baked into them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListColor {
    Plain,
    Ansi,
}

impl ListColor {
    /// Split from the environment probe so the decision is testable
    /// without a terminal.
    fn decide(stdout_is_terminal: bool, no_color_set: bool) -> Self {
        if stdout_is_terminal && !no_color_set {
            Self::Ansi
        } else {
            Self::Plain
        }
    }

    fn for_stdout() -> Self {
        use std::io::IsTerminal;
        Self::decide(
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        )
    }

    /// Colour for a row's `state`, or `None` when this mode is plain. An
    /// unrecognized value — a newer daemon sending a state this client does
    /// not know — renders plain rather than being guessed at.
    fn color_for(self, state: &str) -> Option<&'static str> {
        if self == Self::Plain {
            return None;
        }
        match state {
            "pinned" => Some("\x1b[33m"),
            "dangling" => Some("\x1b[31m"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_kind_help_lists_every_registered_kind() {
        let help = managed_kinds_help();
        for kind in MANAGED_KINDS {
            assert!(
                help.contains(kind.name),
                "help should include managed kind {}",
                kind.name
            );
        }
    }

    #[test]
    fn managed_kind_names_are_unique() {
        let mut names = MANAGED_KINDS
            .iter()
            .map(|kind| kind.name)
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), MANAGED_KINDS.len());
    }

    fn prune(kind: Option<&str>) -> GcAction {
        GcAction::Prune {
            dry_run: false,
            older_than: None,
            kind: kind.map(String::from),
        }
    }

    fn purge(kind: Option<&str>, dry_run: bool, yes: bool) -> GcAction {
        GcAction::Purge {
            dry_run,
            yes,
            older_than: None,
            kind: kind.map(String::from),
        }
    }

    #[test]
    fn prune_and_purge_without_kind_fail_before_daemon() {
        assert_eq!(validate_pre_daemon(&prune(None)), Some(2));
        assert_eq!(validate_pre_daemon(&purge(None, false, false)), Some(2));
    }

    #[test]
    fn unknown_kind_fails_before_daemon() {
        assert_eq!(validate_pre_daemon(&prune(Some("missing-kind"))), Some(2));
    }

    /// Issue #506: `gc purge 7d` (the pre-#110 duration positional) now
    /// parses as a positional kind and must be rejected here at runtime.
    #[test]
    fn legacy_duration_positional_rejected_as_unknown_kind() {
        assert_eq!(
            validate_pre_daemon(&purge(Some("7d"), false, true)),
            Some(2)
        );
    }

    #[test]
    fn purge_kind_requires_yes_before_daemon() {
        assert_eq!(
            validate_pre_daemon(&purge(Some("trash"), false, false)),
            Some(2)
        );
        assert_eq!(
            validate_pre_daemon(&purge(Some("trash"), true, false)),
            None
        );
    }

    #[test]
    fn all_purge_requires_yes_before_daemon() {
        assert_eq!(
            validate_pre_daemon(&GcAction::All {
                purge: true,
                dry_run: false,
                yes: false,
                older_than: None,
            }),
            Some(2)
        );
    }

    // ---------- issue #506: `all` pseudo-kind + positional KIND ----------

    #[test]
    fn all_pseudo_kind_is_valid_for_prune_and_purge() {
        assert_eq!(validate_pre_daemon(&prune(Some(ALL_KIND))), None);
        assert_eq!(
            validate_pre_daemon(&purge(Some(ALL_KIND), false, true)),
            None
        );
        // Like single-kind purge, --dry-run previews without --yes.
        assert_eq!(
            validate_pre_daemon(&purge(Some(ALL_KIND), true, false)),
            None
        );
    }

    #[test]
    fn purge_all_without_yes_fails_before_daemon() {
        assert_eq!(
            validate_pre_daemon(&purge(Some(ALL_KIND), false, false)),
            Some(2)
        );
    }

    #[test]
    fn normalize_prefers_positional_kind_and_falls_back_to_flag() {
        let positional = normalize(GcSubcommand::Purge {
            dry_run: false,
            yes: true,
            older_than: None,
            kind_pos: Some("trash".into()),
            kind: None,
        });
        assert_eq!(positional, purge(Some("trash"), false, true));

        let flag = normalize(GcSubcommand::Prune {
            dry_run: true,
            older_than: None,
            kind_pos: None,
            kind: Some("worktree".into()),
        });
        assert_eq!(
            flag,
            GcAction::Prune {
                dry_run: true,
                older_than: None,
                kind: Some("worktree".into()),
            }
        );
    }

    /// Regression guard: `--older-than` used to be parsed by clap and then
    /// dropped on the floor by `normalize`'s `..` rest-pattern, so
    /// `clud gc prune --older-than 2d` was a silent no-op. Assert the value
    /// survives normalization on every subcommand that accepts it — `All`
    /// especially, which was the arm that never forwarded it.
    #[test]
    fn normalize_threads_older_than_through_every_subcommand() {
        assert_eq!(
            normalize(GcSubcommand::Prune {
                dry_run: false,
                older_than: Some("2d".into()),
                kind_pos: Some("worktree".into()),
                kind: None,
            }),
            GcAction::Prune {
                dry_run: false,
                older_than: Some("2d".into()),
                kind: Some("worktree".into()),
            }
        );

        assert_eq!(
            normalize(GcSubcommand::Purge {
                dry_run: false,
                yes: true,
                older_than: Some("48h".into()),
                kind_pos: Some("trash".into()),
                kind: None,
            }),
            GcAction::Purge {
                dry_run: false,
                yes: true,
                older_than: Some("48h".into()),
                kind: Some("trash".into()),
            }
        );

        assert_eq!(
            normalize(GcSubcommand::All {
                purge: true,
                dry_run: false,
                yes: true,
                older_than: Some("7d".into()),
            }),
            GcAction::All {
                purge: true,
                dry_run: false,
                yes: true,
                older_than: Some("7d".into()),
            }
        );
    }

    /// `uv-cache` has no duration-aware handler, so `--older-than` must be
    /// refused rather than accepted and dropped — accept-and-ignore is the
    /// defect this command already shipped for the tracked kinds.
    #[test]
    fn older_than_is_rejected_for_uv_cache_and_allowed_elsewhere() {
        assert_eq!(
            validate_pre_daemon(&GcAction::Prune {
                dry_run: false,
                older_than: Some("2d".into()),
                kind: Some(UV_CACHE_KIND.into()),
            }),
            Some(2)
        );
        assert_eq!(
            validate_pre_daemon(&GcAction::Purge {
                dry_run: false,
                yes: true,
                older_than: Some("2d".into()),
                kind: Some(UV_CACHE_KIND.into()),
            }),
            Some(2)
        );
        // Without the flag, uv-cache is untouched by this rule.
        assert_eq!(
            validate_pre_daemon(&prune(Some(UV_CACHE_KIND))),
            None,
            "uv-cache without --older-than must still be accepted"
        );
        // And a tracked kind still accepts the flag.
        assert_eq!(
            validate_pre_daemon(&GcAction::Prune {
                dry_run: false,
                older_than: Some("2d".into()),
                kind: Some("worktree".into()),
            }),
            None
        );
    }

    /// Issue #896: yellow for deliberately-retained rows, red for problem
    /// rows, nothing for ordinary ones — and an unrecognized state from a
    /// newer daemon renders plain rather than being guessed at.
    #[test]
    fn only_pinned_and_dangling_states_are_colored() {
        let ansi = ListColor::Ansi;
        assert_eq!(ansi.color_for("pinned"), Some("\x1b[33m"));
        assert_eq!(ansi.color_for("dangling"), Some("\x1b[31m"));
        assert_eq!(ansi.color_for("reclaimable"), None);
        assert_eq!(ansi.color_for("something-newer"), None);
    }

    /// Redirected output must stay parseable: no escapes when stdout is not
    /// a terminal, and none when the user sets `NO_COLOR`.
    #[test]
    fn color_is_suppressed_when_redirected_or_no_color_is_set() {
        assert_eq!(ListColor::decide(true, false), ListColor::Ansi);
        assert_eq!(ListColor::decide(false, false), ListColor::Plain);
        assert_eq!(ListColor::decide(true, true), ListColor::Plain);
        assert_eq!(ListColor::decide(false, true), ListColor::Plain);
        assert_eq!(ListColor::Plain.color_for("pinned"), None);
        assert_eq!(ListColor::Plain.color_for("dangling"), None);
    }

    #[test]
    fn normalize_list_kind_all_means_no_filter() {
        let action = normalize(GcSubcommand::List {
            targets: false,
            json: false,
            kind: Some(ALL_KIND.into()),
        });
        assert_eq!(
            action,
            GcAction::List {
                targets: false,
                json: false,
                kind: None,
            }
        );
    }

    #[test]
    fn error_hints_are_copy_pasteable_commands() {
        // Missing kind on purge surfaces both the kind and --yes at once.
        let msg = missing_kind_error("purge");
        assert!(msg.contains("`clud gc purge all --yes`"), "{msg}");
        assert!(msg.contains("`clud gc purge worktree --yes`"), "{msg}");
        // Prune hint omits --yes (non-destructive).
        let msg = missing_kind_error("prune");
        assert!(msg.contains("`clud gc prune all`"), "{msg}");
        assert!(!msg.contains("--yes"), "{msg}");
        // Missing --yes prints the exact command to re-run.
        let msg = missing_yes_error("trash");
        assert!(msg.contains("`clud gc purge trash --yes`"), "{msg}");
        // Unknown kind mentions the `all` pseudo-kind.
        let msg = unknown_kind_error("bogus");
        assert!(msg.contains("`all`"), "{msg}");
    }

    #[test]
    fn kinds_help_footer_mentions_all_pseudo_kind() {
        let help = managed_kinds_help();
        assert!(help.contains("all"), "{help}");
        assert!(help.contains("clud gc purge all --yes"), "{help}");
    }
}
