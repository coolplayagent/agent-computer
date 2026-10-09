#!/usr/bin/env python3
"""Explicit real-runsc component test, run as root only inside a disposable VM.

Uses fresh private containers and workspaces; requires the rootfs.py fixture.
Does not mount an actual Candidate, publish execution results or certify fencing.
"""
import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import time
import uuid

p = argparse.ArgumentParser()
p.add_argument("--runsc", type=pathlib.Path, required=True)
p.add_argument("--rootfs", type=pathlib.Path, required=True)
p.add_argument("--work-dir", type=pathlib.Path, required=True)
a = p.parse_args()
assert os.geteuid() == 0, "requires a disposable root-controlled VM"
a.work_dir.mkdir(parents=True, exist_ok=False)
runtime = [str(a.runsc), "--root=" + str(a.work_dir / "state"), "--platform=systrap",
           "--network=none", "--ignore-cgroups=true", "--directfs=false"]
records = []


def execute(name, argv, *, outcome="succeeded", cancel=False, setup_failure=False, external_stop=False, **overrides):
    case = a.work_dir / name
    case.mkdir()
    workspace = case / "workspace"
    workspace.mkdir()
    workspace.chmod(0o700)
    os.chown(workspace, 1000, 1000)
    (workspace / "nested").mkdir()
    os.chown(workspace / "nested", 1000, 1000)
    (workspace / "escape").symlink_to("/tmp")
    request = dict(execution_id=name, generation=7, argv=argv, cwd="", timeout_seconds=10,
                   lease_budget_ms=15000, term_grace_ms=100, output_limit_bytes=4096)
    request.update(overrides)
    (case / "request.json").write_text(json.dumps(request))
    config = {
        "ociVersion": "1.0.2", "hostname": "sandbox-component",
        "root": {"path": str(a.rootfs), "readonly": True},
        "process": {"terminal": False, "user": {"uid": 1000, "gid": 1000},
                    "args": ["/bin/agent-computer-sandbox", "--request", "/request.json"],
                    "env": ["AC_HOST_SECRET=must-not-leak"], "cwd": "/", "noNewPrivileges": True,
                    "capabilities": {k: [] for k in ["bounding", "effective", "inheritable", "permitted", "ambient"]},
                    "rlimits": [{"type": "RLIMIT_NOFILE", "hard": 128, "soft": 128},
                                {"type": "RLIMIT_NPROC", "hard": 64, "soft": 64}]},
        "mounts": [
            {"destination": "/proc", "type": "proc", "source": "proc", "options": ["nosuid", "noexec", "nodev"]},
            {"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "mode=755", "size=1m"]},
            {"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=1777", "size=64m"]},
            {"destination": "/workspace", "type": "bind", "source": str(workspace), "options": ["rbind", "rw", "nosuid", "nodev"]},
            {"destination": "/request.json", "type": "bind", "source": str(case / "request.json"), "options": ["bind", "ro", "nosuid", "nodev"]}],
        "linux": {"namespaces": [{"type": n} for n in ["pid", "network", "ipc", "uts", "mount"]],
                  "maskedPaths": ["/proc/kcore", "/proc/keys", "/proc/timer_list", "/sys/firmware"],
                  "readonlyPaths": ["/proc/sys", "/proc/sysrq-trigger"]},
    }
    (case / "config.json").write_text(json.dumps(config))
    container = "ac-supervisor-" + uuid.uuid4().hex
    before = time.monotonic()
    with (case / "stdout.json").open("w") as stdout, (case / "stderr.log").open("w") as stderr:
        process = subprocess.Popen(runtime + ["run", "--bundle=" + str(case), container], stdout=stdout, stderr=stderr)
        try:
            if cancel or external_stop:
                until = time.monotonic() + 15
                while not (workspace / "started").exists():
                    assert process.poll() is None, "container exited before cancellation"
                    assert time.monotonic() < until, "child did not start"
                    time.sleep(0.05)
                if external_stop:
                    # A wedged/stopped init cannot run its local watchdog. Lack of
                    # a report is unresolved; only an external runtime can intervene.
                    time.sleep(1)
                    assert process.poll() is None
                    assert (case / "stdout.json").stat().st_size == 0
                subprocess.run(runtime + ["kill", container, "KILL" if external_stop else "TERM"], check=True, capture_output=True, timeout=5)
            code = process.wait(timeout=25)
        finally:
            subprocess.run(runtime + ["delete", "--force", container], check=True, capture_output=True, timeout=10)
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
    wall_ms = round((time.monotonic() - before) * 1000)
    if external_stop:
        assert code == 137, (name, code, (case / "stderr.log").read_text())
        assert (case / "stdout.json").stat().st_size == 0
        records.append(dict(name=name, exit_code=code, wall_ms=wall_ms, external_kill=True,
                            local_report_absent=True, authoritative_outcome="unresolved"))
        return None
    if setup_failure:
        assert code == 125, (name, code, (case / "stderr.log").read_text())
        assert (case / "stdout.json").stat().st_size == 0
        assert "Setup" in (case / "stderr.log").read_text()
        records.append(dict(name=name, exit_code=code, wall_ms=wall_ms, rejected=True))
        return None
    assert code == 0, (name, code, (case / "stderr.log").read_text())
    report = json.loads((case / "stdout.json").read_text())
    assert report["outcome"] == outcome, (name, report)
    assert report["children_reaped"], (name, report)
    assert report["execution_id"] == name and report["generation"] == 7
    assert report["request_digest"].startswith("sha256:")
    for stream in ["stdout", "stderr"]:
        assert len(report[stream]["bytes"]) <= request["output_limit_bytes"]
    records.append(dict(name=name, exit_code=code, wall_ms=wall_ms, report=report))
    return report


