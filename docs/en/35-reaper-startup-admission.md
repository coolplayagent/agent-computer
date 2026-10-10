# 35. Reaper availability in startup admission

## 35.1 Live service binding

The [persistent expiry service](34-node-expiry-reaper.md) is now required before a node guard can authorize a new startup. Two independent watchdogs first persist their enrollment and arm the original fixed deadline. The node then sends a fresh 256-bit random challenge to `reaper.sock` in the same private spool. The service checks both exact journal references, matching enrolled request and cgroup device, distinct original guard PIDs, current boot and cgroup identity, absence of final reports, and a future deadline. Merely checking the cgroup does not construct a kill-on-drop handle.

The reply binds the nonce, request, both journals, cgroup device, service instance/PID, spool device/inode and observation time. Each service start generates a new random instance. The endpoint is created while the service holds its exclusive spool lock, before readiness notification. Requests run on the expiry scan loop; a separate heartbeat thread cannot keep answering while that loop is stalled. A single scan batch services at most eight requests, with additional polling during the existing 250 ms inter-pass interval. Frames are limited to 4 KiB and socket IO is nonblocking.

This is trusted host-local IPC. Root-owned private directories and sockets prevent tenant access, and a connected datagram client accepts replies from its selected endpoint. It is not a signed attestation or protection against compromised host root. Keep the spool, runtime and node binaries in the existing host trust boundary.

## 35.2 Fresh checks and irreversible handle failure

Every existing `ArmedGuard::remaining_budget_ms` boundary now performs a new challenge before checking the original guard processes and computing remaining budget. This covers arm registration, startup grants and subsequent runtime checks. The client requires the same service instance, PID and spool identity throughout the handle's lifetime. Any probe failure permanently invalidates that client. Resuming or restarting the service cannot revive it; serialized receipts cannot construct a live guard.

A successful probe must complete within 200 ms, report a time between challenge start and receipt, and finish before the original execution deadline. Probe time consumes that deadline; it never resets it. Filesystem operations remain synchronous and are not cancellable, so 200 ms is an acceptance bound, not a hard bound on blocked filesystem calls. Store checks currently call the local probe synchronously while holding their transaction; unavailable or slow storage may delay that transaction. A failure immediately after a successful reply is inherently possible; the independent guard pair and restartable expiry service retain their separate roles.

Migration 18 binds the initial receipt to the immutable arm and its two journal references, checks field formats and observation freshness, rejects duplicate instance/nonce pairs on the same boot, and requires reaper evidence before a new planned-Pod startup grant. Stored JSON is audit metadata, not proof of current liveness. Migration preserves historical rows and does not backfill authorization: a historical arm without reaper evidence cannot issue a new planned-Pod grant.

## 35.3 Deployment and verification

Install the [systemd unit](../../deploy/systemd/agent-computer-expiry-reaper.service) using the [existing procedure](34-node-expiry-reaper.md#343-operator-installation), with exactly the execution worker's `node.spool`. Drain dispatches during binary replacement and update pinned hashes. The service must run before new dispatches. A missing endpoint now fails at the Watchdog phase before arm registration or grant creation; the worker retains Unknown/Draining and does not run the user command. Already armed independent timers retain their original deadline on setup failure.

Validation includes reply replay/transplant/freshness rejection, PostgreSQL metadata constraints and historical migration behavior. A root fixture checks missing/stopped service refusal, healthy probes without killing the target, duplicate journal rejection, restart identity changes and permanent invalidation after failure. The real single-node K3s/CSI/gVisor worker fixture adds missing-service refusal to its execution, cancellation and controller/PID 1 failure scenarios. This is component evidence, not completion of product T01–T43.

The service still only enforces termination intent. Neither its liveness nor an empty cgroup proves storage fencing, writer drain, accepted output or successful completion. Those remain separate pending work.

The 2026-10-10 [source-bound evidence](../evidence/reaper-startup-admission-2026-10-10.json) and [logs](../evidence/reaper-startup-admission-2026-10-10.log) record 315 passing default tests (138 PostgreSQL cases), ten Bazel test targets, 38 root component scenarios and eight real worker scenarios. Five durable arms have distinct challenges bound to the same live service instance. Missing-service refusal creates no arm or grant; all eight writers remain Draining. A new read-only JuiceFS client read back both nine-byte outputs with two object GETs. The owned VM and its private files were removed.
