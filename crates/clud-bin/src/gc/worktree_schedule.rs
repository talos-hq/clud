//! Issue #2101 item 1: warn, then clean.
//!
//! When `~/.clud/worktrees` is over `worktrees.warn_bytes`, each of the
//! largest worktrees (`gc::worktree_top`) that the verdict cannot prove safe
//! gets a removal date [`GRACE_SECS`] out, recorded in
//! `~/.clud/worktrees-schedule.json`. The launch banner names the date. A
//! tick after the date moves the worktree to `clud trash` (recoverable for
//! 72 h) instead of deleting it. A candidate leaves the schedule as soon as
//! it is kept (`clud gc keep`), in use, reclaimable by the normal verdict, or
//! no longer among the largest, and the whole schedule clears once the root
//! is back under the limit.
//!
//! #2487 adds age retention: a worktree idle for [`MAX_AGE_SECS`] is a
//! candidate even while the root is under budget, with the same exemptions
//! (kept, in use, dirty or unpushed, reclaimable elsewhere) and the same
//! warn-then-trash grace. Every row under the root is considered, not only
//! the largest.
//!
//! [`plan`] is the pure decision; the daemon's probe thread gathers the facts
//! and performs the moves.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name of the schedule, a sibling of `worktrees`.
pub const SCHEDULE_FILE_NAME: &str = "worktrees-schedule.json";

/// Warning period before a scheduled worktree moves to trash.
pub const GRACE_SECS: i64 = 7 * 24 * 60 * 60;

/// A worktree has to be idle this long before it is even warned about.
pub const MIN_IDLE_SECS: i64 = 24 * 60 * 60;

/// #2487: idle this long, a clean, unused worktree is scheduled regardless
/// of the root's size. With [`GRACE_SECS`] it leaves after three weeks idle.
pub const MAX_AGE_SECS: i64 = 14 * 24 * 60 * 60;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// Path → unix time after which it moves to trash.
    #[serde(default)]
    pub remove_after: BTreeMap<String, i64>,
}

/// One of the largest worktrees, with the facts that can exempt it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub bytes: u64,
    pub idle_secs: i64,
    /// Kept, in use, unknown to the probe, or already reclaimable.
    pub exempt: bool,
    /// Why the worktree holds work that trashing would lose (uncommitted
    /// tracked changes, unpushed commits, or unreadable git state). Only
    /// probed for candidates about to be scheduled; see [`mark_dirty`].
    pub dirty: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Newly scheduled; log the date.
    Warn {
        path: String,
        bytes: u64,
        remove_after: i64,
    },
    /// The date has passed: move it to trash.
    Trash { path: String },
    /// Exempt because it holds unsaved work; log "kept: dirty".
    KeptDirty { path: String, reason: String },
}

/// Candidates that would be scheduled this tick: not exempt and idle long
/// enough. Only these are worth a git probe.
fn schedulable(c: &Candidate) -> bool {
    !c.exempt && c.idle_secs >= MIN_IDLE_SECS
}

/// #2487: idle past [`MAX_AGE_SECS`] and otherwise schedulable.
pub fn aged(c: &Candidate) -> bool {
    schedulable(c) && c.idle_secs >= MAX_AGE_SECS
}

/// Schedulable this tick: over budget, or [`aged`].
pub fn due(c: &Candidate, over: bool) -> bool {
    schedulable(c) && (over || aged(c))
}

/// Fill `dirty` for every [`due`] candidate with `probe`, which returns the
/// reason a worktree holds unsaved work. The probe must fail closed (an
/// unreadable repo is reported as dirty).
pub fn mark_dirty(
    candidates: &mut [Candidate],
    over: bool,
    probe: &dyn Fn(&str) -> Option<String>,
) {
    for c in candidates.iter_mut().filter(|c| due(c, over)) {
        c.dirty = probe(&c.path);
    }
}

/// Git facts that make trashing a worktree lossy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirtyFacts {
    /// `git status --porcelain --untracked-files=all --ignored=no` is non-empty
    /// (modified, staged or untracked-new files).
    pub tracked_changes: bool,
    /// HEAD holds commits no remote-tracking ref reaches.
    pub unpushed: bool,
    /// Git could not be read; treated as dirty.
    pub unreadable: Option<String>,
}

/// The reason to keep the worktree, or `None` when it is safe to trash.
pub fn dirty_reason(f: &DirtyFacts) -> Option<String> {
    if let Some(e) = &f.unreadable {
        return Some(format!("git state unreadable: {e}"));
    }
    match (f.tracked_changes, f.unpushed) {
        (true, true) => Some("uncommitted tracked changes and unpushed commits".into()),
        (true, false) => Some("uncommitted tracked changes".into()),
        (false, true) => Some("unpushed commits".into()),
        (false, false) => None,
    }
}

