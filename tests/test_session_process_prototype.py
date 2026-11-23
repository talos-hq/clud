"""Incident-shaped regression fixtures for the Linux process inspector."""

from bench.session_processes import Process, match_session, read_stat


def test_matches_detached_gate_and_zombie_by_distinct_evidence():
    rows = [
        Process(2949262, 2128, 2949262, 20, "S", "bash", 629029),
        Process(2949610, 2949262, 2949262, 21, "S", "python", 629029),
        Process(2941000, 2079183, 2941000, 25, "T", "soldr", 2078717),
        Process(2945367, 2941000, 2941000, 26, "Z", "cargo"),
        Process(4039464, 2128, 4039464, 27, "S", "zccache-daemon"),
        Process(629029, 1, 1, 30, "S", "unrelated"),
        Process(100, 1, 1, 31, "S", "worker", 629029),
    ]
    processes = {row.pid: row for row in rows}
    gate = match_session(processes, 629029, 1)
    assert [row["pid"] for row in gate] == [2949262, 2949610]
    assert all(row["matched_by"] == "originator tag" for row in gate)
    build = match_session(processes, 2078717, 10)
    assert [row["pid"] for row in build] == [2941000, 2945367]
    assert build[1]["zombie"]
    assert build[1]["matched_by"] == "parent chain"


def test_stat_preserves_process_name_and_monotonic_identity(tmp_path):
    path = tmp_path / "stat"
    path.write_text("7 (cargo ) worker) Z 5 6 4 " + "0 " * 15 + "12345 0 0\n")
    assert read_stat(path) == Process(7, 5, 4, 12345, "Z", "cargo ) worker")
