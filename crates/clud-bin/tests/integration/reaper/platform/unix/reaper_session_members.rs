//! A live Unix session leader cannot protect ordinary tagged workers.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use clud::orphan_reaper;

use crate::reaper::reaper_orphan_sweep_survival::{
    a_dead_originator_pid, quiet, sweep_until_selected,
};

#[test]
fn exit_scan_reaps_a_tagged_worker_in_a_live_foreign_session() {
    let me = std::process::id();
    assert_tagged_session_worker_is_reaped(me, |_| orphan_reaper::scan_and_report(me, &quiet()));
}

#[test]
fn orphan_sweep_reaps_a_tagged_worker_in_a_live_foreign_session() {
    assert_tagged_session_worker_is_reaped(a_dead_originator_pid(), |worker| {
        orphan_reaper::reap_orphans_filtered(&quiet(), &mut |pid| pid == worker)
    });
}

fn assert_tagged_session_worker_is_reaped(
    originator: u32,
    mut sweep: impl FnMut(u32) -> orphan_reaper::ReapOutcome,
) {
    let _guard = crate::reaper::REAPER_TEST_LOCK
        .lock()
        .expect("reaper test lock poisoned");
    // Keep the session leader alive and tag only its worker. The shell stays
    // the leader because it waits for the background job.
    let mut leader = Command::new("setsid")
        .args([
            "sh",
            "-c",
            r#"RUNNING_PROCESS_ORIGINATOR="$1" sleep 30 & printf '%s\n' "$!"; wait "$!""#,
            "sh",
            &format!("CLUD:{originator}"),
        ])
        .env_remove(running_process::ORIGINATOR_ENV_VAR)
        .env_remove(running_process::DAEMON_MARKER_ENV_VAR)
        .stdout(Stdio::piped())
        .spawn()
        .expect("start a separate session");
    let mut line = String::new();
    BufReader::new(leader.stdout.take().expect("worker PID pipe"))
        .read_line(&mut line)
        .expect("read worker PID");
    let worker = line.trim().parse::<u32>().expect("parse worker PID");

    // SAFETY: getsid queries the kernel for a PID, without using pointers.
    let worker_session = unsafe { libc::getsid(worker as libc::pid_t) };
    let outcome = sweep_until_selected(worker, || sweep(worker));
    if !outcome.reaped_pids.contains(&worker) {
        // Only this test's process tree, even if the assertion below fails.
        clud::process_tree::kill_tree(worker);
    }
    let worker_status = leader.wait().expect("wait for session leader");

    assert!(
        outcome.reaped_pids.contains(&worker),
        "tagged worker was not reaped: {outcome:?}"
    );
    assert_eq!(worker_session as u32, leader.id());
    assert!(!worker_status.success(), "the worker exited normally");
}
