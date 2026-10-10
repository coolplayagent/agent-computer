# 48. Bounded background execution

New execution submissions default to `lifetime: background`. The submitted task keeps its reserved writer epoch when its logical connection closes. Explicit `lifetime: connection` retains the previous behavior: closing the connection lowers writer authority, and dispatched work without completion evidence becomes Unknown. The immutable lifetime is returned in execution metadata and participates in the idempotency digest.

```json
{"lease_id":"writer_example","lease":{"connection_session_id":"session_example","generation":1,"epoch":1,"expected_revision":1},"sandbox_id":"sandbox_example","command":{"argv":["/bin/sh","-c","printf saved > result.txt"],"cwd":"","timeout_seconds":10,"term_grace_ms":500,"output_limit_bytes":4096}}
```

## Authority and limits

This disconnect policy does not itself extend a deadline. Fixed v1 executions retain their original budget of at most 30 seconds, reduced by setup. New admissions can separately use [renewable execution leases](49-renewable-execution-leases.md); only the original trusted worker can authorize and confirm those extensions.

A Closed connection has no capabilities and cannot renew, acquire, save files or submit new work. Reconnecting cannot steal the reserved epoch or change its deadline. The original still-valid credential may query, replay or cancel its owned execution. Another credential does not inherit that ownership. Explicit cancellation and `checkpoint-stop` with `cancel_running=true` still cancel background work; cancellation alone never proves that a process stopped.

Only the logical connection Active check is replaced for the exact immutable background reservation. Original principal, credential, scopes, Computer/Workspace grants, catalog, generation, Candidate identity and fixed deadlines remain required at dispatch, startup, observation and completion. Revocation still drains the writer and leaves unconfirmed dispatch Unknown. Live process/IO proof and verified output remain necessary for accepted completion and release. Restart never reconstructs that proof or replays a dispatch.

The session-close transaction preserves only pending background reservations. Cancelled undispatched work does not retain a writer across close; old background history cannot authorize a new epoch. The background identity predicate includes terminal history because successful completion rechecks authority in the same transaction after recording the outcome. This does not bypass writer state, deadline or drain checks.

## Compatibility and verification

Migration 28 constrains stored lifetime values and adds the exact organization/lease/epoch predicate. Existing explicit `connection` inputs, hashes and receipts are unchanged. Old serialized response metadata without lifetime reads as connection; new omitted request lifetime reads as background. The capability advertises `execution.admission: bounded-queued`; public `execution` remains unsupported and Computer ready remains false.

Database and HTTP regressions cover disconnect before/after dispatch, WAL restart, outbox rollback, retry conflicts, immutable modes, expiry, credential/principal/grant/catalog revocation, denied new writes and checkpoint cancellation. The real gVisor fixture closes one background connection before dispatch and another after observing a flushed start marker, before the output file exists. It verifies accepted completion, a retry using the same live completion seal, retained bytes and authenticated output downloads. A third detached task is explicitly cancelled, checkpointed and restored. Independent SQL, signed S3 reads and a separate readonly JuiceFS client verify the results.

[Source-bound record](../evidence/background-execution-2026-10-10.json) · [Validation log](../evidence/background-execution-2026-10-10.log)

Evidence is limited to one disposable Linux VM and the bounded integrated path. Full lifecycle scheduling, App/browser health and checkpointing, automatic drain recovery, cross-node fencing, T01–T43 product acceptance remain incomplete.
