use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use running_process::{
    NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode, StreamKind,
};

use crate::gc::TrackedEntry;
use crate::subprocess;
use crate::win_creation_flags::invisible_helper_creationflags;

use super::{DEFAULT_EXTERN_REPO_STALE_AFTER_SECS, ENV_GC_EXTERN_REPO_MAX_AGE_SECS};

/// Hard ceiling on a single git plumbing query in the purge probe.
///
/// This probe runs on the **registry worker thread** (`process_op` →
/// `dispatch_purge_entries` → `partition_purgeable`), the one thread that
/// owns redb. `worktrees::run_git` is deliberately patient — its read loop
/// leaves only on EOF or on a poll timeout *after* the child already
/// exited — so a git that stays alive with stdout open never returns. On
/// the worker that would wedge every client op at `WORKER_REPLY_TIMEOUT`,
/// including launch-path `record_repo_visit`. A checkout on a stale
/// network mount is enough to trigger it, so the probe gets its own
/// bounded runner that kills the child instead (same shape as the
/// `ls_files` startup guard, #556).
const GIT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn extern_repo_stale_after() -> Duration {
    let secs = std::env::var(ENV_GC_EXTERN_REPO_MAX_AGE_SECS)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_EXTERN_REPO_STALE_AFTER_SECS);
    Duration::from_secs(secs)
}

/// Whether a checkout holds work that an auto-purge would destroy.
///
/// This is the safety signal that replaced sole reliance on filesystem
/// mtime: mtime only answers "has anything been touched recently", which a
/// stray build artifact can reset forever, and says nothing about whether
/// there is *uncommitted or unpushed work* a delete would lose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitWorkState {
    /// Working tree is clean AND every local commit exists on a remote —
    /// the checkout is trivially re-clonable, so deleting it loses nothing.
    Clean,
    /// Uncommitted changes, untracked files, unpushed commits, or a stash
    /// are present. Deleting would destroy local work, so pin the entry.
    HasLocalWork,
    /// The tree is clean and every local commit is already *contained in the
    /// remote default branch as a patch*, even though it is reachable from no
    /// remote ref (#993).
    ///
    /// This is what a squash merge leaves behind, and it is the common case in
    /// repos that squash-merge and delete the branch afterwards: the original
    /// commits are ancestors of nothing on the remote, so `log --all --not
    /// --remotes` reports them and the checkout reads as `HasLocalWork` —
    /// pinned forever, when in truth its work is not merely pushed but already
    /// shipped. Exactly backwards: these are the checkouts that should be
    /// reclaimed *soonest*.
    Merged,
    /// Not a git work tree of its own. Its contents have not been verified,
    /// so pin it: a parent directory may contain nested active checkouts.
    Unknown,
    /// A probe query exceeded `GIT_PROBE_TIMEOUT` or git could not be run.
    /// Distinct from `Unknown`: there *is* a repository here, we simply
    /// failed to read it, so falling back to mtime-only would delete a
    /// checkout whose contents we never managed to inspect. Spare instead.
    ProbeFailed,
}

/// Environment variables that redirect git's repository discovery. If the
/// daemon inherited any of them — it is long-lived and may have been started
/// from a git hook or `git rebase -x` — then every probe would answer about
/// *that* repository no matter which `cwd` we pass, which is the parent-repo
/// bug all over again but invisible to the `.git` gate. `ProcessConfig.env`
/// can only set overrides, never unset, so we detect and spare instead.
const GIT_DISCOVERY_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
];

pub(super) fn git_discovery_env_is_poisoned() -> bool {
    GIT_DISCOVERY_ENV
        .iter()
        .any(|key| std::env::var_os(key).is_some())
}

/// Run one git plumbing query under a hard deadline, killing the child if
/// it overruns. Returns `None` on timeout, spawn failure, or non-zero exit
/// — the caller maps all three to `ProbeFailed`.
///
/// Deliberately not `worktrees::run_git`: that helper is unbounded by
/// design and other callers rely on its patience. See `GIT_PROBE_TIMEOUT`.
fn probe_git(cwd: &Path, args: &[&str]) -> Option<String> {
    match probe_cmd("git", cwd, args, GIT_PROBE_TIMEOUT)? {
        (0, text) => Some(text),
        _ => None,
    }
}

