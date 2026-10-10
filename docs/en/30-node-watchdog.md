# 30. Node cgroup watchdog

## 30.1 Delivered component

`agent-computer-watchdog` is a single-use Linux host process, built with Rust and Bazel. It runs outside a trusted operator's dedicated workload cgroup and terminates that subtree at a fixed deadline, independently of the execution controller and the in-container PID 1. It does not launch a workload or contact PostgreSQL, Kubernetes or a runtime socket. [31's local node adapter](31-node-guarded-startup.md) now invokes it before the execution worker issues a startup grant.

The request binds an execution correlation ID, the current node boot ID, a normalized path under `/sys/fs/cgroup`, the expected cgroup inode, and an absolute `CLOCK_BOOTTIME` deadline. Future deadlines must be within 30 seconds; expired requests terminate immediately. Retrying an identical request does not reset its deadline. Execution ID is a correlation value, not evidence that the cgroup belongs to that execution.

## 30.2 Kernel boundary

The component requires root and a real cgroup v2 mount. It rejects the hierarchy root, its own cgroup or ancestors, noncanonical paths, mismatched boot/inode identities, threaded domains and group/world-writable or non-root-owned migration controls along the target path. `openat2` disallows symlinks, traversal and crossing descendant mounts. Open directory/control-file descriptions pin the original cgroup; deletion and path reuse never retarget the watchdog to a replacement tree.

An absolute `CLOCK_BOOTTIME` timerfd is armed before emitting the `armed` receipt. This clock counts suspended time and is independent of wall-clock adjustments; it does not wake a suspended machine or guarantee scheduling latency. A closed or full receipt pipe triggers immediate termination. The CLI uses atomic nonblocking pipe frames up to 4096 bytes, so controller output backpressure cannot postpone termination. Stdout must be a pipe, not a terminal or regular file.

At expiry the component writes `1` to the pinned `cgroup.kill` file, then polls the pinned `cgroup.events` for up to five seconds. A successful kill followed by recursive `populated 0` yields `EmptyObserved`; failed kill, lost observation or observation timeout yields `Unknown`. Setup failures after opening a validated group also attempt termination. The [kernel cgroup v2 specification](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html) defines subtree kill and recursive live-process observations; the [timerfd manual](https://www.man7.org/linux/man-pages/man2/timerfd_create.2.html) defines the clock and absolute timer semantics.

## 30.3 Trusted invocation and limits

Build with `bazel build //crates/watchdog:agent-computer-watchdog`. A trusted node operator supplies a bounded JSON file to `agent-computer-watchdog --request PATH` and reads the two JSON lines through a pipe: `armed`, then a report. The report contains the fixed request, device identity, arm/kill/observation times, trigger and observation. Exit 0 means a local `EmptyObserved` report was emitted; exit 2 means rejection, unknown observation or unavailable output. Missing output never proves that termination did not happen.

The operator must run it in the host's initial namespaces, outside the workload tree and its controller's lifecycle, retain exclusive cgroup migration/admission authority, and use the fixed deadline established before launch. The CLI creates an independent session; it provides no service supervision, authenticated remote protocol or restart recovery. Durable execution-arm registration belongs to the node adapter/store integration. A killed/stopped watchdog, kernel failure or node partition still requires trusted recovery. Replaying a request is not a production recovery protocol.

`EmptyObserved` is a point-in-time local kernel observation. It does not prevent later process admission, prove that asynchronous storage operations have drained, establish the Pod/container/Candidate mapping, accept command output, or release a database writer. [31](31-node-guarded-startup.md) delivers local Pod/runtime/cgroup identity and durable arming before the startup grant. Production operation still needs durable node service registration, supervision and crash recovery, storage fencing and database reconciliation. Execution remains unsupported on the public runtime; executions retain Unknown/Draining under [29's workflow](29-execution-worker.md). T01–T43 remain `not_run`.

## 30.4 Verification

Five default contracts cover absolute deadlines, boot identity, self/ancestor protection, strict bounded request parsing and explicit recursive-empty interpretation. This increment originally established 293 tests across nine Bazel test targets; [31](31-node-guarded-startup.md) records the current count.

Explicit root-only tests must run in a disposable VM:

```bash
python3 crates/watchdog/tests/component.py --watchdog /absolute/path/agent-computer-watchdog --output /private/path/kernel.json
python3 crates/sandbox/tests/rootfs.py --supervisor /absolute/path/agent-computer-sandbox --destination /private/path/rootfs
python3 crates/watchdog/tests/runsc_component.py --watchdog /absolute/path/agent-computer-watchdog --runsc /absolute/path/runsc --rootfs /private/path/rootfs --work-dir /private/path/fresh-run
```

The kernel fixture covers stopped supervisors and nested session-escaping descendants, frozen trees, expired deadlines, closed/full pipes, controller exit, cgroup deletion/reuse, mismatched inode/boot, self-ancestor targeting, delegated controls and threaded topology. The gVisor fixture first proves that a child keeps working after stopping PID 1 and exceeding its local lease, then checks that the external watchdog terminates the actual runsc/gofer tree. These are component probes with a local fixture directory, without a real Candidate or Kubernetes binding. Source-bound evidence is recorded separately from default tests.

The explicit probes passed 12 kernel cases and one gVisor PID 1 STOP case on the exact code later committed as `2bde240`. The [source-bound record](../evidence/node-watchdog-2026-10-10.json) verifies 206 source/build inputs and exact binaries; [raw output](../evidence/node-watchdog-2026-10-10.log) also preserves the earlier threaded-topology fixture failure. The gVisor child demonstrably ran beyond the 200 ms local lease before the node watchdog killed the runtime tree at its fixed deadline; the kernel reported no live processes 10 ms later. The owned VM, disk and private credentials were removed. This is component evidence only, with no accepted completion, writer release or product acceptance.