r = execute("literal", ["/bin/sh", "-c", 'printf "%s" "$1"', "fixture", "a;$(id)"])
assert bytes(r["stdout"]["bytes"]) == b"a;$(id)"
r = execute("environment", ["/bin/env"])
assert sorted(bytes(r["stdout"]["bytes"]).decode().splitlines()) == ["HOME=/tmp", "LANG=C", "PATH=/usr/bin:/bin"]
r = execute("cwd", ["/bin/sh", "-c", "pwd; printf saved > result"], cwd="nested")
assert bytes(r["stdout"]["bytes"]) == b"/workspace/nested\n"
assert (a.work_dir / "cwd/workspace/nested/result").read_text() == "saved"
execute("symlink-cwd", ["/bin/true"], cwd="escape", setup_failure=True)
r = execute("nonzero", ["/bin/sh", "-c", "exit 23"], outcome="failed")
assert r["main_exit_code"] == 23
execute("missing-executable", ["/missing-executable"], outcome="spawn_failed")
r = execute("init-protected", ["/bin/sh", "-c", "if echo forged > /proc/1/fd/1; then exit 1; fi; printf protected"])
assert bytes(r["stdout"]["bytes"]) == b"protected"
execute("suspended-supervisor", ["/bin/sh", "-c", "echo started > /workspace/started; kill -STOP 1; /bin/sleep 10"],
        external_stop=True, lease_budget_ms=200)
loop = "trap '' TERM; echo started > /workspace/started; while :; do /bin/sleep 10; done"
r = execute("timeout", ["/bin/sh", "-c", loop], outcome="timed_out", timeout_seconds=1)
assert r["kill_sent"] and r["main_signal"] == 9 and r["reaped_processes"] >= 2
r = execute("lease-expiry", ["/bin/sh", "-c", loop], outcome="lease_expired", lease_budget_ms=200)
assert r["kill_sent"] and r["elapsed_ms"] < 3000
r = execute("cancel", ["/bin/sh", "-c", loop], outcome="cancelled", cancel=True)
assert r["kill_sent"] and r["main_signal"] == 9
orphan = "/bin/setsid /bin/sh -c \"trap '' TERM; echo started > /workspace/started; while :; do /bin/sleep 10; done\" & while test ! -e /workspace/started; do :; done; exit 0"
r = execute("escaped-descendant", ["/bin/sh", "-c", orphan], outcome="descendants_terminated")
assert r["main_exit_code"] == 0 and r["kill_sent"] and r["reaped_processes"] >= 3
r = execute("output-flood", ["/bin/sh", "-c", "/bin/yes x & /bin/yes y >&2 & wait"], outcome="timed_out", timeout_seconds=1, output_limit_bytes=17)
for stream in ["stdout", "stderr"]:
    assert len(r[stream]["bytes"]) == 17 and r[stream]["truncated"] and r[stream]["observed_bytes"] > 65536
    assert r[stream]["eof"]
assert r["elapsed_ms"] < 3000
version = subprocess.run([str(a.runsc), "--version"], check=True, capture_output=True, text=True).stdout
result = dict(component="sandbox-supervisor", runtime=version.strip(), platform="systrap", network="none",
              cgroup_limits_verified=False, product_execution_admission=False, candidate_mount_verified=False,
              physical_fencing=False, accepted_runtime_tests=[], tests=records,
              fixture_files=json.loads((a.rootfs / "fixture-files.json").read_text()),
              test_script_sha256=hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest())
(a.work_dir / "result.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps(dict(passed=len(records), result=str(a.work_dir / "result.json"))))
