use super::super::Roots;
use super::*;

pub(crate) fn setup() -> (
    kernal_api::platform::fs::TemporaryDirectory,
    PathBuf,
    PathBuf,
    PathBuf,
) {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let launch = tmp.path().join("launch");
    let sibling = tmp.path().join("sibling");
    let other = tmp.path().join("other");
    for root in [&launch, &sibling, &other] {
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("obsolete.rs"), "old").unwrap();
    }
    (tmp, launch, sibling, other)
}

#[test]
fn grant_is_scoped_audited_and_refuses_ungranted_sibling() {
    let (tmp, launch, sibling, other) = setup();
    let state = tmp.path().join("state");
    grant(
        &sibling,
        "approved migration cleanup",
        "session-a",
        std::slice::from_ref(&launch),
        &state,
    )
    .unwrap();
    assert!(load(&state, "session-b", std::slice::from_ref(&launch)).is_empty());
    let grants = load(&state, "session-a", std::slice::from_ref(&launch));
    assert_eq!(grants.len(), 1);
    let mut roots = Roots::fixed(vec![launch], true).with_session_grants(grants);
    let accepted = super::super::resolve(
        sibling.join("obsolete.rs").to_str().unwrap(),
        tmp.path(),
        None,
        &mut roots,
    )
    .unwrap();
    let super::super::Resolved::Present(target) = accepted else {
        panic!("expected existing file");
    };
    assert!(target
        .ledger
        .unwrap()
        .contains("approved migration cleanup"));
    assert!(super::super::resolve(
        other.join("obsolete.rs").to_str().unwrap(),
        tmp.path(),
        None,
        &mut roots,
    )
    .is_err());
    assert!(
        super::super::resolve(sibling.to_str().unwrap(), tmp.path(), None, &mut roots).is_err()
    );
}
