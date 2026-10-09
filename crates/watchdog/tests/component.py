#!/usr/bin/env python3
"""Destructive only to fresh owned cgroups; run as root in a disposable Linux VM.

Actual cgroup v2 kernel tests, separate from default Cargo/Bazel contracts.
This does not certify Kubernetes identity binding or Candidate writer fencing.
"""
import argparse
import hashlib
import json
import os
import pathlib
import select
import signal
import subprocess
import sys
import tempfile
import time
import uuid


def now_ms():
    return time.clock_gettime_ns(time.CLOCK_BOOTTIME) // 1_000_000


def wait_for(test, seconds=5):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if test():
            return
        time.sleep(0.01)
    raise AssertionError("condition timed out")


def empty(group):
    return "populated 0\n" in (group / "cgroup.events").read_text()


def frame(pipe):
    assert select.select([pipe], [], [], 8)[0], "missing watchdog frame"
    # Unbuffered pipe avoids read-ahead hiding the next frame from select.
    return json.loads(pipe.readline())


def child(group, marker):
    (group / "cgroup.procs").write_text(str(os.getpid()))
    descendant = os.fork()
    if descendant == 0:
        os.setsid()
        (group / "child" / "cgroup.procs").write_text(str(os.getpid()))
        marker.with_suffix(".child").write_text(str(os.getpid()))
        time.sleep(300)
        os._exit(0)
    wait_for(marker.with_suffix(".child").exists)
    marker.write_text(json.dumps({"parent": os.getpid(), "child": descendant}))
    os.kill(os.getpid(), signal.SIGSTOP)
    time.sleep(300)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--watchdog", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()
    assert os.geteuid() == 0
    binary = args.watchdog.resolve()
    boot_id = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    root = pathlib.Path("/sys/fs/cgroup") / ("ac-watchdog-" + uuid.uuid4().hex)
    root.mkdir()
    results = []
    processes = []
    guards = []
    try:
        with tempfile.TemporaryDirectory(prefix="ac-watchdog-") as temporary:
            work = pathlib.Path(temporary)

            def workload(name):
                group = root / name
                group.mkdir()
                (group / "child").mkdir()
                marker = work / (name + "-" + uuid.uuid4().hex)
                process = subprocess.Popen([sys.executable, __file__, "--child", str(group), str(marker)])
                processes.append(process)
                wait_for(marker.exists)
                pids = json.loads(marker.read_text())
                assert pathlib.Path(f"/proc/{pids['parent']}/status").read_text().split("State:", 1)[1].lstrip().startswith("T")
                assert not empty(group)
                return group, pids

            def request(group, deadline=1200):
                return {"version": 1, "execution_id": group.name, "boot_id": boot_id,
                        "cgroup_path": str(group.relative_to("/sys/fs/cgroup")),
                        "cgroup_inode": group.stat().st_ino,
                        "deadline_boottime_ms": now_ms() + deadline}

            def start(value, output=subprocess.PIPE, in_group=None):
                path = work / (uuid.uuid4().hex + ".json")
                path.write_text(json.dumps(value))
                command = [str(binary), "--request", str(path)]
                if in_group is not None:
                    command = [sys.executable, __file__, "--exec-in-group", str(in_group), *command]
                guard = subprocess.Popen(command, stdout=output, stderr=subprocess.PIPE, start_new_session=True, bufsize=0)
                guards.append(guard)
                return guard

            def passed(name, **details):
                value = {"case": name, "status": "pass", **details}
                results.append(value)
                print(json.dumps(value), flush=True)

            for name in ["stopped-supervisor", "frozen-tree", "expired-deadline"]:
                group, pids = workload(name)
                if name == "frozen-tree":
                    (group / "cgroup.freeze").write_text("1")
                    wait_for(lambda: "frozen 1\n" in (group / "cgroup.events").read_text())
                value = request(group, -1000 if name == "expired-deadline" else 1200)
                guard = start(value)
                ack, report = frame(guard.stdout), frame(guard.stdout)
                assert guard.wait(timeout=2) == 0, guard.stderr.read()
                assert ack["event"] == "armed" and ack["request"] == value
                assert report["request"] == value and report["observation"] == "EmptyObserved", report
                assert report["trigger"] == "Deadline" and report["error"] is None
                assert report["kill_boottime_ms"] >= value["deadline_boottime_ms"]
                assert report["kill_boottime_ms"] <= max(ack["armed_boottime_ms"], value["deadline_boottime_ms"]) + 1500
                assert empty(group)
                passed(name, pids=pids, report=report)

            for name in ["closed-receipt", "full-receipt"]:
                group, pids = workload(name)
                value = request(group, 25_000)
                read_fd, write_fd = os.pipe2(os.O_NONBLOCK)
                if name == "closed-receipt":
                    os.close(read_fd)
                else:
                    try:
                        while True:
                            os.write(write_fd, b"x" * 4096)
                    except BlockingIOError:
                        pass
                started = now_ms()
                guard = start(value, write_fd)
                os.close(write_fd)
                assert guard.wait(timeout=5) == 2
                assert empty(group) and now_ms() - started < 5000
                if name == "full-receipt":
                    os.close(read_fd)
                passed(name, pids=pids, elapsed_ms=now_ms() - started, deadline=value["deadline_boottime_ms"])

            # The controller exits after spawning the detached guard. Its exit
            # cannot renew or disarm the already fixed deadline.
            group, pids = workload("controller-exit")
            value = request(group)
            path = work / "controller-request.json"
            path.write_text(json.dumps(value))
            read_fd, write_fd = os.pipe()
            controller = subprocess.Popen([sys.executable, __file__, "--controller", str(binary), str(path)], stdout=write_fd)
            os.close(write_fd)
            assert controller.wait(timeout=2) == 0
            with os.fdopen(read_fd, "rb", buffering=0) as pipe:
                ack, report = frame(pipe), frame(pipe)
            assert ack["request"] == value and report["observation"] == "EmptyObserved"
            assert empty(group)
            passed("controller-exit", pids=pids, report=report)

            group, pids = workload("path-reuse")
            value = request(group, 1800)
            guard = start(value)
            assert frame(guard.stdout)["event"] == "armed"
            (group / "cgroup.kill").write_text("1")
            wait_for(lambda: empty(group))
            (group / "child").rmdir()
            group.rmdir()
            group, replacement = workload("path-reuse")
            assert group.stat().st_ino != value["cgroup_inode"]
            report = frame(guard.stdout)
            assert guard.wait(timeout=2) == 2
            assert report["observation"] == "Unknown" and report["error"] == "KillFailed", report
            assert not empty(group), "guard killed a different cgroup at the reused path"
            passed("path-reuse", original=pids, replacement=replacement, report=report)

            for name in ["wrong-inode", "wrong-boot", "own-ancestor", "delegated-group"]:
                group, pids = workload(name)
                value = request(group)
                if name == "wrong-inode":
                    value["cgroup_inode"] += 1
                elif name == "wrong-boot":
                    value["boot_id"] = "00000000-0000-0000-0000-000000000000"
                elif name == "delegated-group":
                    os.chown(group / "cgroup.procs", 1000, 1000)
                guard = start(value, in_group=group / "child" if name == "own-ancestor" else None)
                stdout, stderr = guard.communicate(timeout=3)
                assert guard.returncode == 2 and not stdout, (stdout, stderr)
                assert not empty(group), "rejected identity must not kill the target"
                passed(name, pids=pids)

            # Threaded topology must be established before populating it.
            domain = root / "threaded-group"
            domain.mkdir()
            group = domain / "thread"
            group.mkdir()
            (group / "cgroup.type").write_text("threaded")
            assert (group / "cgroup.type").read_text() == "threaded\n"
            guard = start(request(group))
            stdout, stderr = guard.communicate(timeout=3)
            assert guard.returncode == 2 and not stdout, (stdout, stderr)
            passed("threaded-group")

        record = {"schema_version": 1, "scope": "Linux cgroup component, not product acceptance",
                  "kernel": os.uname().release, "boot_id": boot_id,
                  "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                  "test_sha256": hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(),
                  "cases": results}
        args.output.write_text(json.dumps(record, indent=2) + "\n")
    finally:
        # Only this invocation's fresh owned subtree is ever removed.
        (root / "cgroup.kill").write_text("1")
        wait_for(lambda: empty(root))
        for process in processes + guards:
            process.wait(timeout=8)
        for directory in sorted(root.rglob("*"), key=lambda p: len(p.parts), reverse=True):
            if directory.is_dir():
                directory.rmdir()
        root.rmdir()


if __name__ == "__main__":
    if sys.argv[1] == "--child":
        child(pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3]))
    elif sys.argv[1] == "--exec-in-group":
        (pathlib.Path(sys.argv[2]) / "cgroup.procs").write_text(str(os.getpid()))
        os.execv(sys.argv[3], sys.argv[3:])
    elif sys.argv[1] == "--controller":
        subprocess.Popen([sys.argv[2], "--request", sys.argv[3]], start_new_session=True)
    else:
        main()
