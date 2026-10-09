# 16. Durable Computer start admission

## 16.1 Delivered scope

Migration 7 adds durable start requests and Computer control counters. The service can admit a request into `Queued`, return its original receipt after a lost response, inspect current admission state, and cancel an undispatched request. The capability `computer.start_admission` is `control-plane`; `computer` remains `unsupported`.

An admission does not select an Artifact input manifest, initialize an empty Workspace, prepare files, grant a modification lease, create a Pod or report Ready. In particular, an existing Workspace is never silently treated as empty. Connecting the [Candidate preparer](14-candidate-storage.md) to an authorized worker, input publication, dispatch fencing and driver health checks remain necessary. No runtime start worker is exposed in this increment.

## 16.2 Atomic admission

`POST /v1alpha1/computers/{id}/start` requires a service Bearer credential, one `Idempotency-Key` header and uncompressed `application/json`:

```json
{
  "expected_revision": 1,
  "expected_spec_revision": 1,
  "max_runtime_seconds": 300
}
```

`expected_revision` is the Computer control revision, initially 1. `expected_spec_revision` selects the current Computer definition head. Generation starts at 0; successful first admission allocates generation 1 and control revision 2. These are three independent counters. Later cancellation advances the control revision without resetting generation. A subsequent start allocates a new generation and Candidate ID. Both IDs and organization/principal come from the server; extra fields, including a caller-supplied generation, Candidate, checkpoint or actor, are rejected.

One PostgreSQL transaction holds the organization stream lock, rechecks credential/principal state, verifies the complete required runtime grant set, pins the immutable resource graph, checks capacity, and writes the request, counters, reservations, idempotency receipt, event and Outbox. Credential expiry is checked again immediately before commit. Failed admission rolls all of these back. Scope and grant revocation participate in the existing [authorization locking contract](15-runtime-authorization.md).

The graph includes exact resource revisions and digests, specs and transitive dependencies. A changed noncatalog head cannot rewrite a captured version; enabled catalog references must still match their captured revision/digest. Conflicting versions of one resource are rejected. Capture is bounded to 256 resources, 1 MiB and 32 distinct authorization requirements. The snapshot contains no bearer token; the request separately binds the original credential ID. Replaying with another valid credential for the same principal does not transfer that binding.

Required permissions are Computer `activate`; main Workspace `read` and `modify`; each additional referenced Workspace `read`; each App `app.use` and bounded `activate`; each private browser profile `app.use`. Each also requires its corresponding credential scope. Definition manage/reference permission grants no runtime authority. No arbitrary shell execution is admitted by this API.

## 16.3 Reservations and queue limits

These conservative platform ceilings are enforced in the database transaction; callers cannot override them:

| Reservation | Ceiling |
| --- | --- |
| Outstanding requests per organization | 64 |
| Outstanding requests per principal in the organization | 8 |
| Outstanding requests per Computer / writable Workspace | 1 / 1 |
| Reserved CPU per organization | 64,000 millicores |
| Reserved memory per organization | 131,072 MiB |
| Reserved Candidate storage per organization | 1 TiB |
| Candidate storage per request | 10 GiB |
| Reserved Candidate storage per Volume | Pinned Volume `quotaBytes` |
| Sum of requested run durations per principal | 86,400 seconds |

Each request sums the distinct pinned Sandbox resources. Both `Queued` and `Preparing` consume reservations. An undersized Volume is rejected without rounding or changing its declaration. Capacity exhaustion returns 429 and creates no request. These reservations are admission accounting, not measurements of actual Kubernetes scheduling, filesystem usage or billed cost. Fair scheduling, operator budget configuration and cumulative historical billing remain pending.

The database assigns a 15-minute queue deadline. A future dispatcher must reject expired requests and revalidate the original credential, grants, budget and input before any effect. There is currently no automatic queue expiry worker; cancellation releases an undispatched reservation. Neither a deadline nor a revoked credential releases a possibly dispatched writer. The granted run duration is a bound to enforce when runtime execution is implemented; admission itself starts no elapsed-runtime watchdog.

## 16.4 Receipts, reads and cancellation

A successful admission returns 202 with stable `request_id`, `candidate_id`, generation, control/spec revisions, snapshot digest, reserved resources, deadline and event sequence. Its state is `Queued`, with reason `awaiting_runtime_preparation`. Repeating the same key and input rechecks current permissions and returns the original receipt. Changed input returns 409; a retired key returns 410. A different key cannot allocate a second active generation. A retry after cancellation still returns the original admission receipt and never reactivates it.

`GET /v1alpha1/computers/{id}/runtime` requires Computer `read` and `runtime.read`. It returns current revision, generation, active request ID and start state; `ready` is always false at this stage. An untouched Computer reads as revision 1, generation 0, without creating runtime rows. The endpoint does not expose the private graph or credential ID.

`POST /v1alpha1/computers/{id}/start/cancel` requires Computer `manage`, `runtime.manage`, an idempotency key and:

```json
{
  "expected_revision": 2,
  "request_id": "start_actual_request"
}
```

Cancellation must name the active request at the current revision. It atomically changes only `Queued` to `Cancelled`, releases its reservations, advances control revision and publishes an event/Outbox entry. Request identity, snapshot and generation history remain immutable. A `Preparing` request returns 409 and retains reservations; its isolation and cleanup require a separate worker protocol. This cancellation API is not Computer stop/recover. Browser Origin is rejected on all three endpoints. Missing or inaccessible resources share 404; invalid credentials return 401, missing scopes or excessive grant duration 403, and stale definition revision 412. See the [OpenAPI contract](../../schemas/openapi-v1alpha1.json).

## 16.5 Verification and remaining work

Eleven real PostgreSQL cases cover upgrade preservation, concurrent retries, immutable snapshots, grant/scope separation, catalog disable, revocation while waiting for admission, Volume/Workspace contention, principal queue limits, rollback on receipt/Outbox failure, late credential expiry, WAL crash recovery and cancellation after preparation begins. One HTTP case covers request parsing, server-owned fields, revisions, replay, read, cancellation, scope and Origin rejection. These are component contracts; T01–T43 remain `not_run`.

Next work must bind an immutable input manifest and real Volume identity to the allocated Candidate, implement durable preparation claims and receipts, recheck writer authority at dispatch, and integrate Pod/driver observation. Physical fencing, stop/recover, checkpoints, lease watchdogs and full runtime acceptance remain unfinished.
