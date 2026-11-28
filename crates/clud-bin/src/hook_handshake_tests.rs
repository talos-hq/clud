use super::*;

fn facts(own: &str, expected: &str) -> Facts {
    Facts {
        own: Some(own.into()),
        expected: Some(expected.into()),
        launcher: Some(PathBuf::from("/opt/clud/bin/clud")),
        launcher_exists: true,
        relayed: false,
    }
}

#[test]
fn a_matching_build_runs_here() {
    assert_eq!(decide(&facts("2.8.31+10", "2.8.31+10")), Decision::Run);
}

#[test]
fn a_skewed_build_relays_to_the_launcher() {
    assert_eq!(
        decide(&facts("2.8.27+9", "2.8.31+10")),
        Decision::Relay {
            launcher: PathBuf::from("/opt/clud/bin/clud"),
            own: "2.8.27+9".into(),
            expected: "2.8.31+10".into(),
        }
    );
}

#[test]
fn no_identity_a_missing_launcher_or_a_relayed_run_never_relays() {
    assert_eq!(decide(&Facts::default()), Decision::Run);
    let missing = Facts {
        launcher_exists: false,
        ..facts("a", "b")
    };
    assert_eq!(decide(&missing), Decision::Run);
    let relayed = Facts {
        relayed: true,
        ..facts("a", "b")
    };
    assert_eq!(decide(&relayed), Decision::Run, "a relay never loops");
}

#[test]
fn the_fingerprint_names_the_version_and_size() {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let exe = dir.path().join("clud");
    std::fs::write(&exe, b"12345").unwrap();
    assert_eq!(
        fingerprint(&exe),
        Some(format!("{}+5", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(fingerprint(&dir.path().join("missing")), None);
}