/// Gather [`DirtyFacts`] with git. Every failure fails closed.
pub fn probe_dirty(dir: &Path) -> DirtyFacts {
    let mut facts = DirtyFacts::default();
    match crate::worktrees::run_git(
        dir,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored=no",
        ],
    ) {
        Ok(out) => facts.tracked_changes = !out.trim().is_empty(),
        Err(e) => {
            facts.unreadable = Some(e);
            return facts;
        }
    }
    match crate::rm_tool::has_unpushed(dir, "HEAD") {
        Ok(v) => facts.unpushed = v,
        Err(e) => facts.unreadable = Some(e),
    }
    facts
}

/// Update `schedule` for this tick and return what to do.
pub fn plan(
    schedule: &mut Schedule,
    over: bool,
    candidates: &[Candidate],
    now: i64,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut eligible: Vec<&Candidate> = Vec::new();
    for c in candidates.iter().filter(|c| due(c, over)) {
        match &c.dirty {
            // Dropped from the schedule below, even if already scheduled.
            Some(reason) => actions.push(Action::KeptDirty {
                path: c.path.clone(),
                reason: reason.clone(),
            }),
            None => eligible.push(c),
        }
    }
    schedule
        .remove_after
        .retain(|path, _| eligible.iter().any(|c| &c.path == path));
    for c in eligible {
        match schedule.remove_after.get(&c.path) {
            Some(&after) if now >= after => {
                schedule.remove_after.remove(&c.path);
                actions.push(Action::Trash {
                    path: c.path.clone(),
                });
            }
            Some(_) => {}
            None => {
                let remove_after = now + GRACE_SECS;
                schedule.remove_after.insert(c.path.clone(), remove_after);
                actions.push(Action::Warn {
                    path: c.path.clone(),
                    bytes: c.bytes,
                    remove_after,
                });
            }
        }
    }
    actions
}

/// `<wt_root>/../worktrees-schedule.json`.
pub fn schedule_path_for(wt_root: &Path) -> PathBuf {
    wt_root.parent().unwrap_or(wt_root).join(SCHEDULE_FILE_NAME)
}

