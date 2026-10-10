# 42. Accepted execution completion

The trusted single-execution worker can now finish a fenced execution and release its Candidate writer. It combines the original dispatch handle, a live kernel/process seal, the Candidate IO barrier, verified durable output and a PostgreSQL transaction. Public execution remains unsupported; Computer `ready` is false and T01–T43 remain `not_run`.

## Live completion authority

Before startup authorization, the node adapter pins the admitted cgroup domain and opens pidfds for the observed gVisor sentry and gofers. Each process must retain its original start ticks and exact cgroup membership across `pidfd_open`; an open `/proc/<pid>` directory prevents a recycled numeric PID from substituting its identity. These are private live handles, not serialized permissions.

After the attach attempt returns, the controller closes Candidate mutation admission, revokes CSI publication and kills the original pinned domain. It requires both recursive cgroup emptiness and exit of every pinned runtime process. A stopped process does not count as exited. A removed original domain is recognized only through `ENODEV` from its pinned, previously validated cgroup v2 core `cgroup.events` FD. Missing paths and directory link counts are not removal evidence. On the qualified Linux kernel, the directory retains link count 2 after deletion.

This relies on the kernel's [cgroup destruction ordering](https://github.com/torvalds/linux/blob/v6.8/kernel/cgroup/cgroup.c): it rejects populated groups and live children, prevents new migration, then removes core files. The [kernfs read path](https://github.com/torvalds/linux/blob/v6.8/fs/kernfs/file.c) returns `ENODEV` for a deactivated node; ordinary read errors remain unconfirmed. [cgroup.kill](https://docs.kernel.org/admin-guide/cgroup-v2.html) covers descendants and concurrent forks. [pidfd polling](https://man7.org/linux/man-pages/man2/pidfd_open.2.html), without `PIDFD_THREAD`, observes process/thread-group exit and remains useful after reaping. These observations do not themselves drain filesystem IO.

The worker then joins the live FUSE mutation barrier and synchronizes the Candidate root. The resulting `SealedExecution` retains the original node guard, pinned kernel identities and `SealedFence`. Its evidence is serializable for audit; the handle is neither cloneable nor deserializable. Independent watchdogs remain armed for their original deadlines. Their retained FDs cannot redirect a kill into a replacement group at the same path.

## Durable outcome and writer release

Migration 23 adds immutable `execution_completions`. One transaction binds the seal to the exact dispatch, registered arm, Pod plan, mount instance, prepared Candidate and writer epoch. It records the accepted outcome and outbox event, appends `execution_drained`, and releases the writer. A deferred constraint rejects incomplete transactions and successful/failed completion crossing the original dispatch deadline during commit.

| State/input at completion | Accepted state |
| --- | --- |
| Dispatching, current authority, original budget, verified `succeeded` output | Succeeded |
| Same conditions with `failed`, `spawn_failed`, `timed_out` or `descendants_terminated` output | Failed |
| Explicit CancelRequested and a live physical/IO seal | Cancelled |
| Missing output, expired/revoked authority or an uncertain supervisor report, with the seal | Unknown |
| Already Unknown, with the seal | Unknown; original revision and reason are preserved |
| Missing process/IO proof or unavailable database | No accepted completion; conservative recovery retains uncertainty |

Cancelled confirms that execution writes are closed after an explicit cancellation; it does not roll back prior effects. Unknown may have a released physical writer while its outcome remains unresolved. Durable output recovery cannot relabel a completion, recreate its live seal or release a writer by itself. Artifact/checkpoint publication continues to reject Unknown execution history, even when its physical writer has been released.

Publication is idempotent for the original dispatch and same seal. A bounded retry handles a rolled-back transaction or ambiguous commit response without repeating the command, startup grant or Pod creation. A recorded completion is checked before the current writer epoch, so retry cannot mutate a later writer. Recovery reads preparation through immutable epoch history; acquiring a new epoch does not make an old execution's cleanup inputs disappear.

## Recovery and limits

The operator dispatch result includes an optional `completion` receipt. The existing execution API exposes Succeeded/Failed and distinguishes queued cancellation from sealed dispatched cancellation using `dispatch_started`. Serialized output, completion, watchdog and IO metadata do not grant execution or storage authority.

Pod cleanup runs after bounded database work, including when completion fails. A controller that survives can seal and complete; a killed controller loses its live IO handle. Its replacement only revokes publication, conditionally deletes the original Pod and observes immutable records. It does not reconstruct a seal from an empty cgroup, missing Pod or recovered output. Cross-node fencing, controller-loss drain recovery, arbitrary App checkpointing and automatic registry reclamation remain separate work.

## Verification

The default suite covers SQL binding rejection, atomic release, explicit cancellation, verified output, immutable/WAL-preserved receipts, historical epoch lookup, Unknown retention and deadline expiry during commit. The disposable VM fixture additionally exercises pinned-domain removal/path reuse, stopped descendants, real gVisor execution through CSI, command failure, timeout, permission revocation, retained Unknown, output storage/publication failure, completion transaction retry and a subsequent file save under the next writer epoch. Run the manual tests only in an explicitly disposable root VM with the private configuration described in [section 41](41-fenced-execution-csi.md).

Evidence is collected separately from the controller using SQL records, signed S3 reads and a fresh read-only JuiceFS client. Component evidence does not establish full Computer product acceptance.

[component record](../evidence/accepted-execution-completion-2026-10-10.json) · [logs](../evidence/accepted-execution-completion-2026-10-10.log)

[51 Durable drain recovery](51-durable-drain-recovery.md) adds automatic publication of original node seals after controller loss. Pre-seal loss and unresolved outcomes remain blocked; this does not complete general drain recovery or cross-node fencing.
