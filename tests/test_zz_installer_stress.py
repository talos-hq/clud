"""Throwaway: time repeated Windows installs; always fails to surface the data."""
import os, shutil, sys, time
from pathlib import Path
import pytest
from running_process.compat import TimeoutExpired
from tests.process import run as run_process
from tests.test_installer_native_entry import clud_binary

@pytest.mark.skipif(sys.platform != "win32", reason="windows")
def test_stress(tmp_path):
    out = []
    binary = clud_binary()
    out.append(f"size={binary.stat().st_size}")
    ps = run_process(["powershell", "-NoProfile", "-Command",
        "Get-MpComputerStatus | Select RealTimeProtectionEnabled,AntivirusEnabled | Format-List; Get-PSDrive C,D | Format-Table Name,Used,Free; Get-PhysicalDisk | Format-Table MediaType,Size"],
        capture_output=True, timeout=60, check=False)
    out.append(ps.stdout.decode(errors="replace"))
    dest = Path(os.environ["LOCALAPPDATA"]) / "Programs" / "clud"
    for i in range(30):
        for unlock in (True, False):
            env = os.environ.copy()
            trace = tmp_path / f"t{i}{unlock}.txt"
            env["CLUD_INSTALLER_TRACE"] = str(trace)
            if not unlock:
                env["CLUD_NO_UNLOCK"] = "1"
            t = time.monotonic()
            try:
                r = run_process([str(binary), "--installer", "--install-current", "--yes", "--no-modify-path"],
                    env=env, capture_output=True, timeout=120, check=False)
                rc = r.returncode
            except TimeoutExpired:
                rc = "TIMEOUT"
            el = time.monotonic() - t
            tr = trace.read_text() if trace.exists() else ""
            lines = [l.split(" ", 2) for l in tr.splitlines()]
            steps = " ".join(f"{p}+{int(m)-int(lines[0][0])}" for m, _, p in lines)
            out.append(f"{i} unlock={unlock} rc={rc} {el:.1f}s [{steps}]")
            if dest.exists():
                q = tmp_path / f"q{i}{unlock}"
                try:
                    dest.replace(q)
                except OSError as e:
                    out.append(f"move: {e}")
    pytest.fail("\n".join(out), pytrace=False)
