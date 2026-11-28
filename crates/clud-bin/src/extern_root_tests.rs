//! Where foreign checkouts live, and who may claim the directory (#986).

use super::*;
use kernal_api::platform::fs::TemporaryDirectory;
use std::fs;

fn repo(base: &Path, name: &str) -> PathBuf {
    let path = base.join(name);
    fs::create_dir_all(path.join(".git")).unwrap();
    path
}

#[test]
fn checkouts_belong_beside_the_repo_not_inside_it() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");

    let sibling = sibling_for(&root).expect("derivable");

    assert_eq!(sibling, tmp.path().join("myrepo-extern"));
    assert!(
        !sibling.starts_with(&root),
        "the whole point is that it is outside the repo tree"
    );
}

#[test]
fn a_repo_with_no_parent_has_no_sibling_to_derive() {
    // A repo at a filesystem root cannot have one placed beside it. Callers
    // have to handle this rather than be handed a wrong path.
    let root = if crate::platform::host::WINDOWS {
        PathBuf::from("C:\\")
    } else {
        PathBuf::from("/")
    };

    assert_eq!(sibling_for(&root), None);
}

#[test]
fn the_legacy_location_is_still_known() {
    // Existing checkouts have to keep working while users move them.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");

    assert_eq!(legacy_for(&root), root.join(".extern-repos"));

    let known = known_roots(&root);
    assert_eq!(known[0], tmp.path().join("myrepo-extern"), "sibling first");
    assert!(known.contains(&root.join(".extern-repos")));
}

#[test]
fn a_rootless_repo_still_reports_the_legacy_location() {
    // Losing the guard entirely because a sibling cannot be derived would
    // make clones *more* permissive than before, not less.
    let root = if crate::platform::host::WINDOWS {
        PathBuf::from("C:\\")
    } else {
        PathBuf::from("/")
    };

    let known = known_roots(&root);

    assert_eq!(known, vec![root.join(".extern-repos")]);
}

// -----------------------------------------------------------------
// Containment.
// -----------------------------------------------------------------

#[test]
fn containment_matches_either_known_location() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let roots = known_roots(&root);

    assert!(is_within_any(
        &tmp.path().join("myrepo-extern").join("dep"),
        &roots
    ));
    assert!(is_within_any(
        &root.join(".extern-repos").join("dep"),
        &roots
    ));
    assert!(!is_within_any(&root.join("src").join("lib.rs"), &roots));
    assert!(!is_within_any(&tmp.path().join("elsewhere"), &roots));
}

#[test]
fn a_sibling_whose_name_merely_starts_the_same_is_not_contained() {
    // `myrepo-extern` and `myrepo-external` are different directories; a
    // prefix comparison without the separator would conflate them.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let roots = known_roots(&root);

    assert!(!is_within_any(
        &tmp.path().join("myrepo-external").join("dep"),
        &roots
    ));
}

// -----------------------------------------------------------------
// Claiming.
// -----------------------------------------------------------------

#[test]
fn an_absent_or_empty_directory_is_free_to_claim() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let sibling = sibling_for(&root).unwrap();

    assert_eq!(claim_state(&sibling, &root), ClaimState::Available);

    fs::create_dir_all(&sibling).unwrap();
    assert_eq!(claim_state(&sibling, &root), ClaimState::Available);
    assert!(claim_state(&sibling, &root).usable());
}

#[test]
fn a_directory_holding_someone_elses_work_is_refused() {
    // The name is derived from the repo's own; guessing wrong must not
    // scatter clones through an unrelated project.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let sibling = sibling_for(&root).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    fs::write(sibling.join("their_notes.md"), "mine").unwrap();

    let state = claim_state(&sibling, &root);

    assert_eq!(state, ClaimState::OccupiedUnclaimed);
    assert!(!state.usable());
}

#[test]
fn a_directory_this_repo_already_claimed_is_ours() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let sibling = sibling_for(&root).unwrap();

    claim(&sibling, &root).unwrap();

    assert_eq!(claim_state(&sibling, &root), ClaimState::OursAlready);
    assert!(claim_state(&sibling, &root).usable());
}

#[test]
fn a_directory_another_repo_claimed_is_refused_and_names_the_owner() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let other = repo(tmp.path(), "otherrepo");
    let sibling = sibling_for(&root).unwrap();

    claim(&sibling, &other).unwrap();

    match claim_state(&sibling, &root) {
        ClaimState::ClaimedByOther { owner } => {
            assert!(owner.contains("otherrepo"), "{owner}");
        }
        other => panic!("expected a foreign claim, got {other:?}"),
    }
    assert!(!claim_state(&sibling, &root).usable());
}

#[test]
fn claiming_is_idempotent() {
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let sibling = sibling_for(&root).unwrap();

    claim(&sibling, &root).unwrap();
    let first = fs::read_to_string(sibling.join(CLAIM_FILE)).unwrap();
    claim(&sibling, &root).unwrap();
    let second = fs::read_to_string(sibling.join(CLAIM_FILE)).unwrap();

    assert_eq!(first, second);
    assert_eq!(claim_state(&sibling, &root), ClaimState::OursAlready);
}

#[test]
fn a_claim_marker_clud_cannot_read_is_treated_as_ours() {
    // Refusing on a corrupt marker would strand the repo with nowhere to put
    // checkouts, and the directory is one clud created either way.
    let tmp = TemporaryDirectory::new().unwrap();
    let root = repo(tmp.path(), "myrepo");
    let sibling = sibling_for(&root).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    fs::write(sibling.join(CLAIM_FILE), "{not json").unwrap();

    assert_eq!(claim_state(&sibling, &root), ClaimState::OursAlready);
}
