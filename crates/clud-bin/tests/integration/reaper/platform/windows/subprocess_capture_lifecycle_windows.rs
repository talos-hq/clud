//! Regression coverage for #634's post-spawn Job-assignment race.
//!
//! The fixture parent deliberately uses raw `std::process::Command`: it must
//! create a descendant immediately and without adding its own containment, or
//! the test would mask the exact gap it exists to exercise. The production
//! launch goes through `subprocess::ManagedSubprocess`.

use std::time::{Duration, Instant};

use clud::subprocess::ManagedSubprocess;

/// libtest matches `--exact` against the **module-qualified** test id.
///
/// #1056 consolidated the former top-level `tests/*.rs` files into modules of
/// one target, which changed every id from `<fn>` to `<module>::<fn>` (this
/// target's own `main.rs` says so). These fixtures re-invoke the test binary
/// and kept the pre-consolidation bare names, so `--exact` matched nothing,
/// the child ran zero tests, and the pid file it was supposed to write never
/// appeared -- surfacing 30s later as `NotFound` on the *parent* side, which
/// reads like a spawn or containment bug rather than a name that stopped
/// resolving.
///
/// Built from `module_path!()` rather than written out, so renaming this
/// module cannot leave a stale literal behind -- which is exactly how the
/// bare names survived #1056.
fn fixture_id(name: &str) -> String {
    format!(
        "{}::{name}",
        crate::reaper::fixture_ids::libtest_module_prefix(module_path!())
    )
}

#[test]
fn production_adapter_contains_an_immediate_descendant() {
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let pid_file = temp.path().join("descendant.pid");
    let exe = std::env::current_exe().expect("current test executable");
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.push(("CLUD_634_FIXTURE".into(), "parent".into()));
    env.push((
        "CLUD_634_PID_FILE".into(),
        pid_file.to_string_lossy().into_owned(),
    ));
    let process = ManagedSubprocess::start(
        vec![
            exe.to_string_lossy().into_owned(),
            "--ignored".into(),
            "--exact".into(),
            fixture_id("race_parent_fixture"),
        ],
        None,
        env,
        true,
        Some(clud::win_creation_flags::CREATE_NEW_PROCESS_GROUP),
    )
    .expect("spawn immediate parent through production adapter");
    // 30s: the fixture's Job-scoped descendant needs to spawn and write its
    // pid file, and a loaded shared runner (#994) routinely pushes that
    // past the old 5s deadline.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !pid_file.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("fixture descendant pid (did the 30s deadline expire first?)")
        .trim()
        .parse()
        .expect("numeric fixture pid");
    assert_ne!(
        clud::process_identity::start_time_of(pid),
        clud::process_identity::UNKNOWN_START_TIME,
        "the fixture descendant must initially be alive inside the Job"
    );

    // Mimic the stream-JSON renderer: capture readers time out while the
    // descendant holds inherited writers, then poll notices that the direct
    // root exited. That terminal observation must close the Job so the next
    // read reaches EOF rather than hanging until ManagedSubprocess is dropped.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match process.read_stdout(Some(Duration::from_millis(100))) {
            running_process::ReadStatus::Line(_) => {}
            running_process::ReadStatus::Eof => break,
            running_process::ReadStatus::Timeout if Instant::now() < deadline => {
                let _ = process.poll().expect("poll fixture root");
            }
            running_process::ReadStatus::Timeout => {
                panic!("captured handles did not reach EOF after direct-root exit")
            }
        }
    }
    assert_eq!(process.wait(Some(Duration::from_secs(5))), Ok(0));

    let deadline = Instant::now() + Duration::from_secs(5);
    while clud::process_identity::start_time_of(pid) != clud::process_identity::UNKNOWN_START_TIME
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        clud::process_identity::start_time_of(pid),
        clud::process_identity::UNKNOWN_START_TIME,
        "observing direct-root exit must close the Job and reap its descendant"
    );
}

#[test]
#[ignore = "compiled fixture invoked by production_adapter_contains_an_immediate_descendant"]
#[allow(clippy::zombie_processes)]
fn race_parent_fixture() {
    if std::env::var("CLUD_634_FIXTURE").as_deref() != Ok("parent") {
        return;
    }
    let exe = std::env::current_exe().expect("current test executable");
    // Deliberately do not wait: the fixture parent must exit while its child
    // still owns the inherited stdout/stderr handles. The production Job
    // owner in the outer test is responsible for reaping that descendant.
    // Bound first: an array literal is homogeneous, so mixing `&'static str`
    // with a `&String` temporary does not compile.
    let sleeper = fixture_id("race_sleeper_fixture");
    let child = std::process::Command::new(exe)
        .args(["--ignored", "--exact", sleeper.as_str()])
        .env("CLUD_634_FIXTURE", "sleeper")
        .spawn()
        .expect("spawn the pre-containment descendant fixture");
    std::fs::write(
        std::env::var_os("CLUD_634_PID_FILE").expect("pid-file env"),
        child.id().to_string(),
    )
    .expect("write descendant pid");
}

#[test]
#[ignore = "compiled fixture invoked by race_parent_fixture"]
fn race_sleeper_fixture() {
    if std::env::var("CLUD_634_FIXTURE").as_deref() == Ok("sleeper") {
        std::thread::sleep(Duration::from_secs(30));
    }
}
