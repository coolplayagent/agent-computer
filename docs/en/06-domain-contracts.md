# 06. Domain state contracts

## 06.1 Implemented rules

`crates/core` implements in-memory domain values and pure state transitions. It performs no I/O and is not a persistent service or an alternative local runtime.

| Module | Implemented constraints | Specification |
| --- | --- | --- |
| `identity` | Distinct ID types, independent revision/generation/lease epoch, checked overflow, SHA-256 digest format | D02 |
| `computer` | Stable Computer/Workspace/organization identities, start generations, health receipts, activity protection, draining/recovery/tombstones | D02, D04 |
| `lease` | Exclusive ownership state, maximum 30-second TTL, expired-action rejection, new epoch after confirmed draining, organization/connection binding | D06 |
| `execution` | Dispatch, actual outcome, cancellation requests, original Unknown facts and separate reconciliation | D08, D11 |
| `idempotency` | Organization/principal/operation/key scope, original execution reuse, changed-input conflicts, retired-key tombstones | D08 |

IDs currently accept 1–128 ASCII letters, digits, underscores, or hyphens. Digests accept `sha256:` followed by 64 hexadecimal digits. Domain types do not generate globally unique IDs, calculate hashes, or verify actual content. Future transport schemas must state their format constraints explicitly.

## 06.2 Trust boundaries

Services must authorize mutations, lock/check state inside a database transaction, and atomically persist revisions, idempotency records, events, and Outbox entries. Rust `&mut` only serializes changes to one local object; it does not establish cross-process exclusion. Storage must enforce one authoritative lease per Computer generation and scope. Lease decisions use database time.

`RuntimeFence`, `DrainEvidence`, and `Receipt` reference evidence verified by trusted adapters. Never trust them by directly deserializing client input. Constructing these values does not establish process termination, released input, isolation, or durable results. Adapters must verify the evidence's organization, resource, generation, input, provenance, and persistence.

Normal stop protects human/Presentation activity while allowing Draining to wait for executions. Idle stop requires every activity category to be clear. Force stop still requires actual fencing and an explicit loss record for unsaved state; the latest valid checkpoint is retained.

Unknown prohibits redispatch and ordinary late finish callbacks. A separately authorized `reconcile` appends a single resolution while retaining `status: Unknown`; readers present the resolution alongside the original fact. Cancellation requests leave Running unchanged until a durable receipt establishes the actual outcome, including completion/cancellation races.

## 06.3 Verification

`bazel test //crates/core:core_contracts_test` exercises stage 02 domain constraints, including unchanged state after rejection. Tests derive from specified fault conditions; external runtime acceptance T01–T43 remains unexecuted. Service tests must later add concurrent transactions, crash recovery, forged identity/evidence, and actual worker failures.
