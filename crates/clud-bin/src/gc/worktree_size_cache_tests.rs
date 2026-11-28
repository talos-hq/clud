//! Issue #1610: decision table for the launch-banner worktrees size warning and
//! the cache round trip. Every test works under a `tempdir`; none touches the
//! real `~/.clud`.

use super::*;
use kernal_api::platform::fs::TemporaryDirectory;

const GIB: u64 = 1 << 30;
const NOW: i64 = 1_800_000_000;

fn cached(check: SizeCheck, age_secs: i64) -> CachedTreeSize {
    CachedTreeSize {
        checked_unix: NOW - age_secs,
        warn_bytes: 50 * GIB,
        check,
    }
}

/// One row per reason: (cache, warn_bytes, expected verdict).
#[test]
fn banner_decision_table() {
    let over = cached(SizeCheck::Over(60 * GIB), 60);
    let rows: Vec<(&str, Option<CachedTreeSize>, u64, BannerVerdict)> = vec![
        (
            "over threshold, fresh",
            Some(over),
            50 * GIB,
            BannerVerdict::Warn { bytes: 60 * GIB },
        ),
        (
            "disabled wins even when over",
            Some(over),
            0,
            BannerVerdict::Skip(SkipReason::Disabled),
        ),
        (
            "no cache shows nothing",
            None,
            50 * GIB,
            BannerVerdict::Skip(SkipReason::NoCache),
        ),
        (
            "stale cache shows nothing",
            Some(cached(SizeCheck::Over(60 * GIB), MAX_CACHE_AGE_SECS + 1)),
            50 * GIB,
            BannerVerdict::Skip(SkipReason::Stale),
        ),
        (
            "exactly at max age is still fresh",
            Some(cached(SizeCheck::Over(60 * GIB), MAX_CACHE_AGE_SECS)),
            50 * GIB,
            BannerVerdict::Warn { bytes: 60 * GIB },
        ),
        (
            "timestamp from the future is not trusted",
            Some(cached(SizeCheck::Over(60 * GIB), -(FUTURE_SKEW_SECS + 1))),
            50 * GIB,
            BannerVerdict::Skip(SkipReason::FutureTimestamp),
        ),
        (
            "under threshold",
            Some(cached(SizeCheck::Under(10 * GIB), 60)),
            50 * GIB,
            BannerVerdict::Skip(SkipReason::Under),
        ),
        (
            "threshold raised past the cached lower bound",
            Some(over),
            100 * GIB,
            BannerVerdict::Skip(SkipReason::Under),
        ),
        (
            "threshold lowered below an exact under total",
            Some(cached(SizeCheck::Under(10 * GIB), 60)),
            5 * GIB,
            BannerVerdict::Warn { bytes: 10 * GIB },
        ),
        (
            "budget exhausted: unproven, stay quiet",
            Some(cached(SizeCheck::Unknown, 60)),
            50 * GIB,
            BannerVerdict::Skip(SkipReason::Unknown),
        ),
    ];
    for (name, cache, warn, expected) in rows {
        assert_eq!(
            banner_decision(cache.as_ref(), warn, NOW),
            expected,
            "row: {name}"
        );
    }
}

#[test]
fn banner_line_names_size_threshold_path_and_setting() {
    let root = Path::new("/h/.clud/worktrees");
    let line = banner_line(root, 60 * GIB, 50 * GIB);
    assert!(!line.contains('\n'), "one line: {line}");
    assert!(line.contains("60.0 GiB"), "{line}");
    assert!(line.contains("50.0 GiB"), "{line}");
    assert!(line.contains(&root.display().to_string()), "{line}");
    assert!(line.contains("worktrees.warn_bytes"), "{line}");
}

