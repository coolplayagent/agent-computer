#!/usr/bin/env python3
"""External watchdog against the real gVisor PID 1 STOP fault, disposable VM only."""
import argparse
import hashlib
import json
import os
import pathlib
import select
import subprocess
import sys
import time
import uuid


def wait_for(condition, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if condition():
            return
        time.sleep(0.02)
    raise AssertionError("condition timed out")


def frame(pipe):
    assert select.select([pipe], [], [], 10)[0]
    return json.loads(pipe.readline())


def main():
    parser = argparse.ArgumentParser()
    for argument in ["watchdog", "runsc", "rootfs", "work-dir"]:
        parser.add_argument("--" + argument, required=True, type=pathlib.Path)
    args = parser.parse_args()
    assert os.geteuid() == 0
    args.work_dir.mkdir(parents=True, exist_ok=False)
    case = args.work_dir.resolve()
    workspace = case / "workspace"
    workspace.mkdir(mode=0o700)
    os.chown(workspace, 1000, 1000)
    group = pathlib.Path("/sys/fs/cgroup") / ("ac-watchdog-runsc-" + uuid.uuid4().hex)
    group.mkdir()
    request = {"execution_id": "pid1-stop", "generation": 7,
               "argv": ["/bin/sh", "-c", "echo started > /workspace/started; kill -STOP 1; /bin/sleep 1; echo alive > /workspace/after-local-deadline; /bin/sleep 60"],
               "cwd": "", "timeout_seconds": 1, "lease_budget_ms": 200,
               "term_grace_ms": 100, "output_limit_bytes": 4096}
    (case / "request.json").write_text(json.dumps(request))
    config = {
        "ociVersion": "1.0.2", "hostname": "watchdog-component",
        "root": {"path": str(args.rootfs.resolve()), "readonly": True},
        "process": {"terminal": False, "user": {"uid": 1000, "gid": 1000},
                    "args": ["/bin/agent-computer-sandbox", "--request", "/request.json"],
                    "env": [], "cwd": "/", "noNewPrivileges": True,
                    "capabilities": {key: [] for key in ["bounding", "effective", "inheritable", "permitted", "ambient"]}},
        "mounts": [
            {"destination": "/proc", "type": "proc", "source": "proc", "options": ["nosuid", "noexec", "nodev"]},
            {"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "mode=755", "size=1m"]},
            {"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=1777", "size=64m"]},
            {"destination": "/workspace", "type": "bind", "source": str(workspace), "options": ["rbind", "rw", "nosuid", "nodev"]},
            {"destination": "/request.json", "type": "bind", "source": str(case / "request.json"), "options": ["bind", "ro", "nosuid", "nodev"]}],
        "linux": {"namespaces": [{"type": name} for name in ["pid", "network", "ipc", "uts", "mount"]]},
    }
    (case / "config.json").write_text(json.dumps(config))
    runtime = [str(args.runsc.resolve()), "--root=" + str(case / "state"), "--platform=systrap",
               "--network=none", "--ignore-cgroups=true", "--directfs=false"]
    container = "ac-watchdog-" + uuid.uuid4().hex
    guard = None
    process = None
    try:
        with (case / "stdout.json").open("wb") as stdout, (case / "stderr.log").open("wb") as stderr:
            process = subprocess.Popen([sys.executable, __file__, "--exec-in-group", str(group),
                                        *runtime, "run", "--bundle=" + str(case), container], stdout=stdout, stderr=stderr)
            wait_for(lambda: (workspace / "after-local-deadline").exists())
            assert process.poll() is None
            assert (case / "stdout.json").stat().st_size == 0
            members = []
            for pid in (group / "cgroup.procs").read_text().splitlines():
                command = pathlib.Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                members.append({"pid": int(pid), "argv": [part.decode() for part in command if part]})
                assert pathlib.Path(f"/proc/{pid}/cgroup").read_text().strip() == "0::/" + group.name
            assert any("boot" in member["argv"] for member in members), members
            assert any("gofer" in member["argv"] for member in members), members
            value = {"version": 1, "execution_id": "pid1-stop",
                     "boot_id": pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                     "cgroup_path": group.name, "cgroup_inode": group.stat().st_ino,
                     "deadline_boottime_ms": time.clock_gettime_ns(time.CLOCK_BOOTTIME) // 1_000_000 + 1000}
            (case / "guard.json").write_text(json.dumps(value))
            guard = subprocess.Popen([str(args.watchdog.resolve()), "--request", str(case / "guard.json")],
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True, bufsize=0)
            ack, report = frame(guard.stdout), frame(guard.stdout)
            assert guard.wait(timeout=2) == 0
            assert ack["request"] == value and report["request"] == value
            assert report["observation"] == "EmptyObserved" and report["trigger"] == "Deadline", report
            assert "populated 0\n" in (group / "cgroup.events").read_text()
            assert process.wait(timeout=2) == -9
            assert (case / "stdout.json").stat().st_size == 0
            record = {"case": "gvisor-pid1-stop", "status": "pass", "report": report,
                      "kernel": os.uname().release, "runtime_members_before_kill": members,
                      "local_lease_budget_ms": 200, "work_after_local_deadline_observed": True,
                      "supervisor_report_absent": True, "runtime_host_exit_code": -9,
                      "watchdog_sha256": hashlib.sha256(args.watchdog.read_bytes()).hexdigest(),
                      "runsc_sha256": hashlib.sha256(args.runsc.read_bytes()).hexdigest(),
                      "runsc_version": subprocess.check_output([str(args.runsc), "--version"], text=True).strip(),
                      "test_sha256": hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(),
                      "rootfs_files": json.loads((args.rootfs / "fixture-files.json").read_text()),
                      "candidate_mount": False, "kubernetes_binding": False, "durable_fence": False}
            (case / "result.json").write_text(json.dumps(record, indent=2) + "\n")
            print(json.dumps(record), flush=True)
    finally:
        (group / "cgroup.kill").write_text("1")
        wait_for(lambda: "populated 0\n" in (group / "cgroup.events").read_text())
        if process is not None:
            process.wait(timeout=5)
        if guard is not None:
            guard.wait(timeout=8)
        subprocess.run(runtime + ["delete", "--force", container], check=True, capture_output=True, timeout=10)
        group.rmdir()


if __name__ == "__main__":
    if sys.argv[1] == "--exec-in-group":
        (pathlib.Path(sys.argv[2]) / "cgroup.procs").write_text(str(os.getpid()))
        os.execv(sys.argv[3], sys.argv[3:])
    else:
        main()
