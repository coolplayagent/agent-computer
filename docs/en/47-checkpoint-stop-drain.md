# 47. Explicit execution cancellation during checkpoint stop

The optional `cancel_running=true` field makes `checkpoint-stop` drain the current Candidate. Omitting it or sending false retains the already-drained requirement and the canonical idempotency digest of existing requests.

```http
POST /v1alpha1/computers/{id}/checkpoint-stop
Authorization: Bearer <credential>
Idempotency-Key: <stable-key>
Content-Type: application/json

{"request_id":"start_example","expected_revision":4,"publish_current":true,"cancel_running":true}
```

## States and evidence

Admission atomically records Artifact and runtime `Draining`, a control revision, events/outbox, cancellation of undispatched work, and `CancelRequested` for dispatched executions. Leaving Prepared prevents acquisition or renewal of writer leases, new execution or file modification, and new startup grants. Existing executions may only confirm cancellation; this path grants no new execution authority.

Cancellation is not termination evidence. Undispatched work uses the existing `no_dispatch` proof; bounded file work requires its sealed completion; dispatched work requires a live process and IO drain seal from the original trusted worker. Original identity, credential, grants, deadline and storage checks remain in force. Controller loss, authority loss or exhausted budgets can leave Unknown. Restart, Pod deletion, expiry or a new publisher credential cannot invent a result.

The Artifact worker discovers Draining work, rechecks publication authority and active use, and collects only existing drain proofs. Incomplete proof leaves the work Draining without a capture lease. Metadata reports `drain_reason=drain_pending`, or `recovery_blocked` when an execution is Unknown. Unknown blocks capture even after physical writer release. Complete proof atomically promotes Artifact to Capturing and runtime to Sealing, incrementing the control revision again, before the existing capture, complete S3 verification, publication and stop transaction.

Only `stop_receipt` confirms stop. A 202 response, cancellation request or uploaded object does not. Head CAS, branch and conflict semantics follow [checkpoint stop](46-checkpoint-stop-worker.md). Historical retries cannot stop a new generation. The original principal can refresh the publisher credential with the same key, input and mode; this cannot change the execution owner or the immutable `cancel_running` choice.

## Authorization and migration

Computer read/modify/manage, Workspace read/modify/publish, and matching credential scopes remain required. Declared Apps block admission because profile capture is pending. Another principal's active connection or active human input also blocks normal stop; newly active use blocks promotion and finalization. This option does not implement force stop.

Migration 27 preserves artifact identity and idempotency input, defaulting historical `cancel_running` to false. Database constraints reject skipped drain phases, sealing without proof, capture leases while draining, mode changes and mismatched runtime boundaries. Upgrade fixtures restore the previous functions and checks, then verify migration; old history comparisons explicitly exclude newly defaulted columns.

## Verification and limits

Database and HTTP regressions cover atomic cancellation and outbox rollback, WAL restart, mode conflicts, original credential revocation and publisher credential replacement, Unknown, Apps and active use, rejected new writers and competing claims. The real VM fixture requests stop after a gVisor process fully writes `started.txt`, verifies that capture cannot begin before drain, accepts its cancellation completion, then publishes and restores into a new Candidate. Independent SQL, signed S3 GETs and a new readonly JuiceFS client with cache disabled verify bytes, receipts and distinct inodes; `late.txt` is absent.

[Source-bound record](../evidence/checkpoint-stop-drain-2026-10-10.json) · [Validation log](../evidence/checkpoint-stop-drain-2026-10-10.log)

Evidence covers one node and the integrated execution path. General process draining, automatic recovery after controller loss, cross-node fencing, App/browser checkpointing, GC and full lifecycle scheduling remain incomplete. Computer ready=false, public execution remains unsupported, and T01–T43 product acceptance is unchanged.
