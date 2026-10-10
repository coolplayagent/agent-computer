# 31. Node-guarded execution startup

## 31.1 Delivered path

The [execution worker](29-execution-worker.md) now requires an independently armed [node watchdog](30-node-watchdog.md) before issuing a Candidate startup grant. The new Rust `agent-computer-node` crate connects an authenticated Kubernetes observation to the local K3s/containerd/runsc process tree and the prepared Candidate inode. This is a qualified single-node adapter; it does not accept tenant-supplied host paths or runtime commands.

The worker first obtains the original Pod's startup challenge and rechecks storage. Kubernetes readback requires the exact namespace, Pod UID/spec, configured Node name/UID/boot ID, Ready status, and one Running container with restart count zero. The API observation is an opaque typed value, not proof of a host process. The node adapter then verifies the current host boot, CRI container and sandbox identities, runsc runtime, fixed supervisor argv, read-only root, UID/GID 1000, no-new-privileges and empty capabilities.

It derives the standard systemd Pod parent from the Pod UID and confirms both OCI cgroup paths, the Sentry PID/start ticks, and Sentry plus both Gofers inside the original sandbox scope. The actual kubelet bind must be `/var/lib/kubelet/pods/<UID>/volume-subpaths/<PV handle>/sandbox/2`. Its FUSE data directory must match the prepared inode, owner and mode. The middle component is the recorded PV/CSI handle, not the mount's logical name. Unknown layouts are rejected; the verbose CRI fields and this runsc layout are not claimed as portable guarantees for all container runtimes. See the upstream [CRI protocol](https://raw.githubusercontent.com/kubernetes/cri-api/v0.37.1/pkg/apis/runtime/v1/api.proto) and [containerd status implementation](https://raw.githubusercontent.com/containerd/containerd/main/internal/cri/server/container_status.go).

## 31.2 Fixed timer and durable authorization

The absolute node deadline is derived from the remaining original dispatch attempt before node inspection. Hashing, runtime inspection and arming consume that budget; neither a database read nor writer renewal resets it. Host commands use pinned, root-owned ELF descriptions and configured SHA-256 hashes, clear the environment, bound output to 1 MiB and share a maximum ten-second inspection budget. The watchdog starts in its own session and acknowledges its original cgroup/inode/boot/deadline only after arming the kernel timer. Dropping the live handle never kills or renews it; a detached waiter reaps it if the controller remains alive.

Migration 16 adds immutable `execution_watchdog_arms`, binding the execution, observed Pod, node/boot, container, cgroup inode, evidence digest and expiry. Registration requires the live, non-cloneable `ArmedGuard`, the original dispatch attempt and current authority. It checks the Candidate inode/PV handle and the exact registered Pod plan, emits metadata-only Outbox evidence, then rechecks authority and remaining time before commit. The same node/boot/cgroup inode cannot be assigned twice.

The worker rereads the Pod/Node identity after arming. Grant issuance requires the same live handle and persisted evidence; the remaining grant is capped by the original attempt, database expiry and live node timer. SQL also prevents a planned Pod from receiving a startup grant without a matching unexpired arm. Reading serialized evidence after a restart cannot reconstruct an `ArmedGuard`, replay a grant or authorize another process. Historical grants remain readable without fabricated arms. Every execution still ends in Unknown with its writer Draining; no local process report or empty cgroup grants writer release.

## 31.3 Operator configuration

`execution-dispatch-once` must run as root on the configured node, in the host namespaces and outside the target Pod tree. Extend the private `execution` configuration from [29](29-execution-worker.md) with:

```json
{
  "node": {
    "node": {"name": "ac-component-node", "uid": "ACTUAL_NODE_UID", "boot_id": "ACTUAL_BOOT_UUID"},
    "k3s": {"path": "/usr/local/bin/k3s", "sha256": "sha256:VERIFIED_K3S_DIGEST"},
    "watchdog": {"path": "/usr/local/bin/agent-computer-watchdog", "sha256": "sha256:VERIFIED_WATCHDOG_DIGEST"},
    "runtime_socket": "/run/k3s/containerd/containerd.sock",
    "spool": "/root/agent-computer/watchdogs"
  }
}
```

Replace every placeholder with observed deployment values. Executables and their ancestors must be root-owned and not group/world writable; the spool must be private and root-owned. The adapter creates and removes a private request file there. The local runtime socket is privileged operator access and is never mounted into the sandbox. Kubernetes permission adds only `get` for the configured Node; [the fixture RBAC](../../deploy/testing/node-watchdog-rbac.yaml) is restricted to `ac-component-node`. Do not apply its fixture namespace/identity unchanged to another environment.

Build with `bazel build //crates/node //crates/watchdog:agent-computer-watchdog //crates/server:agent-computer-server`. Recovery only observes and conditionally deletes the original Pod; it never rearms from stored JSON.

The current adapter requires a pair of independently armed processes with the same deadline; see [32 Redundant node watchdogs](32-redundant-node-watchdogs.md). The single-guard runtime evidence below describes its original source revision.

## 31.4 Verification and remaining work

Ten new default cases cover Kubernetes node/boot/container identity, structured CRI rejection, pinned command execution, bounded subprocess I/O, child identity during error cleanup, SQL identity/deadline constraints, immutable WAL recovery and migration 16. SQL fixtures explicitly use synthetic metadata; they do not construct live node handles. The final source passed all 303 default Cargo/Bazel tests, including 134 PostgreSQL cases, across ten Bazel test targets.

The explicit `//crates/worker:execution_worker_live_test` now uses seven 10 GiB Candidates on one 70 GiB retained Volume. It exercises library/command execution, cancellation, lost Pod acknowledgement, image rejection, controller SIGKILL, and controller SIGKILL with a stopped sandbox PID 1. The last case proves that a writer continues after both failures, then checks the pinned cgroup's recursive empty state after the original deadline, before any API cleanup. The normal controller-kill case can also be terminated by the independent PID 1 budget; only the stopped-PID-1 case isolates the external watchdog. Root-only component runs and their exact sources are recorded separately from default tests.

This original runtime evidence does not certify watchdog crash supervision or the later node expiry service/restart protocol. Multi-node routing, remote node authentication, post-kill admission sealing, asynchronous storage drainage, accepted output objects, public execution authorization and full Computer readiness remain pending. The host operator and runtime remain trusted. `EmptyObserved` is a point-in-time observation, not durable fencing. Automatic recovery and production deployment certification remain pending; T01–T43 stay `not_run`.

The final source in `fd672ef` passed all seven live cases after the child-reaping fix. Twelve kernel probes passed against the same unchanged watchdog binary. A separate fresh read-only JuiceFS client fetched both saved files through two S3 GETs; database collection retained five arms/grants and zero writer completions/drains. The [component evidence record](../evidence/node-guarded-startup-2026-10-10.json) and [raw logs](../evidence/node-guarded-startup-2026-10-10.log) retain exact source/binary hashes, full VM readback and earlier failures. After collection, the owned QEMU process was stopped and its ten private VM files were removed.

Review found a host PID-reuse window: `try_wait` could reap the CLI leader before error cleanup signalled its old process-group ID. The adapter now observes exit using `waitid` with `NOWAIT`, sends cleanup signals while the child identity remains reserved, and reaps afterward. The real-child regression, full default suites, workspace Clippy and VM rerun passed on this revision. Earlier runtime observations retain their separate source scope in the record.

The persistent expiry service and separate recovery reports are described in [34](34-node-expiry-reaper.md).
