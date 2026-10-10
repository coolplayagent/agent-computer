# 37. Stop before user dispatch

## 37.1 Supported lifecycle

`POST /v1alpha1/computers/{id}/stop` now stops a fully prepared Candidate before any user write has been dispatched. It requires a service credential with `runtime.manage`, an exact Computer `manage` grant, an idempotency key, and the current control revision from `GET /v1alpha1/computers/{id}/runtime`:

```json
{
  "expected_revision": 4,
  "request_id": "start_example"
}
```

Preparation advances the control revision; the original start admission receipt does not contain the latest revision. Queued requests still use `/start/cancel`. Preparing requests cannot use either stop path once storage preparation has started.

The stop transaction checks all writer epochs, not just the current epoch. Any user dispatch blocks this endpoint, including a completed bounded file save in an older epoch, unless a verified file-only checkpoint was published through the [Artifact path](38-workspace-artifact-checkpoints.md). An unreleased writer, a queued execution, or an unexpired human connection reporting active input also blocks stopping. Idle connections may remain connected. Close or idle active connections, cancel queued work and release/reconcile undispatched writers before retrying. Lease expiry alone does not release a writer. There is no force parameter or caller-supplied fencing proof.

## 37.2 Receipts, restart and reservations

The organization transaction lock serializes stopping with session activity, writer acquisition and dispatch. Migration 20 retains an immutable stop receipt, guards the `Prepared` → `Stopped` transition and prevents new Held leases on the stopped start request. Receipt, control revision and `computer.stopped` event/outbox commit together. Failure rolls back all three. Authorization is checked again before commit and on retries.

The receipt identifies the Computer, original start request, generation, Candidate, pinned input revision/digest, retained storage reservation, new control revision, time and event sequence. Its proof is `no_user_dispatch`. Runtime queries include `stop_receipt` when the current generation was stopped and has no active request. Replaying the original stop key returns that historical receipt; it cannot stop a newer generation. Different input with the same key conflicts.

Stopping clears the active request and releases active-Computer/Workspace, principal, CPU, memory and runtime-budget reservations. The old Candidate directory, preparation identity, writer history and its entire storage reservation are retained. No storage cleanup is performed. A subsequent ordinary `/start` admission uses the new control revision, creates a fresh generation and Candidate ID, and pins the current committed Workspace input. It never reuses the stopped directory.

Storage capacity must cover both retained and new Candidates. With the current 10 GiB Candidate reservation, a 10 GiB Volume cannot restart after a prepared stop; a 20 GiB Volume permits one replacement. This conservative accounting remains until verified garbage collection exists. A failed replacement admission leaves the prior stop intact.

## 37.3 Scope and verification

This path establishes that no user workload ever obtained dispatch authority. It does not terminate a running sandbox, drain JuiceFS writes, publish a new checkpoint or reclaim a Candidate. Computer `ready` remains false. General normal/forced stop, idle policy, post-dispatch recovery, multi-node fencing and product T01–T43 acceptance remain pending.

PostgreSQL contracts exercise WAL restart, immutable evidence, fresh authorization, late credential expiry, outbox rollback, concurrent writer acquisition, old-epoch dispatch, active human input, queued/preparing refusal, migration from existing dispatched work, replacement generation and retained storage capacity. HTTP contracts exercise scoped access, strict request bodies, Origin rejection, error envelopes, current runtime receipts and authenticated retries. Synthetic preparation/file receipts in these tests prove database authority only; this increment does not claim a new Kubernetes/CSI runtime experiment.

The 2026-10-10 [source-bound verification record](../evidence/undispatched-computer-stop-2026-10-10.json) and [logs](../evidence/undispatched-computer-stop-2026-10-10.log) record 338 passing Cargo tests, including 151 PostgreSQL cases, and eleven passing Bazel targets. Eleven new cases exercise the stop boundary. Formatting, Clippy, documentation and the existing Qualitygate policy pass.
