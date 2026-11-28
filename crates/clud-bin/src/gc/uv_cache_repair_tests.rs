use super::*;
use kernal_api::platform::fs::TemporaryDirectory;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// An unpacked wheel with `pkg/{__init__,main}.py` and a `RECORD` that
/// lists every file.
fn healthy_archive(dir: &Path) -> PathBuf {
    let files = [
        "pkg/__init__.py",
        "pkg/main.py",
        "pkg-1.0.dist-info/METADATA",
    ];
    let mut record = String::new();
    for file in files {
        write(&dir.join(file), "x");
        record.push_str(&format!("{file},sha256=abc,1\n"));
    }
    record.push_str("pkg-1.0.dist-info/RECORD,,\n");
    write(&dir.join("pkg-1.0.dist-info/RECORD"), &record);
    dir.to_path_buf()
}

#[test]
fn record_path_reads_the_path_column() {
    let cases: &[(&str, Option<&str>)] = &[
        ("pkg/a.py,sha256=x,12", Some("pkg/a.py")),
        (
            "pkg-1.0.dist-info/RECORD,,",
            Some("pkg-1.0.dist-info/RECORD"),
        ),
        ("pkg/a.py,sha256=x,12\r", Some("pkg/a.py")),
        // Hash and size never contain a comma; the path may.
        ("pkg/a,b.py,sha256=x,3", Some("pkg/a,b.py")),
        ("\"pkg/a,b.py\",sha256=x,3", Some("pkg/a,b.py")),
        ("\"pkg/q\"\"t.py\",,", Some("pkg/q\"t.py")),
        ("\"unterminated", None),
        ("no-commas", None),
        ("", None),
    ];
    for (line, want) in cases {
        assert_eq!(record_path(line).as_deref(), *want, "{line:?}");
    }
}

#[test]
fn archive_defect_flags_only_damage_that_breaks_an_install() {
    let tmp = TemporaryDirectory::new().unwrap();

    let healthy = healthy_archive(&tmp.path().join("healthy"));
    assert_eq!(archive_defect(&healthy), None);

    let no_dist_info = healthy_archive(&tmp.path().join("no-dist-info"));
    fs::remove_dir_all(no_dist_info.join("pkg-1.0.dist-info")).unwrap();
    assert_eq!(
        archive_defect(&no_dist_info).as_deref(),
        Some("missing .dist-info directory")
    );

    let no_record = healthy_archive(&tmp.path().join("no-record"));
    fs::remove_file(no_record.join("pkg-1.0.dist-info/RECORD")).unwrap();
    assert_eq!(
        archive_defect(&no_record).as_deref(),
        Some("missing pkg-1.0.dist-info/RECORD")
    );

    let missing_file = healthy_archive(&tmp.path().join("missing-file"));
    fs::remove_file(missing_file.join("pkg/main.py")).unwrap();
    assert_eq!(
        archive_defect(&missing_file).as_deref(),
        Some("missing pkg/main.py")
    );

    // Paths that cannot live inside the archive are not ours to judge.
    let odd_paths = healthy_archive(&tmp.path().join("odd-paths"));
    let record = odd_paths.join("pkg-1.0.dist-info/RECORD");
    let mut contents = fs::read_to_string(&record).unwrap();
    contents.push_str("../../bin/tool,,\n/abs/path,,\n./dot,,\n");
    fs::write(&record, contents).unwrap();
    assert_eq!(archive_defect(&odd_paths), None);

    // Several .dist-info dirs is uv's own error, not cache damage.
    let two = healthy_archive(&tmp.path().join("two-dist-infos"));
    fs::create_dir_all(two.join("other-2.0.dist-info")).unwrap();
    assert_eq!(archive_defect(&two), None);

    assert_eq!(archive_defect(&tmp.path().join("absent")), None);
}

mod unix {
    use super::*;
    use crate::file_lock::FileExt;
    use crate::gc::uv_cache_fixture::{link_exists, wheel, wheel_with_pointer, Entry};

    fn invalidated(entry: &Entry) -> bool {
        !entry.pointer.exists() && !link_exists(entry)
    }

