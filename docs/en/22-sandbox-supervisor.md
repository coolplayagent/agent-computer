# 22. Sandbox process supervisor

## 22.1 Delivered component

`agent-computer-sandbox` is a single-use Linux init for an already isolated execution container. It runs one structured argv, drains bounded stdout/stderr, reaps adopted descendants, and requests termination on timeout, cancellation or local lease-budget expiry. Cargo and Bazel build the same Rust crate. This component is not yet connected to durable Execution admission, Candidate mounting, the API, object output storage or writer-lease completion.

The executable refuses to run unless it is PID 1, `/proc` describes that namespace, UID/GID and effective UID/GID are 1000, effective/permitted/bounding capabilities are empty, `NoNewPrivs` is enabled, and no other process is initially visible. There is no host-execution bypass. The deployment must separately enforce gVisor, private namespaces, a read-only root, restricted mounts, network/resource limits and a credential-free image. The binary cannot prove those deployment properties itself.

## 22.2 Input and execution

Invoke `/bin/agent-computer-sandbox --request /request.json` inside the container. The operator supplies a regular local request file and mounts the admitted directory at `/workspace`. Requests are at most 64 KiB; unknown and duplicate fields fail. An example is:

```json
{
  "execution_id": "execution_1",
  "generation": 1,
  "argv": ["/bin/sh", "-c", "printf hello > result.txt"],
  "cwd": "",
  "timeout_seconds": 10,
  "lease_budget_ms": 15000,
  "term_grace_ms": 100,
  "output_limit_bytes": 65536
}
```

The executable path must be absolute. Up to 128 arguments and 32 KiB of argv are accepted. There is no implicit shell; scripts explicitly name their interpreter. `cwd` is empty or a normalized relative directory, at most 1024 bytes/32 segments. An `openat2` directory descriptor resolves beneath `/workspace` without symlinks or crossing another mount; the child changes directory through that pinned descriptor. This confines initial cwd, not every filesystem operation a command can attempt; container mounts remain the filesystem boundary.

The child receives null stdin and only `PATH=/usr/bin:/bin`, `HOME=/tmp`, `LANG=C`. Environment references and stdin references are pending. The supervisor makes itself non-dumpable before spawning to deny same-UID access to its memory and `/proc/1/fd` descriptors. A new process group is allocated. Arbitrary interpreters still require modify admission in the future control integration, regardless of command text.

`timeout_seconds` is 1–3600, `lease_budget_ms` is 1–30000 and TERM grace is 0–5000 ms. A monotonic clock starts before cwd resolution/spawn, and the earlier timeout/budget wins. Budget is trusted orchestration input, not a signed lease or permission. A dispatcher must deduct delivery time conservatively; no renewal protocol is implemented. Blocking mount/exec operations or a suspended supervisor require the independent runtime watchdog.

## 22.3 Termination and output

The supervisor observes SIGTERM, SIGINT and SIGHUP as cancellation. Once stopping, it repeatedly signals every signalable process in its private PID namespace, including descendants that used `setsid` or changed process groups. After the grace it sends SIGKILL. The loop bounds both output reads and `wait(2)` work so output/fork floods cannot indefinitely starve the timer. It waits for `ECHILD` and both output EOFs, with a two-second final drain bound. Failure to establish that result is `unknown`. Dropping the Rust future also requests SIGKILL, without asserting successful reaping.

Main-process exit alone is insufficient: surviving descendants trigger cleanup and `descendants_terminated`, even if the main process exited 0. Normal success requires exit 0, all descendants reaped, both EOFs and an unexpired deadline. TERM-resistant timeout/cancel cases may have a main SIGKILL status and a distinct local outcome. A deadline first observed after exit wins over apparent success. Setup errors exit 125; a successfully serialized local report exits 0, so exit code 0 alone says nothing about command success.

Each stream retains a prefix of 0–1 MiB, counts observed bytes and marks truncation while continuing to drain excess data. The bounded report contains the input digest, execution identity/generation, main exit/signal, reaped count, elapsed time and binary byte arrays. It is a local collector format; no output bytes are written to PostgreSQL. Durable chunks, object references and authenticated result acceptance remain pending.

## 22.4 Trust limits and reproduction

**The report is not physical fencing or a durable writer drain proof.** It cannot release a Candidate lease. Linux namespace-init semantics and the supported runtime's actual behavior must both be checked; the [Linux PID namespace manual](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html) describes namespace init and process termination. In the tested gVisor `release-20261005.0`, a same-UID child using `kill -STOP 1` suspended init and its local watchdog. The fault test requires an external runtime kill and records no local report, leaving the authoritative outcome unresolved. It does not reinterpret container deletion as product fencing. A trusted external watchdog and node/fence evidence are mandatory before runtime integration can permit shared-workspace handoff.

The explicit component scripts are [rootfs.py](../../crates/sandbox/tests/rootfs.py) and [component.py](../../crates/sandbox/tests/component.py). The fixture copies the locally built trusted binary, selected shell tools and their dynamic libraries; it is not a product OCI image. Run its `ldd` step only on trusted local binaries. Build with:

```sh
cargo build -p agent-computer-sandbox --locked
bazel build //crates/sandbox:agent-computer-sandbox --lockfile_mode=error
python3 crates/sandbox/tests/rootfs.py \
  --supervisor target/debug/agent-computer-sandbox \
  --destination /tmp/supervisor-rootfs
```

Transfer the fixture and component script into a disposable Linux VM containing the pinned runsc release. As root **inside that VM**, run:

```sh
python3 component.py --runsc /usr/local/bin/runsc \
  --rootfs /root/supervisor-rootfs --work-dir /root/supervisor-component
```

The script uses fresh private runtime state, runs each container as nonroot with no capabilities, forces cleanup, and records binary/library hashes. Thirteen cases cover argv boundaries, environment clearing, confined cwd, symlink rejection, exit/spawn failures, init descriptor protection, suspended-init uncertainty, timeout, lease expiry, cancellation, escaped descendants and both-stream flooding. It uses an ordinary temporary directory and `--ignore-cgroups=true`; actual Candidate/CSI binding, cgroup enforcement, Kubernetes lifecycle, database/network partitions, authenticated execution results and T01–T43 acceptance remain unverified.

All thirteen component assertions passed on source `f7a8a2a`, using the Bazel-built binary. [Pinned evidence](../evidence/sandbox-supervisor-2026-10-10.json) retains each OCI/request input, runtime/library hashes and the suspended-init limitation. The disposable VM, overlay and private SSH artifacts were removed.
