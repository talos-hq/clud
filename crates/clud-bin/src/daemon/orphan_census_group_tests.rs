use super::*;
use crate::orphan_reaper::{reap_orphans_from_scan, ReapOpts};
use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
use std::time::{Duration, Instant};

#[test]
fn sweep_reaps_group_peers_after_launcher_exits() {
    if crate::platform::process_identity::process_group(std::process::id()).is_none() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let pids_path = tmp.path().join("pids");
    let launcher = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            "sh".into(),
            "-c".into(),
            r#"unset RUNNING_PROCESS_IS_DAEMON CLUD_ORIGIN_START_TIME
               RUNNING_PROCESS_ORIGINATOR="CLUD:$$" sleep 30 & tagged=$!
               unset RUNNING_PROCESS_ORIGINATOR
               sleep 30 & peer=$!
               printf '%s %s' "$tagged" "$peer" > "$1""#
                .into(),
            "sh".into(),
            pids_path.to_string_lossy().into_owned(),
        ]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: true,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    launcher.start().unwrap();
    let launcher_pid = launcher.pid().unwrap();
    launcher.wait(Some(Duration::from_secs(10))).unwrap();

    let children: Vec<u32> = std::fs::read_to_string(pids_path)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();
    assert_eq!(children.len(), 2);
    super::super::io_helpers::write_json_file(
        &super::super::paths::session_snapshot_path(tmp.path(), "stale"),
        &serde_json::json!({
            "id": "stale", "kind": "subprocess", "daemon_pid": 0,
            "worker_pid": launcher_pid, "worker_port": 0, "root_pid": launcher_pid,
        }),
    )
    .unwrap();
    let mut scan = scan_env("CLUD");
    assert!(scan.orphaned_children.contains(&children[0]));
    assert!(scan.tagged.iter().all(|child| child.pid != children[1]));
    let mut handover = Default::default();
    expand_stale_groups(tmp.path(), &mut scan, &mut handover, || Ok(Vec::new()));
    let identities: Vec<_> = children.iter().map(|pid| scan.live[pid]).collect();
    let (outcome, _) = reap_orphans_from_scan(
        scan,
        &ReapOpts {
            keep: false,
            quiet: true,
            explain: false,
        },
        &handover,
        &mut |pid| children.contains(&pid),
    );
    assert_eq!(outcome.reaped_pids.len(), 2, "{:?}", outcome.spared);
    let deadline = Instant::now() + Duration::from_secs(3);
    while identities.iter().any(ProcessIdentity::is_live) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(identities.iter().all(|child| !child.is_live()));
}