    fn intact(entry: &Entry) -> bool {
        entry.pointer.exists() && link_exists(entry)
    }

    #[test]
    fn repair_invalidates_only_corrupt_pointers_and_keeps_archives() {
        if !crate::platform::host::UNIX {
            return;
        }
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path().join("uv");
        let no_dist_info = wheel(&root, "pydantic", "2.13.4", "UZfKIsCOzJxPSTiX");
        let no_record = wheel(&root, "boto3", "1.40.72", "QyCXRcDq7Uvf5m2G");
        let missing_file = wheel(&root, "pyjwt", "2.14.0", "jwtjwtjwtjwtjwt0");
        let healthy = wheel(&root, "idna", "3.18", "12FEHyBt4cSdD5YF");
        fs::remove_dir_all(&no_dist_info.dist_info).unwrap();
        fs::remove_file(no_record.dist_info.join("RECORD")).unwrap();
        fs::remove_file(missing_file.archive.join("pyjwt/main.py")).unwrap();

        let report = repair_corrupt_wheels_at(&root, false);

        assert_eq!(
            report,
            RepairReport {
                corrupt_found: 3,
                repaired: 3,
                skipped: 0,
                dry_run: false,
            }
        );
        for entry in [&no_dist_info, &no_record, &missing_file] {
            assert!(invalidated(entry), "{}", entry.link.display());
            assert!(entry.archive.exists(), "archives are never deleted");
            assert!(entry.lock.exists(), "uv's entry lock file stays");
        }
        assert!(no_dist_info.archive.join("pydantic/__init__.py").exists());
        assert!(intact(&healthy) && healthy.dist_info.join("RECORD").exists());
        assert_eq!(repair_corrupt_wheels_at(&root, false).corrupt_found, 0);
    }

    #[test]
    fn dry_run_reports_without_touching_anything() {
        if !crate::platform::host::UNIX {
            return;
        }
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path().join("uv");
        let bad = wheel(&root, "click", "8.4.2", "rSjdBjdBMaYzOqLS");
        fs::remove_dir_all(&bad.dist_info).unwrap();

        let report = repair_corrupt_wheels_at(&root, true);

        assert_eq!((report.corrupt_found, report.repaired), (1, 1));
        assert!(report.dry_run && intact(&bad));
    }

    #[test]
    fn an_entry_uv_is_filling_is_skipped() {
        if !crate::platform::host::UNIX {
            return;
        }
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path().join("uv");
        let bad = wheel(&root, "idna", "3.15", "EFeC21fNvyaO7F5i");
        fs::remove_dir_all(&bad.dist_info).unwrap();
        // uv holds `<key>.lock` exclusively while it refetches the entry.
        let uv_lock = fs::File::open(&bad.lock).unwrap();
        assert!(FileExt::try_lock_exclusive(&uv_lock).unwrap());

        let report = repair_corrupt_wheels_at(&root, false);

        assert_eq!((report.corrupt_found, report.skipped), (1, 1));
        assert_eq!(report.repaired, 0);
        assert!(intact(&bad));
        drop(uv_lock);
        // flock belongs to the open file description, and a sibling test
        // forking a child at this instant holds a duplicate of our fd until
        // that child execs. The lock is then still held for a moment after
        // the drop, so poll briefly instead of asserting on one pass.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while repair_corrupt_wheels_at(&root, false).repaired == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "entry lock never released"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(invalidated(&bad));
    }

    #[test]
    fn a_running_uv_cache_clean_skips_the_whole_pass() {
        if !crate::platform::host::UNIX {
            return;
        }
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path().join("uv");
        let bad = wheel(&root, "cbor2", "6.1.4", "YhCxU60UQ8nYjHbp");
        fs::remove_dir_all(&bad.dist_info).unwrap();
        write(&root.join(".lock"), "");
        // `uv cache clean`/`prune` hold the cache-wide lock exclusively.
        let clean = fs::File::open(root.join(".lock")).unwrap();
        assert!(FileExt::try_lock_exclusive(&clean).unwrap());

        let report = repair_corrupt_wheels_at(&root, false);

        assert_eq!((report.corrupt_found, report.skipped), (1, 1));
        assert!(intact(&bad));
    }

