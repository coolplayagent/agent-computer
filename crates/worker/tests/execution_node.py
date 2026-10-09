#!/usr/bin/env python3
"""Independent node observation for the disposable execution worker fixture."""
import argparse
import json
import os
import pathlib
import subprocess
import time

parser = argparse.ArgumentParser()
parser.add_argument("--observation", type=pathlib.Path, required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
args = parser.parse_args()
if os.geteuid() != 0:
    raise SystemExit("run only as the trusted controller in a disposable VM")
deadline = time.monotonic() + 180
while not args.observation.exists():
    if time.monotonic() >= deadline:
        raise SystemExit("execution inspection marker did not arrive")
    time.sleep(0.05)
observed = json.loads(args.observation.read_text())


def read(argv):
    return json.loads(subprocess.check_output(argv, timeout=10))


pod = read(["k3s", "kubectl", "get", "pod", "-n", observed["namespace"],
            observed["name"], "-o", "json"])
assert pod["metadata"]["uid"] == observed["uid"]
assert pod["status"]["phase"] == "Running"
container = pod["status"]["containerStatuses"][0]["containerID"].removeprefix("containerd://")
info = read(["k3s", "ctr", "-n", "k8s.io", "containers", "info", container])
cri = read(["k3s", "crictl", "inspect", container])
runtime = info.get("Runtime", info.get("runtime", {}))
assert runtime.get("Name", runtime.get("name")) == "io.containerd.runsc.v1"
spec = cri["info"]["runtimeSpec"]
assert spec["root"]["readonly"] is True
assert spec["process"]["user"]["uid"] == 1000
assert spec["process"]["user"]["gid"] == 1000
workspace = next(m for m in spec["mounts"] if m["destination"] == "/workspace")
assert workspace["source"].startswith("/var/lib/kubelet/pods/" + observed["uid"] + "/volume-subpaths/")
assert workspace["source"].endswith("/sandbox/2")
stat = os.stat(workspace["source"])
prepared = observed["prepared"]
assert stat.st_ino == prepared["data_inode"]
assert stat.st_uid == stat.st_gid == 1000
assert stat.st_mode & 0o777 == 0o700
binding = json.loads(pod["metadata"]["annotations"]["agent-computer.io/binding"])
assert binding["workspace"]["prepared"] == prepared
record = {"pod_name": observed["name"], "pod_uid": observed["uid"],
          "container_id": container, "runtime": runtime, "workspace_mount": workspace,
          "node_mount_stat": {"inode": stat.st_ino, "uid": stat.st_uid,
                              "gid": stat.st_gid, "mode": stat.st_mode},
          "oci_root_readonly": True, "oci_user": spec["process"]["user"],
          "pod_security_context": pod["spec"]["securityContext"],
          "limits": ["one real node observation; not physical fencing or product acceptance"]}
args.output.write_text(json.dumps(record, indent=2) + "\n")
args.observation.with_suffix(".inspected").touch()
print("EXECUTION_NODE_IDENTITY_AND_INODE_VERIFIED")
