# 51. Durable recovery of completed execution drains

The original node now records a completed process and IO seal before returning it to the worker. If that worker exits before its database transaction commits, `execution-worker` can publish the retained receipt after the original execution window closes. No command, Pod creation, startup grant or renewal is repeated.

## Receipt and authority

The receipt is written only while the original `SealedExecution` still holds its pinned process descriptors and sealed Candidate IO gate. It contains the same version 1 seal accepted by the existing completion transaction. A private staging file is synced, renamed without replacement, then its parent directory is synced. The filename binds the complete original watchdog arm, and the record includes a content digest. Conflicting second receipts are rejected.

The node pins its configured root-owned private spool before arming. Recovery checks every path ancestor, file type, ownership, private mode, single link, byte limit, digest and exact database-registered arm. Symlinks, incomplete files and transplanted receipts cannot establish drainage. These files contain trusted runtime metadata, not stdout/stderr or user file contents. Keep them with the original watchdog spool; no automatic receipt garbage collection is implemented.

This creates an opaque historical drain receipt, never a live guard or a renewable dispatch handle. Ordinary SQL/JSON evidence, a missing Pod, an empty cgroup, a watchdog expiry report or recovered output cannot substitute for it. Old installations with no receipt keep their previous recovery behavior.

## Automatic and explicit recovery

The queue alternates normal dispatch with eligible completion recovery, sharing its existing 1–4 job slots and shutdown join. Read-only discovery selects at most 64 IDs per page for the exact organization, storage target and original node name/UID. It includes prior node boots because a completed, persisted seal remains a historical fact. A cursor advances past missing receipts and wraps after the final page. Discovery grants no authority and never selects an old dispatch for execution.

Both discovery and the publication transaction wait until the original deadline and every issued renewal deadline have elapsed, including unacknowledged renewals. This leaves an active original worker its full result-publication window. Every publication rechecks the original arm, Candidate, dispatch and writer epoch. The completion row, execution state, writer drain and events retain their existing atomic transaction and idempotency rules. Concurrent recovery returns the same immutable receipt; historical retry cannot release a later writer epoch.

An operator can also run:

```sh
agent-computer-server execution-completion-recover --database-url-file /private/control-url --organization example --execution-id exec_example --config-file /private/execution-worker.json
```

This uses the existing private worker configuration but does not open Kubernetes credentials or call its API. Missing evidence returns `completion: null`; invalid evidence or an unexpired window fails without publishing a completion. Pod observation/deletion remains a separate cleanup path; publishing a historical receipt makes no Kubernetes call. The daemon reports `completion_recovered` or `completion_recovery_failed`, with separate summary counters. Stuck local filesystem reads retain their worker slot and can delay shutdown; a timeout is not evidence that IO stopped.

## Outcomes and remaining recovery

An execution still in `CancelRequested` may become `Cancelled` using the original completed drain. That can unblock an already admitted checkpoint stop and its Artifact worker. Cancellation confirms stopped writes; it does not undo earlier effects. A recovered `Dispatching` or `Unknown` execution remains `Unknown`, even if durable output reports success. Its physical writer may be released, but Artifact capture still rejects its unresolved execution history.

A crash before a completed receipt was durably published remains unconfirmed. The new path does not reconstruct lost process/IO handles or provide cross-node fencing, power-loss certification, arbitrary App/profile checkpoints or external-side-effect reconciliation. Full lifecycle scheduling, Browser/ComputerView and product acceptance remain pending; Computer `ready=false` and public execution support are unchanged.

## Verification

Node tests cover immutable publication, concurrent identical writers, pinned directories, partial/corrupt/transplanted receipts, permissions, links, FIFOs and size bounds. PostgreSQL tests cover deadline, organization/node/storage scoping, cursor progress, read-only discovery, WAL restart and exclusion of completed work. The disposable VM fixture kills the real gVisor execution controller at a database barrier after its seal, checks missing/corrupt evidence refusal, restarts the queue, verifies checkpoint restoration after cancellation, and retains Unknown for unconfirmed results or missing seals. Final source-bound runtime evidence is recorded with this increment after validation.
