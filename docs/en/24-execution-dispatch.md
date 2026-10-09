# 24. Durable execution dispatch journal

## 24.1 Delivered behavior

Migration 13 connects the [23 execution queue](23-execution-admission.md) to the exclusive writer dispatch journal. A trusted Rust store client can commit one fixed dispatch intent, recover its inputs after a lost response, request cancellation after commitment, and mark an uncertain outcome. The transaction rechecks the original connection credential and current permissions. No Kubernetes Pod or process is started by these methods. The service continues to report `execution: unsupported` and `execution.admission: connection-queued`.

| State | Meaning | Writer handoff |
| --- | --- | --- |
| Queued | Input reserved; no dispatch intent | Cancellation can free the reservation |
| Cancelled | An undispatched reservation was cancelled | Existing zero-dispatch/file rules apply |
| Dispatching | A dispatch intent committed; the external effect may or may not exist | Blocked |
| CancelRequested | A cancellation request committed after dispatch | Draining; blocked |
| Unknown | Dispatch result or continued authority cannot be confirmed | Draining; blocked |

`dispatch_started: true` means that the durable intent committed. It does not establish process start, successful completion, termination or storage drainage. There is no Succeeded/Failed/Running projection from a caller-supplied status.

## 24.2 Transaction and recovery contract

`begin_candidate_execution_dispatch(organization, execution_id, expected_revision)` is a trusted worker method, not an HTTP authentication mechanism. It derives the principal/credential from the previously admitted connection. Credential and principal share locks serialize revocation; all connect/read/modify scopes, the active connection, five independent Computer/Workspace grants, current generation/Candidate, preparation digest and pinned catalogs must still be valid. A collaborator uses their own connection authority even if the original start credential was revoked.

The transaction consumes the queued reservation, inserts an immutable dispatch intent and matching writer journal, increments execution and writer revisions, and emits an event/Outbox entry. The fixed execution ID is also the dispatch identity. The digest binds organization, execution, lease/epoch, immutable input/binding digests, start time and deadline. SQL deferred checks reject a half-written intent or journal; immutable triggers reject replacement, deletion, return to Queued or cancellation as undispatched. Final authorization and expiry checks occur after event writes. Any failure rolls back all these changes.

Only the winning transaction returns `ExecutionDispatchAttempt`. This Rust value is neither cloneable nor deserializable. Its local monotonic budget decreases while the caller holds it. Repeating the begin call returns `DispatchAlreadyStarted`, even after a dropped result, crash or lost acknowledgement. `candidate_execution_dispatch` returns fixed inputs and current metadata for recovery only; it never issues another attempt. Recovery data is a trusted internal surface containing command bytes and storage bindings; HTTP metadata and events omit those values.

The absolute deadline remains the queue deadline captured at admission. Writer renewal, recovery and retries cannot extend it. The local budget includes time spent waiting for the database transaction. It is only an adapter input, not a runtime enforcement boundary: a future adapter must revalidate at actual process start and enforce the absolute expiry from outside the workload. A Pod delayed in scheduling cannot start a fresh relative budget.

## 24.3 Cancellation, expiry and uncertainty

The existing cancel endpoint retains execution revision CAS and idempotency. Before dispatch it returns Cancelled. After dispatch it returns CancelRequested and atomically marks the writer Draining, without a drain proof. Repeating that request returns current metadata. An already Unknown execution remains Unknown; the cancellation receipt cannot settle its outcome.

A metadata read or trusted reconciliation converts a dispatched execution to Unknown if the fixed deadline expires, the connection closes, permissions or credentials are lost, or the Candidate/catalog binding becomes invalid. A trusted worker can explicitly call `mark_candidate_execution_unknown` after an unconfirmed external operation. Repeated uncertainty reports do not repeat events. All uncertainty/cancellation state changes and writer draining roll back together if their events cannot commit.

Neither path releases ownership. An execution intent blocks another dispatch, a later writer epoch, a zero-dispatch proof and bounded-file completion/drain evidence. Restoring permissions does not resurrect the intent. Physical runtime reconciliation remains necessary to prove drainage and allow handoff.

## 24.4 Verification and remaining work

Eleven new real PostgreSQL cases cover concurrent begin/cancel, one-winner dispatch, WAL recovery, exact retry, immutable identity, partial-transaction rejection, current authority and collaborator identity, expiry after renewal, Outbox/late-expiry rollback, cancellation rollback, blocked file evidence/handoff and migration 13. One HTTP case verifies Dispatching → CancelRequested → Unknown, revision conflict and credential isolation. The fixtures use synthetic Candidate preparation receipts and launch no processes.

The default workspace has 254 tests: 115 PostgreSQL, 20 server, and the existing 119 other cases. Cargo tests, fmt/Clippy, Bazel build/test, OpenAPI validation and bilingual documentation checks pass. Full Qualitygate checks the existing line-ending policy only. T01–T43 remain `not_run`.

Still required: an actual Candidate Pod/worker, trusted supervisor delivery, storage mount identity checks, process-start authorization, an external watchdog and physical fencing, bounded output objects, accepted completion and independent background lifetimes. The [22 suspended-init fault](22-sandbox-supervisor.md) remains unresolved by a local supervisor report or Kubernetes status. This journal deliberately retains uncertain ownership until that runtime evidence exists.
