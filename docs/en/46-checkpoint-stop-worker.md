# 46. Durable checkpoint stop and Artifact worker

A file-only Computer can now save its current files and stop through one durable request. The request seals a drained Candidate; the background Artifact worker captures and verifies immutable S3 content, then publishes the checkpoint and records stopping in one database transaction. An accepted request is still pending work. Computer Ready, App/profile capture and general process drain remain separate requirements.

## Request and authority

```http
POST /v1alpha1/computers/{id}/checkpoint-stop
Authorization: Bearer <credential>
Idempotency-Key: <stable-key>
Content-Type: application/json

{"request_id":"start_example","expected_revision":4,"publish_current":true}
```

The caller needs Computer read/modify/manage and Workspace read/modify/publish grants, with matching credential scopes. The server derives the original Workspace input revision and manifest from the admitted start. Callers cannot supply storage facts, stop receipts or a force flag. A `202` response contains the Artifact `commit_id`, `state` and `stop_after_commit=true`. Query the existing Artifact endpoint or Computer runtime; only a committed `stop_receipt` confirms stopping.

Admission requires a Prepared Candidate with every writer released and every dispatched effect covered by the existing drain/completion rules. Unknown execution outcomes still block a checkpoint, including those with a released physical writer. Every declared App must be absent because App/profile capture is not implemented. Another principal's unexpired Active connection, or active human input even from the requesting principal, blocks normal stop. The requesting principal's idle connection is allowed. Active use returns `409 active_use` without disclosing other identities. New active use after admission blocks finalization; the sealed Candidate and captured bundle remain available for a later retry.

`publish_current=true` uses the original Workspace base revision for CAS. A changed head leaves a retained Conflict artifact and stops with that exact saved checkpoint; it never overwrites the new head. `false` saves a branch checkpoint. Normal subsequent start selects the current Workspace head; `input_artifact_id` selects the checkpoint explicitly. Both allocate a new generation and independent Candidate. Old Candidate files and reservations remain retained.

The request, original principal and `stop_after_commit` mode are immutable. The original principal may repeat the same key and input with a newly authorized credential; this invalidates the old worker lease without changing captured files. Permission checks occur during claims, renewal and finalization, including after the event/outbox writes. Replaying completed work returns its historical result and cannot stop a newer generation.

## Publication and stopping

Migration 26 adds the stop mode and pending-work index. The Artifact worker reuses the existing capture and complete object verification rules. It changes Sealing to Sealed, builds the existing file-only checkpoint receipt, changes the start to Stopped and clears the active Computer request in the same transaction. Artifact and stop events receive consecutive sequence numbers. A deferred database constraint rejects checkpoint publication without the matching stop receipt. A failed event, expired lease or lost authorization rolls back publication, pointer advancement and stopping together; immutable uploaded objects remain safe to retry.

The existing explicit Artifact publication and immediate stop endpoints retain their roles. This operation requires writers already drained; it does not cancel running work, wait for active use to end before accepting, or implement manager force-stop. The [accepted execution completion](42-accepted-execution-completion.md) proof can make successful/failed/cancelled execution history eligible for capture. An empty cgroup, removed Pod or expired lease alone cannot do so.

## Continuous worker

```sh
agent-computer-server artifact-worker --database-url-file /private/control-url --organization example --worker-id publisher --config-file /private/artifact-worker.json --concurrency 2 --poll-ms 1000
```

The private configuration is the same as [artifact-publish-once](38-workspace-artifact-checkpoints.md): `storage` holds the qualified target and existing mount root, `spool` is outside workload mounts, and `objects` holds the S3 client configuration. The daemon opens these local resources before discovery. It does not provision or mount Volumes. Recorded capture may still be recovered with the one-shot command without recapturing files.

Read-only discovery selects pending commits for the exact organization and full storage target, excluding live worker leases and completed commits. Every job still obtains the original atomic claim and current authorization. The worker rotates eligible IDs, uses a five-second local cooldown after busy/unconfirmed work, and schedules at most one job per poll. Shared bounds are 1–4 concurrent scheduled jobs and 250–5000 ms polling; daemon defaults are one slot and 1000 ms.

SIGTERM/Ctrl-C stop scheduling and join scheduled work, including claims waiting on the database. If renewal fails while a filesystem capture thread is running, its slot stays occupied until that thread finishes; its result is not published under the lost lease. Async upload/finalization can fail while retaining the same immutable capture for retry. Forced termination retains sealed storage and durable identities. Synchronous logging and blocked filesystem IO can delay shutdown. The [systemd example](../../deploy/systemd/agent-computer-artifact-worker.service) uses a 360-second stop timeout; this is not a physical IO deadline or a deployment-wide concurrency bound.

## Evidence and remaining scope

Tests cover authority, normal active-use checks, atomic stop/publication, late expiry, outbox rollback, credential replacement, original-input selection, WAL restart, historical retry, migration, exact discovery and competing claims. A thread barrier verifies that lost renewal never detaches capture. HTTP tests exercise strict request bodies, admission and authorized metadata.

The disposable VM fixture runs the real daemon after failed S3/publication attempts and deletion of its local spool. It sends SIGTERM while a claim waits on a SQL lock, verifies the joined checkpoint-stop result and confirms an empty restart. A separate real gVisor execution fixture preserves accepted execution files through checkpoint-stop and restoration into a fresh Candidate/cache. Independent SQL, S3 and fresh read-only JuiceFS checks verify the results.

[Source-bound component record](../evidence/checkpoint-stop-worker-2026-10-10.json) · [validation log](../evidence/checkpoint-stop-worker-2026-10-10.log)

These are single-node component results. Active process draining, force-stop, App/browser checkpoints, full lifecycle scheduling, cross-node fencing, GC and product certification remain incomplete. Computer `ready=false`, public execution remains unsupported, and T01–T43 remain `not_run`.

The validation record also retains intermittent startup/watchdog refusals from earlier runs: no startup grant or user mutation was accepted. Their exact transient causes were not exposed. The final run used a fresh private watchdog spool after confirming every retained old deadline had expired. A completed component run does not establish startup reliability or throughput.
