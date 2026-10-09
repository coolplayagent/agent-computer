# 28. Durable execution Pod identities

## 28.1 Delivered behavior

Migration 15 adds immutable Pod plans and UID observations to the [execution dispatch journal](24-execution-dispatch.md). A trusted adapter must register its compiled [Candidate startup Pod](27-candidate-pod-mounts.md) before attempting creation. Registration binds the original execution intent to the namespace UID, stable Pod name, exact manifest and a domain-separated SHA-256 digest. The manifest contains private command and storage information and is available only through the trusted Store API, without a tenant HTTP route.

`register_candidate_execution_pod` accepts the original non-cloneable dispatch attempt. It checks the fixed execution/bootstrap/storage identity, current credential and grants, writer state and original deadline. The plan and metadata-only Outbox event commit together. Only the first transaction returns an `ExecutionPodAttempt`; concurrent or identical retries return `DispatchAlreadyStarted`, including after an uncertain commit response. The new attempt retains the original monotonic deadline. It does not renew the writer or start a command.

The Store checks identity, bounded manifest size and digest consistency. The Kubernetes adapter remains responsible for compiling and validating the full Pod security configuration, observing the actual deployment/storage objects and checking the real prepared filesystem. A synthetic manifest accepted by the database is not qualified for Kubernetes creation.

## 28.2 Observation and startup binding

After verifying the actual Pod against the fixed plan, the trusted adapter calls `record_candidate_execution_pod`. The first observed UID is immutable; the same UID is an idempotent retry, while a different UID or plan digest fails. Observation and its Outbox event are atomic. An observation can still be recorded after cancellation or expiry, since it supplies recovery information without granting authority. The adapter must never create a replacement after loss of a creation response.

Once a plan exists, [startup authorization](25-execution-startup.md) requires its recorded UID. Both the Store method and a PostgreSQL trigger reject an unrecorded or different UID. A plan cannot be registered after a startup grant has already committed. Migration 14's low-level trusted component API remains available for dispatches without a plan; it is not a public runtime endpoint. Migration 15 preserves historical grants without inventing a Pod plan or UID observation.

`candidate_execution_pod` returns a read-only recovery snapshot and verifies its digest and identity. It never reissues a creation attempt or grant. Reading or cloning this snapshot cannot authorize mutation. Actual adapter integration must observe the original name/UID and use conditional deletion for cleanup; absence, a Pod UID, a process report or deletion alone is not a writer-drain certificate.

## 28.3 Verification and remaining work

Seven PostgreSQL contracts cover concurrent registration, dropped responses and WAL crash recovery, immutable UID selection, startup binding, transplanted identity/bootstrap/storage rejection, SQL trigger backstops, plan/observation Outbox rollback, revoked authority, expiry and migration compatibility. Inputs and observations are synthetic adapter fixtures; these tests do not create Kubernetes Pods. The default workspace now has 285 tests, including 128 PostgreSQL cases.

Run the normal Cargo and Bazel suites using the PostgreSQL environment described in [08 persistence](08-persistence.md). Formatting, Clippy, paired documentation checks and the full existing Qualitygate policy are separate required checks.

This increment supplies the durable boundary for the runtime worker. Loading pinned runtime inputs, connecting database authorization to actual Pod creation/attach, an independent watchdog, physical fencing, output objects and accepted completion remain pending. Unknown execution outcomes retain a Draining writer. T01–T43 remains `not_run`.
