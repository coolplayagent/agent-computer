# 19. Durable Candidate writer leases

## 19.1 Ownership and authority

Migration 10 adds one modification-lease head per prepared start request, immutable ownership epochs, a dispatch journal and drain proofs. A lease binds the current Computer generation, Candidate, original ConnectionSession and digest of the validated preparation receipt. It does not allocate a new Candidate, copy files or set Computer Ready. `candidate.writer_leases` advertises `control-plane` at the capability endpoint.

Acquisition and each renewal/dispatch require the exact credential that created an Active connection. That connection must request and currently hold Computer `connect`, `read` and `modify`, intersected with `runtime.connect`, `runtime.read` and `runtime.modify` credential scopes. The caller also needs independent Workspace `read` and `modify` grants. Definition creation, Computer `manage` and membership in the same organization imply none of these grants. Prepared state, generation, Candidate identity and pinned catalog availability are rechecked. A collaborator uses their own grants and credential; the original starter's credential does not become their authority.

The default and maximum lease duration is 30 seconds, with renewal recommended every 10 seconds. Requested durations must be 1–30 seconds. Database time determines expiry, capped by the connection's fixed deadline. Renewal never shortens an existing deadline and never extends the connection. The head's revision changes on writes; its ownership epoch advances only when a Released head is acquired again. A second owner cannot acquire a Held or Draining head, even after its deadline.

## 19.2 HTTP contract

All endpoints require the exact original connection credential and `runtime.connect`. Acquisition and renewal additionally require `runtime.read`/`runtime.modify` and the grants above. Browser `Origin` requests remain rejected pending browser authentication. Every POST requires `Idempotency-Key` and strict JSON; owner/principal identities and caller-asserted stopped flags are rejected.

| Endpoint | Result |
| --- | --- |
| `POST /v1alpha1/computers/{id}/leases` | 201 with current modify-lease metadata |
| `GET /v1alpha1/leases/{id}` | 200 with the current own-epoch view |
| `POST /v1alpha1/leases/{id}/renew` | 200 after exact owner/generation/epoch/revision checks |
| `POST /v1alpha1/leases/{id}/release` | 200 Released with `no_dispatch` proof, or 202 Draining when dispatch is recorded |

Acquisition uses IDs from the current start and connection responses:

```json
{
  "scope": "modify",
  "connection_session_id": "connection-example",
  "candidate_id": "candidate-example",
  "generation": 1,
  "duration_seconds": 30
}
```

Renewal wraps an exact lease command; release uses the inner `lease` object directly:

```json
{
  "lease": {
    "connection_session_id": "connection-example",
    "generation": 1,
    "epoch": 1,
    "expected_revision": 1
  },
  "duration_seconds": 30
}
```

Lease metadata includes IDs, generation, epoch, revision, state, expiry/check times, `dispatch_recorded` and nullable `release_proof`. It exposes no filesystem paths or storage credentials. These IDs and JSON are not reusable IO capabilities. Exact retries return the current view without repeating writes or extending expiry; replaying an old key after a later epoch fails. A replacement credential cannot inspect or take over the old owner, even for the same principal. Conflicts return 409; inactive connections return 410; inaccessible owners/resources return 404; authentication/scope failures return 401/403.

## 19.3 Draining and recovery

Expiry or loss of authority derives Draining in a current read. Closing a connection or revoking a relevant Computer/Workspace grant also atomically persists Draining for Held leases and records the changed count in the enclosing event. Regranting does not revive that epoch. Credential revocation and principal disable prevent further authenticated requests; trusted reconciliation can still lower authority. Explicit release accepts a valid original credential after connection close or grant loss so the owner can finish a safe handoff.

The trusted Rust `begin_candidate_writer_dispatch` boundary commits one dispatch ID and normalized-operation digest before returning a non-cloneable permit bound to the preparation receipt. It admits **one dispatch per ownership epoch**. Repeats, including after a lost acknowledgement or WAL restart, never reissue the permit. There is no HTTP dispatch endpoint or executor consuming this permit yet. A future executor must enforce the deadline locally and record actual drain/fence evidence.

Release first removes future admission. Only an epoch with no committed dispatch can receive immutable `no_dispatch` proof and become Released. Dispatch/proof inserts lock the same lease head; SQL guards reject stale epochs, forged zero-dispatch proof after dispatch, history mutation and unproved release. This proof describes the journal, not process termination. Once a dispatch is recorded, release, expiry, connection close and reconciliation retain Draining. No caller flag, timeout or database lease alone permits takeover. Compute/storage reservations remain held.

The operator can perform one reconciliation using a private database URL file:

```sh
agent-computer-server writer-lease-reconcile \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --lease-id lease-example
```

This command leaves live leases alone, releases only zero-dispatch invalid/expired ownership, and returns Draining for recorded dispatch. It is not a supervisor, periodic scheduler or physical fence. Mutations hold the organization lock and commit lease history, idempotency receipts, events and Outbox together. Final authorization/expiry checks roll back late failures.

## 19.4 Verification and next boundary

Twelve new PostgreSQL cases cover WAL recovery, competing connections, stale commands, deadlines, replay, monotonic epochs, independent grants, collaborator credentials, revocation, immutable dispatch/proofs, pinned catalog drift, Outbox rollback, late credential expiry and migration checksums. Two HTTP cases cover lifecycle, 200 versus 202 release, strict request shapes and credential isolation. The existing independent TCP-process test now also acquires a lease, closes its connection and invokes the reconciliation command.

These tests run real PostgreSQL with synthetic preparation receipts; they verify control authority, not physical IO cessation. Default Cargo/Bazel suites contain 204 tests. Supervised file/process writers, watchdogs, physical draining/fencing, Workspace Pod mounts, GUI control leases, Artifact publication and full Computer execution remain pending. T01–T43 runtime acceptance remains `not_run`. See [17 Candidate preparation](17-candidate-preparation-worker.md), [18 Connections](18-connection-sessions.md) and [04 Full plan](04-implementation-plan.md).