/// Bounded runner behind [`probe_git`], shared with the repo-worktree probe
/// (#1591), which also needs the exit code (`merge-base --is-ancestor`
/// answers through it) and a longer deadline for `gh`. Returns
/// `(exit_code, stdout)`, or `None` on spawn failure or timeout.
pub(super) fn probe_cmd(
    program: &str,
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Option<(i32, String)> {
    probe_cmd_streams(program, cwd, args, timeout).map(|(code, stdout, _)| (code, stdout))
}

thread_local! {
    /// #2103: probe commands this thread killed at their deadline, so the
    /// resumable scanner can tell a timed-out repo from a broken one.
    static PROBE_TIMEOUTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Running count of [`probe_cmd_streams`] timeouts on the calling thread.
pub(super) fn probe_timeouts_on_this_thread() -> u64 {
    PROBE_TIMEOUTS.with(std::cell::Cell::get)
}

/// [`probe_cmd`] that also returns stderr, kept apart from stdout, for a
/// caller that reports why a command failed (#1628/#1632: a reclaim's
/// `git worktree remove` exit code alone named no cause).
pub(super) fn probe_cmd_streams(
    program: &str,
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Option<(i32, String, String)> {
    let mut argv = vec![program.to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let process = NativeProcess::new(ProcessConfig {
        command: subprocess::command_spec_for_subprocess(argv),
        cwd: Some(cwd.to_path_buf()),
        env: None,
        capture: true,
        // Keep stderr on its own pipe. The caller's signal is whether
        // stdout is *empty*, so a warning (a missing `core.fsmonitor`
        // hook, a `safe.directory` gripe) must never be mistaken for
        // porcelain output — that would report a spotless checkout as
        // holding local work and pin it forever.
        stderr_mode: StderrMode::Pipe,
        // git is a piped helper; suppress the conhost popup on Windows.
        creationflags: invisible_helper_creationflags(),
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().ok()?;

    // `read_combined` yields both streams regardless of `stderr_mode`, so
    // the stream tag is what actually keeps stderr out of the signal.
    let mut err = String::new();
    let mut push = |text: &mut String, event: running_process::StreamEvent| {
        let sink = if event.stream == StreamKind::Stdout {
            text
        } else {
            &mut err
        };
        sink.push_str(&String::from_utf8_lossy(&event.line));
        sink.push('\n');
    };

    let deadline = Instant::now() + timeout;
    let mut text = String::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(25))) {
            ReadStatus::Line(event) => push(&mut text, event),
            ReadStatus::Eof => break,
            ReadStatus::Timeout => {}
        }
        match process.poll() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            // A wedged git must never hold the registry worker.
            let _ = process.kill();
            PROBE_TIMEOUTS.with(|n| n.set(n.get() + 1));
            return None;
        }
    }

    // Exit is not end of output. The reader threads may still be
    // delivering the child's last bytes, and an unterminated last line is
    // only emitted at pipe EOF. `wait` runs running-process's bounded
    // capture drain (it waits for EOF, and stops waiting if a grandchild
    // holds the pipe), so everything the child wrote is queued once it
    // returns. A fixed few-millisecond drain here used to drop output on a
    // loaded machine, turning `git rev-list --count` into an empty string
    // and a landed worktree into `unverifiable`.
    let code = process.wait(Some(Duration::from_secs(1))).ok()?;
    for event in process.drain_combined() {
        push(&mut text, event);
    }
    Some((code, text, err))
}

/// The remote default branch to compare against, e.g. `origin/main`.
///
/// Prefers the `origin/HEAD` the clone recorded; falls back to the two
/// conventional names. `None` when none resolves, which makes the caller
/// answer "not merged" and keep the checkout pinned.
fn remote_default_ref(cwd: &Path) -> Option<String> {
    if let Some(out) = probe_git(
        cwd,
        &[
            "--no-optional-locks",
            "--git-dir=.git",
            "--work-tree=.",
            "symbolic-ref",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        let name = out.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    for candidate in ["origin/main", "origin/master"] {
        if probe_git(
            cwd,
            &[
                "--no-optional-locks",
                "--git-dir=.git",
                "--work-tree=.",
                "rev-parse",
                "--verify",
                "--quiet",
                candidate,
            ],
        )
        .is_some_and(|out| !out.trim().is_empty())
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Whether every commit this checkout holds that no remote ref reaches is
/// already present in the remote default branch *as a patch* (#993).
///
/// `git cherry` is what answers this: it compares patch ids rather than
/// ancestry, so the new commit a squash merge created on `main` counts as
/// equivalent to the originals. Lines are `- <sha>` for "an equivalent is
/// upstream" and `+ <sha>` for "not upstream"; no `+` lines means fully
/// merged.
///
/// Conservative in every direction, because the cost of a wrong yes is
/// someone's unrecoverable work:
///
/// * A probe that fails, times out, or finds no remote default ref is a no.
/// * `git cherry` only walks `HEAD`. If any *other* ref holds commits the
///   remotes do not, this returns no rather than judging work it did not
///   look at -- so a merged HEAD beside a real feature branch stays pinned.
/// * An empty `git cherry` output with unpushed commits present would mean
///   the two probes disagree; treated as a no.
fn commits_are_squash_merged(cwd: &Path) -> bool {
    let Some(upstream) = remote_default_ref(cwd) else {
        return false;
    };

    // Every unpushed commit, and the subset reachable from HEAD. `git cherry`
    // speaks only for HEAD, so anything outside it must veto.
    let Some(all_unpushed) = unpublished_ref_log(cwd, false) else {
        return false;
    };
    let Some(head_unpushed) = probe_git(
        cwd,
        &[
            "--no-optional-locks",
            "--git-dir=.git",
            "--work-tree=.",
            "log",
            "HEAD",
            "--not",
            "--remotes",
            "--format=%H",
        ],
    ) else {
        return false;
    };
    let all: std::collections::BTreeSet<&str> = all_unpushed.split_whitespace().collect();
    let head: std::collections::BTreeSet<&str> = head_unpushed.split_whitespace().collect();
    if all.is_empty() || all != head {
        return false;
    }

    let Some(cherry) = probe_git(
        cwd,
        &[
            "--no-optional-locks",
            "--git-dir=.git",
            "--work-tree=.",
            "cherry",
            &upstream,
            "HEAD",
        ],
    ) else {
        return false;
    };
    let mut saw_equivalent = false;
    for line in cherry.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('+') {
            return false;
        }
        if line.starts_with('-') {
            saw_equivalent = true;
        }
    }
    saw_equivalent
}

/// Probe a checkout's git state. Runs at most three cheap plumbing queries,
/// and only ever on a directory the caller has already found idle, so the
/// common case (active repos, non-idle repos) never pays for it.
pub(super) fn git_work_state(cwd: &Path) -> GitWorkState {
    // Anchor the probe to *this* directory. `.extern-repos/` lives inside
    // the parent repo by convention, and the scanner tracks every immediate
    // child of it whether or not that child is a clone. Without this gate,
    // git walks up and answers about the **parent** repo: a plain directory
    // beside a parent with one untracked file reads as `HasLocalWork` and
    // is pinned forever, while a clean parent vouches for a checkout that
    // was never inspected. `.git` is a directory in a clone and a file in a
    // linked worktree, so `exists()` covers both. The gate alone is not
    // enough — a *malformed* `.git` (an interrupted clone leaves an empty
    // one) passes `exists()` and git then resumes walking up, verified:
    // from such a directory inside a dirty parent, `status --porcelain`
    // exits 0 and prints the parent's entries. Every query below is
    // therefore additionally pinned with `--git-dir`/`--work-tree`, which
    // disables discovery outright, so a malformed `.git` errors into
    // `ProbeFailed` (spare) instead of borrowing the parent's answer.
    if !cwd.join(".git").exists() {
        // A bare clone/mirror has no `.git`; its repository *is* the
        // directory. We do not probe it (a bare repo has no work tree, so
        // `status` is meaningless), but it plainly holds refs and objects,
        // so it must not fall through to the mtime-only verdict and be
        // deleted uninspected.
        if cwd.join("HEAD").is_file() && cwd.join("objects").is_dir() {
            return GitWorkState::ProbeFailed;
        }
        return GitWorkState::Unknown;
    }

    // This checkout may own the common Git directory of other worktrees,
    // even when their processes have cwds outside this source tree.
    if cwd.join(".git").join("worktrees").exists() {
        return GitWorkState::HasLocalWork;
    }

    // An inherited GIT_DIR (etc.) outranks `cwd` in git's discovery, so the
    // probe would silently answer about someone else's repository.
    if git_discovery_env_is_poisoned() {
        return GitWorkState::ProbeFailed;
    }

    // `--no-optional-locks` keeps the probe read-only: a plain `status`
    // refreshes the stat cache, rewriting `.git/index`. That both bumps the
    // mtime this module uses as its idle signal (re-arming the stale window
    // for another full cycle) and contends with a user's concurrent git
    // command for `index.lock`.
    let Some(status) = probe_git(
        cwd,
        &[
            "--no-optional-locks",
            "--git-dir=.git",
            "--work-tree=.",
            "status",
            "--porcelain",
            "--untracked-files=normal",
        ],
    ) else {
        return GitWorkState::ProbeFailed;
    };
    if !status.trim().is_empty() {
        return GitWorkState::HasLocalWork;
    }

    // Any commit not reachable from *some* remote ref. `--all` rather than
    // `--branches` so commits made on a detached HEAD — a checkout pinned
    // at a tag or sha, then committed on — are covered too; `--branches`
    // only expands `refs/heads` and would report such a repo as clean and
    // let it be deleted. With no remotes configured every commit qualifies
    // → HasLocalWork, which is the conservative (spare) answer we want.
    let Some(unpushed) = unpublished_ref_log(cwd, true) else {
        return GitWorkState::ProbeFailed;
    };
    if !unpushed.trim().is_empty() {
        // Reachable from no remote ref -- but a squash merge produces exactly
        // that, so ask whether these commits are already *in* the remote
        // default branch as patches before pinning (#993). Anything short of a
        // confident yes keeps the old, conservative answer.
        return if commits_are_squash_merged(cwd) {
            GitWorkState::Merged
        } else {
            GitWorkState::HasLocalWork
        };
    }

    // A linked worktree does not own the shared branches or stash. Their
    // metadata survives `git worktree remove`; only this checkout's HEAD
    // and dirty/untracked source can be lost with it.
    if linked_metadata_is_external(cwd) {
        return GitWorkState::Clean;
    }
    let Some(stash) = probe_git(
        cwd,
        &[
            "--no-optional-locks",
            "--git-dir=.git",
            "--work-tree=.",
            "stash",
            "list",
        ],
    ) else {
        return GitWorkState::ProbeFailed;
    };
    if !stash.trim().is_empty() {
        return GitWorkState::HasLocalWork;
    }

    GitWorkState::Clean
}

/// A `.git` file alone is insufficient: separate Git metadata can still
/// live inside the source being removed. Only a linked worktree whose
/// per-worktree and common metadata both resolve outside it qualifies.
fn linked_metadata_is_external(cwd: &Path) -> bool {
    let Some(root) = cwd.canonicalize().ok() else {
        return false;
    };
    let Some(gitfile) = std::fs::read_to_string(root.join(".git")).ok() else {
        return false;
    };
    let Some(gitdir) = gitfile.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let Some(gitdir) = root.join(gitdir.trim()).canonicalize().ok() else {
        return false;
    };
    let Some(common) = std::fs::read_to_string(gitdir.join("commondir")).ok() else {
        return false;
    };
    let Some(common) = gitdir.join(common.trim()).canonicalize().ok() else {
        return false;
    };
    gitdir != common && !gitdir.starts_with(&root) && !common.starts_with(&root)
}

fn unpublished_ref_log(cwd: &Path, first_only: bool) -> Option<String> {
    let mut args = vec![
        "--no-optional-locks",
        "--git-dir=.git",
        "--work-tree=.",
        "log",
    ];
    if linked_metadata_is_external(cwd) {
        // These refs live in this worktree's private Git directory and are
        // removed with it. Shared refs and stash live in the common directory.
        args.extend([
            "HEAD",
            "--glob=refs/worktree/*",
            "--glob=refs/bisect/*",
            "--glob=refs/rewritten/*",
        ]);
    } else {
        args.push("--all");
    }
    args.extend(["--not", "--remotes", "--format=%H"]);
    if first_only {
        args.push("--max-count=1");
    }
    probe_git(cwd, &args)
}

pub(super) fn tracked_source_purge_verdict(
    entry: &TrackedEntry,
    stale_after: Duration,
) -> PurgeDecision {
    if entry.kind == crate::gc::WORKTREE_KIND {
        let path = Path::new(&entry.path);
        extern_repo_purge_decision(path.is_dir(), true, true, git_work_state(path))
    } else {
        extern_repo_purge_verdict(entry, stale_after)
    }
}

/// Why an extern-repo row was or was not reclaimed. The repo's reap/spare
/// convention is to assert **spare + reason**, not just the outcome, so the
/// reason travels with the verdict instead of being recomputed at the log
/// site. Issue #896 surfaces these in `clud gc list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PurgeDecision {
    pub(crate) purge: bool,
    pub(crate) reason: &'static str,
    /// How `clud gc list` should classify the row (issue #896).
    ///
    /// Carried explicitly rather than inferred from `purge`, because "not
    /// purged" is three different things to a reader: a checkout GC will
    /// never auto-reclaim, one it simply has not aged into yet, and a row
    /// whose directory is gone. Collapsing them paints every
    /// recently-touched checkout yellow, which is the legibility loss #896
    /// exists to fix.
    pub(crate) class: PurgeClass,
}

/// The display classes a purge verdict maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PurgeClass {
    /// GC will take it on a future tick.
    Reclaimable,
    /// Not yet eligible, but nothing is holding it — it will age in.
    Spared,
    /// Deliberately retained; GC will keep refusing while this holds.
    Pinned,
    /// The registry row points at a path that is gone.
    Dangling,
}

/// Pure purge decision, factored out so the matrix is unit-testable without
/// touching the filesystem or spawning git.
///
/// | is_dir | idle  | merged_idle | git state    | purgeable |
/// |--------|-------|-------------|--------------|-----------|
/// | false  | *     | *           | *            | no (gone / unreadable)   |
/// | true   | false | false       | *            | no (still active)        |
/// | true   | *     | *           | HasLocalWork | no (pinned: local work)  |
/// | true   | *     | *           | ProbeFailed  | no (pinned: unreadable)  |
/// | true   | true  | *           | Clean        | yes                      |
/// | true   | true  | *           | Unknown      | no (unverified contents)     |
/// | true   | *     | true        | Merged       | yes (short window, #993)  |
///
/// Two idle flags because merged checkouts earn a shorter window: their work
/// is already shipped, so they are the *safest* thing to reclaim and the
/// least useful to keep. `idle` is the full `stale_after`; `merged_idle` is
/// [`merged_stale_after`]. Every other state still requires the full one, so
/// this only ever adds a reclaim path -- it never shortens an existing spare.
pub(super) fn extern_repo_purge_decision(
    is_dir: bool,
    idle: bool,
    merged_idle: bool,
    git: GitWorkState,
) -> PurgeDecision {
    if !is_dir {
        return PurgeDecision {
            purge: false,
            reason: "dangling: path missing",
            class: PurgeClass::Dangling,
        };
    }
    // A merged checkout may pass on the short window alone, so the gate is
    // "idle by *some* applicable measure"; the per-state arms below decide
    // which measure actually applies.
    if !idle && !merged_idle {
        return PurgeDecision {
            purge: false,
            reason: "spared: recently active",
            class: PurgeClass::Spared,
        };
    }
    match git {
        GitWorkState::HasLocalWork => PurgeDecision {
            purge: false,
            reason: "pinned: uncommitted or unpushed work",
            class: PurgeClass::Pinned,
        },
        GitWorkState::ProbeFailed => PurgeDecision {
            purge: false,
            reason: "pinned: git state unreadable",
            class: PurgeClass::Pinned,
        },
        GitWorkState::Merged if merged_idle => PurgeDecision {
            purge: true,
            reason: "reclaimable: already merged upstream",
            class: PurgeClass::Reclaimable,
        },
        GitWorkState::Merged => PurgeDecision {
            purge: false,
            reason: "spared: recently active",
            class: PurgeClass::Spared,
        },
        GitWorkState::Clean if idle => PurgeDecision {
            purge: true,
            reason: "reclaimable: clean and pushed",
            class: PurgeClass::Reclaimable,
        },
        GitWorkState::Unknown => PurgeDecision {
            purge: false,
            reason: "pinned: not a verified git checkout",
            class: PurgeClass::Pinned,
        },
        // Idle only by the merged window, but not merged: the full window
        // still governs.
        GitWorkState::Clean => PurgeDecision {
            purge: false,
            reason: "spared: recently active",
            class: PurgeClass::Spared,
        },
    }
}

/// How long a *merged* checkout must sit idle before it is reclaimed.
///
/// An eighth of the ordinary window -- three hours against the 24h default,
/// and it tracks `CLUD_GC_EXTERN_REPO_MAX_AGE_SECS` so an operator who widens
/// or narrows the main window moves this with it. Floored at a minute so a
/// deliberately tiny override cannot turn into "reclaim instantly".
pub(super) fn merged_stale_after(stale_after: Duration) -> Duration {
    (stale_after / 8).max(Duration::from_secs(60))
}

/// An extern-repo row is purgeable once the on-disk directory has been
/// inactive (no descendant `mtime` change) for `stale_after` **and** holds
/// no uncommitted/unpushed work. Anything the scanner tracks under
/// `<repo>/.extern-repos/` is, by convention, a clud-managed checkout, so
/// beyond the higher-level live-session check applied by `entry_is_live`
/// these two gates are the safety net: mtime throttles how eagerly we act,
/// the git-work check guards against deleting a checkout with local work.
/// Returns the full verdict. `gc list` (issue #896) displays the reason,
/// so it is recorded by the caller rather than discarded here.
pub(super) fn extern_repo_purge_verdict(
    entry: &TrackedEntry,
    stale_after: Duration,
) -> PurgeDecision {
    let path = Path::new(&entry.path);
    if !path.is_dir() {
        return extern_repo_purge_decision(false, false, false, GitWorkState::Unknown);
    }
    // A future mtime (fine-grained file timestamps ahead of this clock read,
    // or skew) means "touched just now": age zero. `.ok()` would turn it into
    // "age unknown", which no window can ever call idle.
    let age = most_recent_mtime(path).map(|mtime| {
        SystemTime::now()
            .duration_since(mtime)
            .unwrap_or(Duration::ZERO)
    });
    let idle = age.map(|age| age >= stale_after).unwrap_or(false);
    let merged_idle = age
        .map(|age| age >= merged_stale_after(stale_after))
        .unwrap_or(false);
    if !idle && !merged_idle {
        // Idle by no measure — skip the git probe entirely, as before. The
        // probe now starts at the *shorter* window, which is the only place
        // this costs anything: a checkout between the two windows is probed
        // where it previously was not, and only to find out whether it is
        // merged.
        return extern_repo_purge_decision(true, false, false, GitWorkState::Unknown);
    }
    let decision = extern_repo_purge_decision(true, idle, merged_idle, git_work_state(path));
    if !decision.purge {
        // The reason is the point: without it a pinned row is
        // indistinguishable from garbage that GC keeps failing to
        // collect. Issue #896 surfaces the same string in `gc list`.
        eprintln!("[clud] gc extern-repo {} — {}", entry.path, decision.reason);
    }
    decision
}

fn most_recent_mtime(path: &Path) -> Option<SystemTime> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let mut latest = metadata.modified().ok()?;
    if metadata.is_dir() {
        let entries = std::fs::read_dir(path).ok()?;
        for entry in entries.flatten() {
            if let Some(child_mtime) = most_recent_mtime(&entry.path()) {
                if child_mtime > latest {
                    latest = child_mtime;
                }
            }
        }
    }
    Some(latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1790: a container directory can hold nested clones and active
    /// container binds even when it has no `.git` of its own.
    #[test]
    fn unknown_container_directory_is_pinned_even_when_stale() {
        let decision = extern_repo_purge_decision(true, true, true, GitWorkState::Unknown);
        assert!(
            !decision.purge,
            "uninspected contents must survive: {decision:?}"
        );
        assert_eq!(decision.class, PurgeClass::Pinned);
        assert_eq!(decision.reason, "pinned: not a verified git checkout");
    }

    #[test]
    fn shared_git_metadata_is_not_disposable_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/worktrees/active-child")).unwrap();
        assert_eq!(git_work_state(dir.path()), GitWorkState::HasLocalWork);
    }

    #[test]
    fn nested_checkout_parent_is_not_a_disposable_clone() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("nested-clone");
        std::fs::create_dir_all(child.join(".git")).unwrap();
        std::fs::write(child.join("unpushed-work"), "irreplaceable").unwrap();
        let entry = TrackedEntry {
            id: 1,
            kind: "sibling-clone".into(),
            path: dir.path().to_string_lossy().into_owned(),
            repo_root: None,
            branch: None,
            agent_id: None,
            created_unix: 0,
        };
        let decision = extern_repo_purge_verdict(&entry, Duration::ZERO);
        assert!(!decision.purge);
        assert_eq!(decision.reason, "pinned: not a verified git checkout");
        assert!(child.join("unpushed-work").is_file());
    }

    /// A directory's mtime can read slightly ahead of `SystemTime::now()`
    /// (fine-grained file timestamps on a fast machine, or plain clock
    /// skew). That is "touched just now", age zero, not "age unknown": with
    /// a zero window it must not read as recently active, and with a real
    /// window it must still be spared.
    #[test]
    fn a_future_mtime_is_age_zero_not_recently_active_forever() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("extern");
        std::fs::create_dir(&repo).unwrap();
        let future = SystemTime::now() + Duration::from_secs(2);
        filetime::set_file_mtime(&repo, filetime::FileTime::from_system_time(future)).unwrap();
        let entry = TrackedEntry {
            id: 1,
            kind: "extern-repo".into(),
            path: repo.to_string_lossy().into_owned(),
            repo_root: None,
            branch: None,
            agent_id: None,
            created_unix: 0,
        };
        let zero = extern_repo_purge_verdict(&entry, Duration::ZERO);
        assert_ne!(zero.reason, "spared: recently active", "{zero:?}");
        let day = extern_repo_purge_verdict(&entry, Duration::from_secs(86_400));
        assert_eq!(day.reason, "spared: recently active", "{day:?}");
    }

    /// Assert **spare + reason**, not just the outcome: the reason is what
    /// makes a pinned row distinguishable from uncollected garbage.
    #[test]
    fn decision_matrix_covers_every_gate() {
        let cases = [
            // (is_dir, idle, git state)   → (purge, reason, display class)
            (
                (false, true, true, GitWorkState::Clean),
                (false, "dangling: path missing", PurgeClass::Dangling),
            ),
            (
                (true, false, false, GitWorkState::Clean),
                // Not yet eligible, but nothing is holding it: it ages in,
                // so it must NOT be classed as pinned (issue #896).
                (false, "spared: recently active", PurgeClass::Spared),
            ),
            (
                (true, true, true, GitWorkState::HasLocalWork),
                (
                    false,
                    "pinned: uncommitted or unpushed work",
                    PurgeClass::Pinned,
                ),
            ),
            (
                (true, true, true, GitWorkState::ProbeFailed),
                (false, "pinned: git state unreadable", PurgeClass::Pinned),
            ),
            (
                (true, true, true, GitWorkState::Clean),
                (
                    true,
                    "reclaimable: clean and pushed",
                    PurgeClass::Reclaimable,
                ),
            ),
            (
                (true, true, true, GitWorkState::Unknown),
                (
                    false,
                    "pinned: not a verified git checkout",
                    PurgeClass::Pinned,
                ),
            ),
        ];
        for ((is_dir, idle, merged_idle, git), (purge, reason, class)) in cases {
            let got = extern_repo_purge_decision(is_dir, idle, merged_idle, git);
            assert_eq!(
                got,
                PurgeDecision {
                    purge,
                    reason,
                    class
                },
                "is_dir={is_dir} idle={idle} merged_idle={merged_idle} git={git:?}"
            );
        }
    }

    /// A probe we could not complete must never authorize a delete. This is
    /// the timeout path: `Unknown` purges on mtime alone, so mapping a hung
    /// git onto it would delete a checkout nobody ever inspected.
    #[test]
    fn unreadable_git_state_is_spared_not_purged() {
        assert!(!extern_repo_purge_decision(true, true, true, GitWorkState::ProbeFailed).purge);
        assert!(!extern_repo_purge_decision(true, true, true, GitWorkState::Unknown).purge);
    }

    /// Build `<tmp>/origin` (a real remote) plus `<tmp>/work` cloned from it,
    /// then squash-merge `work`'s feature commit into the remote's `main` and
    /// delete the branch — the shape #993 describes, and the one this repo
    /// produces on every merge.
    fn squash_merged_clone(tmp: &Path) -> std::path::PathBuf {
        let origin = tmp.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        init_repo(&origin);
        std::fs::write(origin.join("base.txt"), "base\n").unwrap();
        git(&origin, &["add", "-A"]);
        git(&origin, &["commit", "-m", "base"]);

        let work = tmp.join("work");
        git(
            tmp,
            &["clone", origin.to_str().unwrap(), work.to_str().unwrap()],
        );
        init_repo_identity(&work);

        // A feature commit in the checkout...
        std::fs::write(work.join("feature.txt"), "feature\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "feature"]);

        // ...landed on the remote as a *new* squashed commit. Same tree, so
        // the same patch id; different sha, so no ancestry relationship.
        std::fs::write(origin.join("feature.txt"), "feature\n").unwrap();
        git(&origin, &["add", "-A"]);
        git(&origin, &["commit", "-m", "feature (squashed)"]);

        // The checkout learns about it, exactly as a `git fetch` would.
        git(&work, &["fetch", "origin"]);
        git(&work, &["remote", "set-head", "origin", "main"]);
        work
    }

    fn init_repo_identity(root: &Path) {
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "t"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["config", "core.hooksPath", ""]);
    }

    /// Issue #993, gap 2: a squash-merged checkout was pinned **forever**.
    ///
    /// Its commits are ancestors of nothing on the remote, so
    /// `log --all --not --remotes` reports them and the old probe called that
    /// unpushed work. The checkouts that should be reclaimed soonest were the
    /// only ones that never could be.
    #[test]
    fn a_squash_merged_checkout_is_merged_not_local_work() {
        let tmp = tempfile::tempdir().unwrap();
        let work = squash_merged_clone(tmp.path());
        assert_eq!(git_work_state(&work), GitWorkState::Merged);
    }

    /// The conservative default is untouched: a commit that is genuinely not
    /// upstream in any form still pins the checkout.
    #[test]
    fn an_unmerged_commit_still_reads_as_local_work() {
        let tmp = tempfile::tempdir().unwrap();
        let work = squash_merged_clone(tmp.path());
        std::fs::write(work.join("later.txt"), "not upstream\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "later"]);
        assert_eq!(git_work_state(&work), GitWorkState::HasLocalWork);
    }

    /// `git cherry` speaks only for HEAD, so unpushed work on any *other* ref
    /// must veto the merged verdict rather than be judged unseen.
    #[test]
    fn unmerged_work_on_another_branch_vetoes_the_merged_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let work = squash_merged_clone(tmp.path());
        git(&work, &["checkout", "-b", "side"]);
        std::fs::write(work.join("side.txt"), "side\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "side work"]);
        git(&work, &["checkout", "main"]);
        assert_eq!(git_work_state(&work), GitWorkState::HasLocalWork);
    }

    /// A merged checkout is reclaimed on the short window; every other state
    /// still waits the full one, so this only adds a path.
    #[test]
    fn merged_earns_the_short_window_and_nothing_else_does() {
        let full = Duration::from_secs(24 * 60 * 60);
        assert_eq!(merged_stale_after(full), Duration::from_secs(3 * 60 * 60));
        // A tiny override cannot become "reclaim instantly".
        assert_eq!(
            merged_stale_after(Duration::from_secs(10)),
            Duration::from_secs(60)
        );

        // Idle by the short window only.
        assert!(extern_repo_purge_decision(true, false, true, GitWorkState::Merged).purge);
        assert!(!extern_repo_purge_decision(true, false, true, GitWorkState::Clean).purge);
        assert!(!extern_repo_purge_decision(true, false, true, GitWorkState::Unknown).purge);
        assert!(!extern_repo_purge_decision(true, false, true, GitWorkState::HasLocalWork).purge);
        assert!(!extern_repo_purge_decision(true, false, true, GitWorkState::ProbeFailed).purge);
        // Not idle at all.
        assert!(!extern_repo_purge_decision(true, false, false, GitWorkState::Merged).purge);
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        probe_git(cwd, args)
            .unwrap_or_else(|| panic!("git {} failed in {}", args.join(" "), cwd.display()))
    }

    fn init_repo(root: &Path) {
        git(root, &["init", "--initial-branch=main"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "t"]);
        // Insulate the fixture from the developer's global config: a global
        // `commit.gpgsign` with no key, or a `core.hooksPath` pointing at
        // real hooks, would otherwise fail `git commit` here.
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["config", "core.hooksPath", ""]);
    }

    /// Exit is not end of output. The background `sleep` keeps the pipe open
    /// after `sh` exits, so the unterminated `tail` reaches the queue only at
    /// pipe EOF, a second after the exit code. That is the loaded-machine
    /// race made deterministic: the old fixed 5 ms drain after exit returned
    /// an empty stdout here, which is how `git rev-list --count` read as
    /// nothing and a landed worktree was pinned as `unverifiable`.
    #[test]
    fn probe_cmd_keeps_output_that_arrives_after_the_exit_code() {
        if !crate::platform::host::UNIX {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (code, out) = probe_cmd(
            "sh",
            tmp.path(),
            &["-c", "printf 'head\\ntail'; sleep 1 &"],
            Duration::from_secs(10),
        )
        .expect("sh must run");
        assert_eq!(code, 0);
        assert_eq!(out, "head\ntail\n");
    }

    #[test]
    fn standalone_non_git_dir_reads_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(git_work_state(tmp.path()), GitWorkState::Unknown);
    }

    #[test]
    fn linked_worktree_is_not_pinned_by_another_branch_or_shared_stash() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/main", base.trim()],
        );
        let child = tmp.path().join("child");
        git(
            &repo,
            &["worktree", "add", "-b", "child", child.to_str().unwrap()],
        );
        git(
            &repo,
            &["commit", "--allow-empty", "-m", "unrelated local work"],
        );
        std::fs::write(repo.join("scratch"), "keep this stash").unwrap();
        git(&repo, &["stash", "push", "--include-untracked"]);
        assert_eq!(git_work_state(&child), GitWorkState::Clean);
        std::fs::write(child.join("local-work"), "must survive").unwrap();
        assert_eq!(git_work_state(&child), GitWorkState::HasLocalWork);
        git(&child, &["add", "local-work"]);
        git(&child, &["commit", "-m", "unpushed worktree work"]);
        assert_eq!(git_work_state(&child), GitWorkState::HasLocalWork);
        git(&child, &["reset", "--hard", base.trim()]);
        assert_eq!(git_work_state(&child), GitWorkState::Clean);
        let stash = git(&repo, &["stash", "list"]);
        let unrelated_tip = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["worktree", "remove", child.to_str().unwrap()]);
        assert_eq!(git(&repo, &["stash", "list"]), stash);
        assert_eq!(git(&repo, &["rev-parse", "HEAD"]), unrelated_tip);
        assert!(repo.join(".git").is_dir());
    }

    #[test]
    fn linked_worktree_preserves_unpublished_private_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        git(&repo, &["commit", "--allow-empty", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/main", base.trim()],
        );
        let child = tmp.path().join("child");
        git(
            &repo,
            &["worktree", "add", "-b", "child", child.to_str().unwrap()],
        );
        git(
            &child,
            &["commit", "--allow-empty", "-m", "private unpublished work"],
        );
        let private = git(&child, &["rev-parse", "HEAD"]);
        git(&child, &["reset", "--hard", base.trim()]);
        for name in [
            "refs/worktree/private",
            "refs/bisect/private",
            "refs/rewritten/private",
        ] {
            git(&child, &["update-ref", name, private.trim()]);
            assert_eq!(git_work_state(&child), GitWorkState::HasLocalWork, "{name}");
            assert_eq!(git(&child, &["rev-parse", name]).trim(), private.trim());
            git(&child, &["update-ref", "-d", name]);
        }
        assert_eq!(git_work_state(&child), GitWorkState::Clean);
    }

    /// Regression guard for the production layout. `.extern-repos/` lives
    /// *inside* the parent repo and the scanner tracks every immediate
    /// child, git or not. Before the `.git` anchor, git walked up and
    /// answered about the parent: a plain directory next to a parent with
    /// one untracked file reported `HasLocalWork` and was pinned forever,
    /// and `Unknown` was unreachable in production. Without this test a
    /// probe that always says `HasLocalWork` passes the whole suite.
    #[test]
    fn non_git_dir_nested_inside_a_repo_still_reads_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        init_repo(parent);
        // Make the parent unmistakably dirty.
        std::fs::write(parent.join("dirty.txt"), "uncommitted").unwrap();

        let child = parent.join(".extern-repos").join("plain-dir");
        std::fs::create_dir_all(&child).unwrap();

        assert_eq!(
            git_work_state(&child),
            GitWorkState::Unknown,
            "probe must answer about the checkout, not the enclosing repo"
        );
    }

    /// A bare clone/mirror has no `.git` and no work tree, but it does hold
    /// refs and objects. It must be spared, not handed to the mtime-only
    /// verdict that would delete it without ever looking inside.
    #[test]
    fn bare_repo_is_spared_rather_than_purged_uninspected() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "--bare", "mirror.git"]);
        let bare = tmp.path().join("mirror.git");

        assert!(!bare.join(".git").exists(), "fixture is not actually bare");
        assert_eq!(git_work_state(&bare), GitWorkState::ProbeFailed);
        assert!(!extern_repo_purge_decision(true, true, true, git_work_state(&bare)).purge);
    }

    /// Regression guard for the hole the `.git` *existence* gate left open.
    /// An interrupted clone leaves a `.git` that exists but is not a usable
    /// gitdir; `exists()` passes it through, and git then resumes its
    /// upward walk. Verified against real git: from such a directory inside
    /// a dirty parent repo, `status --porcelain` exits 0 and prints the
    /// **parent's** entries — so without the pinned `--git-dir`/`--work-tree`
    /// this reads as `HasLocalWork` (pinned forever on a dirty parent) or,
    /// worse, as `Clean` on a tidy parent, authorizing a delete of a
    /// checkout that was never inspected.
    #[test]
    fn malformed_git_dir_inside_a_repo_does_not_borrow_the_parents_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        init_repo(parent);
        std::fs::write(parent.join("dirty.txt"), "uncommitted").unwrap();

        // `.git` exists, so the gate passes — but it is empty, so it is not
        // a repository.
        let child = parent.join(".extern-repos").join("interrupted-clone");
        std::fs::create_dir_all(child.join(".git")).unwrap();

        assert_eq!(
            git_work_state(&child),
            GitWorkState::ProbeFailed,
            "a malformed .git must fail closed, not inherit the parent repo"
        );
        // Fail closed means spare, never purge.
        assert!(!extern_repo_purge_decision(true, true, true, GitWorkState::ProbeFailed).purge);
    }

    #[test]
    fn unpushed_or_untracked_repo_is_pinned() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_repo(root);
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-m", "init"]);

        // Committed but with no remote → the commit counts as unpushed work.
        assert_eq!(git_work_state(root), GitWorkState::HasLocalWork);

        // An untracked file is likewise local work.
        std::fs::write(root.join("scratch.txt"), "wip").unwrap();
        assert_eq!(git_work_state(root), GitWorkState::HasLocalWork);
    }

    /// The only test that produces `Clean` from a real repository — without
    /// it, a `git_work_state` hardwired to `HasLocalWork` passes everything.
    /// Uses a local bare upstream so no network (and no `git clone`, which
    /// the repo's own guard blocks) is involved.
    #[test]
    fn clean_fully_pushed_checkout_is_reclaimable() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(tmp.path(), &["init", "--bare", "upstream.git"]);

        init_repo(&work);
        std::fs::write(work.join("a.txt"), "hello").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "init"]);
        git(
            &work,
            &["remote", "add", "origin", &upstream.to_string_lossy()],
        );
        git(&work, &["push", "-u", "origin", "main"]);

        // Sanity-check the fixture itself: `--remotes` only sees
        // `refs/remotes/*`, so a push that left no tracking ref would make
        // the assertion below vacuous.
        assert!(
            !git(
                &work,
                &["for-each-ref", "--format=%(refname)", "refs/remotes"]
            )
            .trim()
            .is_empty(),
            "fixture produced no remote-tracking ref"
        );

        assert_eq!(git_work_state(&work), GitWorkState::Clean);

        // A commit made on a detached HEAD is still local work: `--all`
        // (not `--branches`) is what catches it.
        git(&work, &["checkout", "--detach"]);
        std::fs::write(work.join("b.txt"), "detached").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "detached work"]);
        assert_eq!(git_work_state(&work), GitWorkState::HasLocalWork);
    }
}