#[test]
fn cache_round_trips_and_lives_beside_tmp_wt() {
    let home = TemporaryDirectory::new().unwrap();
    let root = crate::gc::worktree_root::worktree_root_for(home.path());
    let path = cache_path_for(&root);
    assert_eq!(path.parent(), root.parent());
    for check in [SizeCheck::Over(7), SizeCheck::Under(3), SizeCheck::Unknown] {
        let entry = cached(check, 0);
        write_cache(&path, &entry).unwrap();
        assert_eq!(read_cache(&path), Some(entry));
    }
}

#[test]
fn missing_or_corrupt_cache_reads_as_none() {
    let dir = TemporaryDirectory::new().unwrap();
    let path = dir.path().join("worktrees-size.json");
    assert_eq!(read_cache(&path), None);
    std::fs::write(&path, b"{not json").unwrap();
    assert_eq!(read_cache(&path), None);
    std::fs::write(&path, br#"{"checked_unix":1,"state":"weird","bytes":1}"#).unwrap();
    assert_eq!(read_cache(&path), None);
}

#[test]
fn launch_warning_reads_cache_only_and_is_failure_silent() {
    let home = TemporaryDirectory::new().unwrap();
    let root = crate::gc::worktree_root::worktree_root_for(home.path());
    // No cache, no settings, no worktrees dir: nothing, no panic.
    assert_eq!(launch_warning_at(home.path(), NOW), None);
    write_cache(
        &cache_path_for(&root),
        &cached(SizeCheck::Over(60 * GIB), 60),
    )
    .unwrap();
    let line = launch_warning_at(home.path(), NOW).expect("fresh over cache warns");
    assert!(line.contains("60.0 GiB"), "{line}");
    // warn_bytes = 0 in settings silences it.
    let settings = crate::clud_settings::settings_path_at(home.path());
    std::fs::write(&settings, br#"{"worktrees":{"warn_bytes":0}}"#).unwrap();
    assert_eq!(launch_warning_at(home.path(), NOW), None);
}

#[test]
fn refresh_cache_records_a_bounded_check() {
    let home = TemporaryDirectory::new().unwrap();
    let root = crate::gc::worktree_root::worktree_root_for(home.path());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("blob"), vec![0u8; 200]).unwrap();
    refresh_cache(&root, 100, NOW).unwrap();
    let entry = read_cache(&cache_path_for(&root)).unwrap();
    assert_eq!(entry.checked_unix, NOW);
    assert_eq!(entry.warn_bytes, 100);
    assert!(matches!(entry.check, SizeCheck::Over(b) if b > 100));
}

// ---- #1327: the same warn-only cached size check for `~/.clud/tmp` ----

#[test]
fn tmp_cache_lives_beside_tmp_not_inside_it() {
    let home = TemporaryDirectory::new().unwrap();
    let tmp = crate::gc::session_tmp::session_tmp_dir_for(home.path());
    let path = tmp_cache_path_for(home.path());
    assert_eq!(path.parent(), tmp.parent(), "sibling of ~/.clud/tmp");
    assert!(!path.starts_with(&tmp), "the 72 h sweep must never see it");
    assert_ne!(
        path,
        cache_path_for(&crate::gc::worktree_root::worktree_root_for(home.path()))
    );
}

#[test]
fn tmp_banner_line_names_tmp_setting_and_says_warn_only() {
    let root = Path::new("/h/.clud/tmp");
    let line = tmp_banner_line(root, 30 * GIB, 20 * GIB);
    assert!(!line.contains('\n'), "one line: {line}");
    assert!(line.contains("30.0 GiB"), "{line}");
    assert!(line.contains("20.0 GiB"), "{line}");
    assert!(line.contains(&root.display().to_string()), "{line}");
    assert!(line.contains("tmp.warn_bytes"), "{line}");
    assert!(!line.contains("worktrees.warn_bytes"), "{line}");
}

#[test]
fn tmp_list_warning_is_quiet_under_and_disabled() {
    let root = Path::new("/h/.clud/tmp");
    assert_eq!(tmp_list_warning(root, 10, SizeCheck::Under(5)), None);
    assert_eq!(tmp_list_warning(root, 0, SizeCheck::Over(50)), None);
    let over = tmp_list_warning(root, 10, SizeCheck::Over(50)).unwrap();
    assert!(over.contains("tmp.warn_bytes"), "{over}");
    assert!(over.contains("keeping it above the cap"), "{over}");
    let unknown = tmp_list_warning(root, 10, SizeCheck::Unknown).unwrap();
    assert!(unknown.contains("tmp.warn_bytes"), "{unknown}");
}

#[test]
fn tmp_refresh_and_launch_warning_round_trip_and_never_delete() {
    let home = TemporaryDirectory::new().unwrap();
    // No cache, no settings, no tmp dir: nothing, no panic.
    assert_eq!(tmp_launch_warning_at(home.path(), NOW), None);
    let tmp = crate::gc::session_tmp::session_tmp_dir_for(home.path());
    std::fs::create_dir_all(tmp.join("claude-1000/proj/sess")).unwrap();
    let blob = tmp.join("claude-1000/proj/sess/blob");
    std::fs::write(&blob, vec![0u8; 300]).unwrap();
    let settings = crate::clud_settings::settings_path_at(home.path());
    std::fs::write(&settings, br#"{"tmp":{"warn_bytes":100}}"#).unwrap();

    refresh_tmp_cache(home.path(), 100, NOW).unwrap();
    let entry = read_cache(&tmp_cache_path_for(home.path())).unwrap();
    assert_eq!(entry.checked_unix, NOW);
    assert!(matches!(entry.check, SizeCheck::Over(b) if b > 100));
    // Warn-only: the walk deleted nothing.
    assert!(blob.exists());

    let line = tmp_launch_warning_at(home.path(), NOW).expect("over cache warns");
    assert!(line.contains("tmp.warn_bytes"), "{line}");
    // The worktrees banner is independent: no worktrees cache, no worktrees line.
    assert_eq!(launch_warning_at(home.path(), NOW), None);

    // A stale cache stays quiet.
    assert_eq!(
        tmp_launch_warning_at(home.path(), NOW + MAX_CACHE_AGE_SECS + 1),
        None
    );

    // 0 disables the banner and refresh removes the old cache.
    std::fs::write(&settings, br#"{"tmp":{"warn_bytes":0}}"#).unwrap();
    assert_eq!(tmp_launch_warning_at(home.path(), NOW), None);
    refresh_tmp_cache(home.path(), 0, NOW).unwrap();
    assert!(!tmp_cache_path_for(home.path()).exists());
    assert!(blob.exists());
}

// ---- #1691: the same warn-only cached size check for `~/.clud/cache` ----

#[test]
fn clud_cache_size_file_lives_beside_cache_not_inside_it() {
    let home = TemporaryDirectory::new().unwrap();
    let root = clud_cache_dir_for(home.path());
    assert_eq!(root, home.path().join(".clud").join("cache"));
    let path = clud_cache_size_path_for(home.path());
    assert_eq!(path.parent(), root.parent(), "sibling of ~/.clud/cache");
    assert!(!path.starts_with(&root), "uv must never see it");
    assert_ne!(path, tmp_cache_path_for(home.path()));
}

#[test]
fn clud_cache_banner_and_list_lines_name_cache_setting() {
    let root = Path::new("/h/.clud/cache");
    let line = clud_cache_banner_line(root, 30 * GIB, 20 * GIB);
    assert!(!line.contains('\n'), "{line}");
    assert!(line.contains("cache.warn_bytes"), "{line}");
    assert!(!line.contains("tmp.warn_bytes"), "{line}");
    assert_eq!(clud_cache_list_warning(root, 10, SizeCheck::Under(5)), None);
    assert_eq!(clud_cache_list_warning(root, 0, SizeCheck::Over(50)), None);
    let over = clud_cache_list_warning(root, 10, SizeCheck::Over(50)).unwrap();
    assert!(over.contains("cache.warn_bytes"), "{over}");
    assert!(
        over.contains("uv cache prune"),
        "points at upstream: {over}"
    );
    let unknown = clud_cache_list_warning(root, 10, SizeCheck::Unknown).unwrap();
    assert!(unknown.contains("cache.warn_bytes"), "{unknown}");
}

#[test]
fn clud_cache_refresh_and_launch_warning_round_trip_and_never_delete() {
    let home = TemporaryDirectory::new().unwrap();
    assert_eq!(clud_cache_launch_warning_at(home.path(), NOW), None);
    let root = clud_cache_dir_for(home.path());
    std::fs::create_dir_all(root.join("uv/archive-v0/x")).unwrap();
    let blob = root.join("uv/archive-v0/x/blob");
    std::fs::write(&blob, vec![0u8; 300]).unwrap();
    let settings = crate::clud_settings::settings_path_at(home.path());
    std::fs::write(&settings, br#"{"cache":{"warn_bytes":100}}"#).unwrap();

    refresh_clud_cache_size(home.path(), 100, NOW).unwrap();
    let entry = read_cache(&clud_cache_size_path_for(home.path())).unwrap();
    assert!(matches!(entry.check, SizeCheck::Over(b) if b > 100));
    assert!(blob.exists(), "warn-only: the walk deletes nothing");

    let line = clud_cache_launch_warning_at(home.path(), NOW).expect("over cache warns");
    assert!(line.contains("cache.warn_bytes"), "{line}");
    // Independent of the tmp banner.
    assert_eq!(tmp_launch_warning_at(home.path(), NOW), None);
    assert_eq!(
        clud_cache_launch_warning_at(home.path(), NOW + MAX_CACHE_AGE_SECS + 1),
        None
    );

    std::fs::write(&settings, br#"{"cache":{"warn_bytes":0}}"#).unwrap();
    assert_eq!(clud_cache_launch_warning_at(home.path(), NOW), None);
    refresh_clud_cache_size(home.path(), 0, NOW).unwrap();
    assert!(!clud_cache_size_path_for(home.path()).exists());
    assert!(blob.exists());
}

/// #2101 item 3: an over-limit refresh names the largest worktrees in the
/// banner; dropping back under the limit clears the list.
#[test]
fn over_limit_refresh_names_the_largest_worktrees() {
    let home = TemporaryDirectory::new().unwrap();
    let root = crate::gc::worktree_root::worktree_root_for(home.path());
    std::fs::create_dir_all(root.join("repo-wt-big")).unwrap();
    std::fs::write(root.join("repo-wt-big").join("blob"), vec![0u8; 4096]).unwrap();
    let settings = crate::clud_settings::settings_path_at(home.path());
    std::fs::write(&settings, br#"{"worktrees":{"warn_bytes":1024}}"#).unwrap();
    refresh_cache(&root, 1024, NOW).unwrap();
    let line = launch_warning_at(home.path(), NOW).expect("over limit warns");
    assert!(line.contains("; largest: repo-wt-big ("), "{line}");
    refresh_cache(&root, 1 << 40, NOW).unwrap();
    assert!(crate::gc::worktree_top::read_for(&root, NOW, 60).is_empty());
}

#[test]
fn over_is_flagged_as_a_lower_bound_and_old_files_still_read() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = dir.path().join("worktrees-size.json");
    for (check, lower) in [
        (SizeCheck::Over(60 * GIB), true),
        (SizeCheck::Under(GIB), false),
        (SizeCheck::Unknown, false),
    ] {
        write_cache(&path, &cached(check, 0)).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["bytes_is_lower_bound"], lower, "{check:?}");
        assert_eq!(read_cache(&path).unwrap().check, check);
    }
    let old = r#"{"checked_unix":1,"warn_bytes":5,"state":"over","bytes":9}"#;
    std::fs::write(&path, old).unwrap();
    assert_eq!(read_cache(&path).unwrap().check, SizeCheck::Over(9));
}
