# 49. Renewable execution leases

New execution admissions default to `renewable: true`. The original trusted worker can keep a command running across the initial 30-second window while preserving bounded expiry at the supervisor, both node watchdogs and the database. `renewable: false` retains fixed protocol v1. This policy is separate from [background lifetime](48-background-execution.md): closing a logical connection does not renew a deadline.

## Admission and compatibility

The immutable binding pins an execution duration ceiling: the minimum of command timeout plus 30 seconds, the admitted Computer start duration and 3,630,000 milliseconds. Dispatch further caps its absolute hard deadline by the original credential expiry, and by session expiry for connection lifetime. Queue and startup setup retain their original deadline; scheduling, reconnect, recovery and client writer renewal cannot extend it. This ceiling is an execution duration, not a completed Computer activation/idle lifetime implementation.

Migration 29 leaves old input, binding, startup and dispatch hashes intact. Existing rows without an execution lease policy stay fixed, including queued history. New omitted `renewable` input is serialized without that field; its policy is added to the new immutable binding. Omitted, explicit true and explicit false remain distinct retry inputs. Returned execution metadata includes the admitted boolean, which is not proof that a controller is alive.

Protocol v2 bootstrap and startup grants bind their hard budget into their digests. Protocol v1 fields and hash domains retain their original serialization. A recovered record cannot recreate a dispatch attempt, attach channel, live guard or completion seal.

## Renewal transaction and transport

PID 1 emits a fresh nonce and increasing sequence, anchored before emission, after approximately one third of its current window (at most ten seconds). One bounded challenge may be outstanding. The authenticated v5 attach stream stays open for v2. Receiving a challenge does not extend any deadline.

1. The Store checks the original live attempt, both guards, original credential/scopes, resource grants, catalog, Candidate, epoch and current deadline. It commits an immutable authorization and Outbox event for that exact startup grant, challenge and node command. Authorization alone does not extend the database or writer deadline.
2. The original live node handle sends the same command to both original watchdogs. Each requires an unexpired timer, increasing sequence, matching request identity and an extension within the original hard ceiling. Both persist their receipts before acknowledging. The same live reaper instance must confirm both exact journal receipts.
3. A second Store transaction rechecks authority and the old deadline, commits both receipts and its Outbox event, and advances the writer expiry. Deferred checks reject commit after the old deadline or without the writer update. Only then can the worker send the response to PID 1, still before the old attach deadline.

Every response grants at most a 30-second window. Each clock domain clamps that window to its own conservative original hard ceiling. Delay from the earlier challenge or authorization anchor is consumed; delivery never starts a fresh duration. Replayed, skipped, cross-execution, unknown, oversized or late frames fail closed. Partial or cancelled writes poison the channel; there is no blind resend or expiry revival.

## Independent termination and uncertain results

A watchdog atomically resets its Linux `CLOCK_BOOTTIME` timer and checks the previous timer's remaining interval. A reset crossing old expiry causes immediate termination. Nonblocking control IO and a bounded separate journal worker keep filesystem writes and `fsync` off the timer path. A journal stall therefore cannot hold up cgroup termination.

The reaper uses a validated latest accepted renewal tied to the original root-private journal and immutable per-sequence record. Missing or unreadable renewal evidence falls back to the original deadline. During concurrent publication it may conservatively terminate early. Its observation never reconstructs a live worker or renews a lease.

A watchdog may have accepted a fresh database-authorized window when its acknowledgement is lost or the second Store transaction fails. The database keeps the old acknowledged deadline; physical supervision remains bounded by the last authorized node window. The live adapter closes both controls on partial acknowledgement, the worker closes the IO fence, and the result remains uncertain. This is not permission to retry the command. Controller or database loss cannot sustain indefinite execution.

Output manifests and final reports bind their accepted sequence and grant digest. Successful/failed completion also requires the original live process/IO seal and exact acknowledged renewal receipts. A stale report, an unacknowledged node extension, or recovered journal metadata cannot promote Unknown to success. Explicit cancellation, authority loss and hard expiry retain the existing drain and completion rules.

## Verification boundary

Contract tests exercise v1 serialization, strict v2 transport, anchored expiry, immutable policy, WAL recovery, both confirmation records and transaction rollback. The explicit disposable-VM fixtures include a command longer than 30 seconds after disconnect, cancellation/revocation/controller loss after renewal, authorization/acknowledgement transaction failures and hard expiry. The root node fixture additionally freezes a private journal filesystem and checks termination independently of stalled persistence. The [source-bound record](../evidence/renewable-execution-leases-2026-10-10.json) and [captured logs](../evidence/renewable-execution-leases-2026-10-10.log) retain 451 passing Cargo tests, 15 passing Bazel targets, 25 real execution cases and eight node renewal cases. The long command completed four renewals after disconnect; independent SQL verified 13 authorizations and 12 acknowledgements, including the deliberately failed second acknowledgement transaction. Separate reads verified 48 S3 references, 22 authenticated output downloads and ten Candidate files. The owned VM and private state were removed after collection. Earlier fixture failures and the final documentation-only source difference are explicitly retained.

This remains the node-local Candidate execution path. Full Computer lifecycle scheduling, automatic drain recovery, multi-node fencing, output streaming, browser/ComputerView and product acceptance remain pending. Computer `ready=false`, public `execution` stays unsupported and T01–T43 stay `not_run`.