    #[test]
    fn a_pointer_uv_republished_after_the_scan_is_left_alone() {
        if !crate::platform::host::UNIX {
            return;
        }
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path().join("uv");
        let bad = wheel(&root, "pyopenssl", "26.4.0", "Z7k5ZDvjpxvvt4zW");
        fs::remove_dir_all(&bad.dist_info).unwrap();
        let pointer = wheel_pointers(&root).pop().unwrap();
        assert_eq!(pointer.link, bad.link);
        // uv refetches into a fresh archive and swaps the link.
        let fresh = wheel(&root, "fresh", "1.0", "FreshArchive0000");
        fs::remove_file(&bad.link).unwrap();
        crate::platform::gc_test_fs::symlink(&fresh.archive, &bad.link).unwrap();

        assert!(!invalidate(&root, &pointer, "missing .dist-info directory"));
        assert!(intact(&bad));
    }

    #[test]
    fn stale_foreign_and_pointerless_links_are_ignored() {
        if !crate::platform::host::UNIX {
            return;
        }
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path().join("uv");
        // Archive gone entirely: uv ignores the pointer and refetches itself.
        let stale = wheel(&root, "anyio", "4.13.0", "L_il0CL7Yel2jsh7");
        fs::remove_dir_all(&stale.archive).unwrap();
        // Link that escapes archive-v0.
        let foreign = wheel(&root, "uvicorn", "0.52.3", "aRqybf-3Og94JN9G");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(outside.join("uvicorn")).unwrap();
        fs::remove_file(&foreign.link).unwrap();
        crate::platform::gc_test_fs::symlink(&outside, &foreign.link).unwrap();
        // A link with no `.http`/`.rev` is not a pointer uv reads.
        let bare = wheel(&root, "watchfiles", "1.2.0", "xVpxJrU7Vcs6hlER");
        fs::remove_dir_all(&bare.dist_info).unwrap();
        fs::remove_file(&bare.pointer).unwrap();

        let report = repair_corrupt_wheels_at(&root, false);

        assert_eq!(report, RepairReport::default());
        assert!(intact(&stale) && intact(&foreign) && link_exists(&bare));
        assert!(outside.join("uvicorn").exists());
    }

    #[test]
    fn a_local_rev_pointer_is_repaired_too() {
        if !crate::platform::host::UNIX {
            return;
        }
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path().join("uv");
        let bad = wheel_with_pointer(&root, "fastled", "2.0.20", "OeSzUNns2fMjF5oz", "rev");
        fs::remove_dir_all(&bad.dist_info).unwrap();

        assert_eq!(repair_corrupt_wheels_at(&root, false).repaired, 1);
        assert!(invalidated(&bad));
    }

    #[test]
    fn each_invalidation_is_audited_before_it_happens() {
        if !crate::platform::host::UNIX {
            return;
        }
        use crate::gc::delete_audit::{StateDirGuard, AUDIT_LOG_FILE};
        let state = TemporaryDirectory::new().unwrap();
        let _guard = StateDirGuard::set(state.path());
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path().join("uv");
        let bad = wheel(&root, "websockets", "17.0.1", "p9D_Iim7736KhyVf");
        fs::remove_dir_all(&bad.dist_info).unwrap();

        assert_eq!(repair_corrupt_wheels_at(&root, false).repaired, 1);

        // One line per removed path: the `.http` pointer, then the link.
        let log = fs::read_to_string(state.path().join(AUDIT_LOG_FILE)).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        for (line, path) in lines.iter().zip([&bad.pointer, &bad.link]) {
            assert!(line.contains("\"site\":\"gc.uv-cache-repair\""), "{line}");
            assert!(line.contains(REPAIR_RULE), "{line}");
            assert!(line.contains("missing .dist-info directory"), "{line}");
            assert!(line.contains(&*path.to_string_lossy()), "{line}");
        }
    }
}