/// Missing or corrupt reads as empty: an empty schedule only delays.
pub fn load(wt_root: &Path) -> Schedule {
    fs::read_to_string(schedule_path_for(wt_root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Atomic write; an empty schedule removes the file.
pub fn save(wt_root: &Path, schedule: &Schedule) -> std::io::Result<()> {
    let path = schedule_path_for(wt_root);
    if schedule.remove_after.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let text = serde_json::to_string(schedule).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &path)
}

/// Banner suffix: `; moves to trash unless kept: a in 3d, b in 6d`.
pub fn banner_suffix(schedule: &Schedule, now: i64) -> String {
    if schedule.remove_after.is_empty() {
        return String::new();
    }
    let items: Vec<String> = schedule
        .remove_after
        .iter()
        .map(|(path, after)| {
            let name = Path::new(path)
                .file_name()
                .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
            let days = ((after - now).max(0) as u64).div_ceil(24 * 60 * 60);
            format!("{name} in {days}d")
        })
        .collect();
    format!(
        "; moves to trash unless `clud gc keep`: {}",
        items.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60;

    fn cand(path: &str, exempt: bool) -> Candidate {
        Candidate {
            path: path.to_string(),
            bytes: 10,
            idle_secs: 2 * DAY,
            exempt,
            dirty: None,
        }
    }

    #[test]
    fn warns_first_then_trashes_after_the_date() {
        let mut s = Schedule::default();
        let c = [cand("/wt/a", false)];
        let first = plan(&mut s, true, &c, 0);
        assert_eq!(
            first,
            [Action::Warn {
                path: "/wt/a".into(),
                bytes: 10,
                remove_after: GRACE_SECS
            }]
        );
        assert!(
            plan(&mut s, true, &c, GRACE_SECS - 1).is_empty(),
            "no repeat warning"
        );
        assert_eq!(
            plan(&mut s, true, &c, GRACE_SECS),
            [Action::Trash {
                path: "/wt/a".into()
            }]
        );
        assert!(s.remove_after.is_empty());
    }

    fn dirty_cand(path: &str) -> Candidate {
        Candidate {
            dirty: Some("uncommitted tracked changes".into()),
            ..cand(path, false)
        }
    }

    #[test]
    fn dirty_reason_covers_each_fact_and_fails_closed() {
        assert_eq!(dirty_reason(&DirtyFacts::default()), None);
        let tracked = DirtyFacts {
            tracked_changes: true,
            ..DirtyFacts::default()
        };
        assert!(dirty_reason(&tracked).unwrap().contains("uncommitted"));
        let unpushed = DirtyFacts {
            unpushed: true,
            ..DirtyFacts::default()
        };
        assert!(dirty_reason(&unpushed).unwrap().contains("unpushed"));
        let unreadable = DirtyFacts {
            unreadable: Some("boom".into()),
            ..DirtyFacts::default()
        };
        assert!(dirty_reason(&unreadable).unwrap().contains("unreadable"));
        let missing = std::path::Path::new("/nonexistent/clud-dirty-probe");
        assert!(dirty_reason(&probe_dirty(missing)).is_some());
    }

    #[test]
    fn only_schedulable_candidates_are_probed() {
        let mut exempt = cand("/wt/exempt", true);
        let mut fresh = cand("/wt/fresh", false);
        fresh.idle_secs = 1;
        let mut cands = [exempt.clone(), fresh, cand("/wt/ok", false)];
        let probed = std::cell::RefCell::new(Vec::new());
        mark_dirty(&mut cands, true, &|p| {
            probed.borrow_mut().push(p.to_string());
            Some("dirty".into())
        });
        assert_eq!(*probed.borrow(), ["/wt/ok"]);
        exempt.dirty = None;
        assert_eq!(cands[0], exempt);
        assert_eq!(cands[2].dirty.as_deref(), Some("dirty"));
    }

    #[test]
    fn dirty_worktrees_are_never_scheduled_and_are_reported() {
        let mut s = Schedule::default();
        let acts = plan(&mut s, true, &[dirty_cand("/wt/d")], 0);
        assert_eq!(
            acts,
            [Action::KeptDirty {
                path: "/wt/d".into(),
                reason: "uncommitted tracked changes".into()
            }]
        );
        assert!(s.remove_after.is_empty());
    }

    #[test]
    fn a_scheduled_worktree_that_becomes_dirty_is_unscheduled() {
        let mut s = Schedule::default();
        plan(&mut s, true, &[cand("/wt/a", false)], 0);
        assert!(s.remove_after.contains_key("/wt/a"));
        let acts = plan(&mut s, true, &[dirty_cand("/wt/a")], GRACE_SECS);
        assert!(!acts.iter().any(|a| matches!(a, Action::Trash { .. })));
        assert!(s.remove_after.is_empty());
    }

    #[test]
    fn kept_recent_and_under_budget_are_never_trashed() {
        let mut s = Schedule::default();
        plan(&mut s, true, &[cand("/wt/a", false)], 0);
        // Kept (or in use, or reclaimable) after the warning: dropped.
        assert!(plan(&mut s, true, &[cand("/wt/a", true)], GRACE_SECS).is_empty());
        assert!(s.remove_after.is_empty());
        // Recently used: never scheduled.
        let mut fresh = cand("/wt/b", false);
        fresh.idle_secs = DAY - 1;
        assert!(plan(&mut s, true, &[fresh], 0).is_empty());
        // Back under budget: the schedule clears.
        plan(&mut s, true, &[cand("/wt/c", false)], 0);
        assert!(plan(&mut s, false, &[cand("/wt/c", false)], GRACE_SECS).is_empty());
        assert!(s.remove_after.is_empty());
    }

    #[test]
    fn an_aged_clean_worktree_is_scheduled_under_budget() {
        let mut s = Schedule::default();
        let mut old = cand("/wt/old", false);
        old.idle_secs = MAX_AGE_SECS;
        let mut young = cand("/wt/young", false);
        young.idle_secs = MAX_AGE_SECS - 1;
        let acts = plan(&mut s, false, &[old.clone(), young], 0);
        assert_eq!(
            acts,
            [Action::Warn {
                path: "/wt/old".into(),
                bytes: 10,
                remove_after: GRACE_SECS
            }]
        );
        assert_eq!(
            plan(&mut s, false, &[old], GRACE_SECS),
            [Action::Trash {
                path: "/wt/old".into()
            }]
        );
    }

    #[test]
    fn an_aged_worktree_with_unsaved_work_or_in_use_is_never_scheduled() {
        let mut s = Schedule::default();
        let mut dirty = dirty_cand("/wt/d");
        dirty.idle_secs = MAX_AGE_SECS * 10;
        let mut used = cand("/wt/u", true);
        used.idle_secs = MAX_AGE_SECS * 10;
        let acts = plan(&mut s, false, &[dirty, used], 0);
        assert_eq!(
            acts,
            [Action::KeptDirty {
                path: "/wt/d".into(),
                reason: "uncommitted tracked changes".into()
            }]
        );
        assert!(s.remove_after.is_empty());
    }

    #[test]
    fn aged_candidates_are_probed_under_budget() {
        let mut old = cand("/wt/old", false);
        old.idle_secs = MAX_AGE_SECS;
        let mut cands = [old, cand("/wt/young", false)];
        let probed = std::cell::RefCell::new(Vec::new());
        mark_dirty(&mut cands, false, &|p| {
            probed.borrow_mut().push(p.to_string());
            None
        });
        assert_eq!(*probed.borrow(), ["/wt/old"]);
    }

    #[test]
    fn schedule_round_trips_and_names_days_left() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("worktrees");
        let mut s = Schedule::default();
        s.remove_after
            .insert(root.join("repo-wt-x").to_string_lossy().into(), 3 * DAY);
        save(&root, &s).unwrap();
        let loaded = load(&root);
        assert_eq!(loaded, s);
        assert_eq!(
            banner_suffix(&loaded, DAY / 2),
            "; moves to trash unless `clud gc keep`: repo-wt-x in 3d"
        );
        save(&root, &Schedule::default()).unwrap();
        assert!(!schedule_path_for(&root).exists());
    }
}
