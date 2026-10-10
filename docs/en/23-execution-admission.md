# 23. Durable execution admission

[48 Bounded background execution](48-background-execution.md) adds a default background lifetime while preserving the original fixed deadlines and identity requirements.

## 23.1 Migration 12 behavior (historical)

Migration 12 and the authenticated HTTP API persist a connection-scoped command queue on a Prepared Candidate. Submission reserves the current writer epoch against competing file dispatch, fixes all inputs and records an event/Outbox entry in one transaction. No runtime dispatcher is connected yet: capabilities report `execution.admission: connection-queued` and `execution: unsupported`; queue-only records have `dispatch_started: false`. [24 Dispatch journal](24-execution-dispatch.md) subsequently adds trusted dispatch intents and unresolved post-dispatch states.

This increment supports an explicit `lifetime: connection`. Closing the original connection, losing authorization, changing the current Candidate, or reaching the fixed queue deadline cancels an undispatched reservation when it is queried/reconciled or its writer is released. Migration 28 adds bounded background reservations as described in chapter 48; longer independent execution budgets remain pending. An HTTP disconnect by itself does not close a ConnectionSession or undo a committed admission; retry with the same key and input.

## 23.2 HTTP contract

All endpoints use the original service credential and `runtime.connect`. Browser Origin authentication remains unsupported. First submission additionally checks `runtime.read`/`runtime.modify`, active own connection capabilities, Computer connect/read/modify grants, Workspace read/modify grants and exact writer generation/epoch/revision. Metadata and cancellation remain available with the valid original credential after connection closure or resource-grant loss. Another credential for the same principal cannot access the execution. The metadata view omits command bytes and storage paths.

| Endpoint | Input | Result |
| --- | --- | --- |
| `POST /v1alpha1/computers/{id}/executions` | Idempotency-Key and SubmitExecution | 202 Queued; exact retry returns current metadata, 200 after leaving Queued |
| `GET /v1alpha1/executions/{id}` | Original credential | 200 metadata; reconciles invalid/expired Queued state to durable Cancelled |
| `POST /v1alpha1/executions/{id}/cancel` | Idempotency-Key and execution `expected_revision` | 200 Cancelled before dispatch; CancelRequested/Unknown after dispatch, without releasing the writer lease |

Requests require uncompressed JSON of at most 64 KiB; unknown/duplicate fields fail. A submission example is:

```json
{
  "lease_id": "lease_1",
  "lease": {
    "connection_session_id": "connection_1",
    "generation": 1,
    "epoch": 1,
    "expected_revision": 1
  },
  "sandbox_id": "sandbox_1",
  "lifetime": "connection",
  "command": {
    "argv": ["/bin/sh", "-c", "printf hello > result.txt"],
    "cwd": "",
    "timeout_seconds": 10,
    "term_grace_ms": 100,
    "output_limit_bytes": 65536
  }
}
```

Command validation shares the [22 supervisor](22-sandbox-supervisor.md) implementation: absolute executable, explicit interpreter, at most 128 arguments/32 KiB, normalized relative cwd, timeout 1–3600 seconds, TERM grace 0–5000 ms and retained output 0–1 MiB per stream. Timeout must also fit the pinned Computer start budget. The API cannot supply a lease budget, process-stopped flag, arbitrary environment or stdin reference. Cwd confinement is validated syntactically here; actual descriptor/mount enforcement belongs to runtime dispatch.

## 23.3 Binding, reservation and cancellation

The selected Sandbox must be directly referenced by the admitted Computer snapshot, use gVisor, and not be used by an App in that snapshot. Admission fixes its immutable revision/spec digest, image/resources/network dependencies, the Candidate preparation receipt and inode, actual storage binding and committed Workspace input revision. It uses the pinned snapshot rather than silently adopting a newer declaration. Pinned catalog disable/version drift still invalidates authority. Runtime mount/security compatibility must be checked by the future adapter before any effect.

There is one execution record per writer epoch. Creating it increments the writer revision, so re-read the lease before renewal or release. The queue deadline is the writer expiry captured at admission; retry and later lease renewal cannot extend it. The deadline is at most 30 seconds under the current lease policy. An already dispatched writer rejects queue admission. Conversely, a queued reservation blocks the public/trusted file dispatch boundary and the database dispatch trigger. `dispatch_recorded: false` on the lease means no external effect intent, not that the slot is unreserved.

Before dispatch, user cancellation uses execution revision CAS and does not release or renew the lease. It frees the reservation for a file dispatch, but another execution needs a new writer epoch. A writer release atomically cancels its queued execution before inserting a zero-dispatch proof and releasing ownership. For an undispatched reservation, expiry/revocation reconciliation only lowers queued authority. Cancelled records never become Queued again; exact original submission retries return that terminal record, including after a later epoch. Events contain IDs, state and digests, not command bytes. Cancellation/Outbox failures roll back the entire transaction.

Migration 12 permits only Queued → Cancelled, retains immutable input/binding/history, and guards queue/dispatch/drain mutual exclusion. Existing writer dispatches are preserved on upgrade and do not acquire invented execution records or drain evidence. Migration 13 extends these guards with a durable dispatch journal; see [24](24-execution-dispatch.md). Running and accepted completion still require runtime evidence.

## 23.4 Verification and remaining work

Eleven new real PostgreSQL cases cover WAL recovery, exact retries, pinned binding, immutable rows, old-generation/foreign-Sandbox rejection, original credential isolation, revocation/connection closure, queue deadline, file-dispatch races, admission and cancellation rollback, and migration 12. Two HTTP cases cover queue/query/cancel/retry and malformed/unknown-lifetime/oversized/browser requests. These tests use synthetic preparation receipts and establish control-state behavior only.

At this increment, the default workspace had 242 tests, including 104 PostgreSQL and 19 HTTP cases. Cargo tests, fmt, Clippy, Bazel build/test, OpenAPI meta-schema/local references and bilingual documentation checks pass. Existing full Qualitygate covers line endings only; it does not establish runtime acceptance. T01–T43 remain `not_run`.

Later chapters deliver guarded Pod/Candidate mounts, node watchdogs, dispatch, durable output, accepted completion and bounded background lifetimes. Physical Unknown recovery, cross-node fencing and longer execution budgets remain pending. The standalone supervisor's JSON report cannot authorize a process or release this lease.
