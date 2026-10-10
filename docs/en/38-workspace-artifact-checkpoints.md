# 38. Workspace artifacts and file-only checkpoints

## 38.1 Publish a fixed file version

After bounded file saves have released every writer, `POST /v1alpha1/workspaces/{id}/artifacts` permanently seals the current Candidate. Supply an idempotency key and the original start input identity, together with the current Computer control revision:

```json
{
  "request_id": "start_example",
  "expected_revision": 4,
  "base_revision": 1,
  "base_manifest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "publish_current": true
}
```

The service credential needs `runtime.publish`, `runtime.read` and `runtime.modify`; resource grants must include Workspace read/modify/publish and Computer read/modify. These checks apply again during publication. The response is `202` with an artifact `commit_id` and `Capturing` state, which acknowledges durable work, not a completed publication. `GET /v1alpha1/artifacts/{id}` returns current metadata. `/manifest` returns null until publication, then the fixed relative file manifest. Both reads require current Workspace read permission and `runtime.read`; neither exposes S3 credentials, object keys or signed URLs.

Migration 21 serializes sealing with writer acquisition under the organization lock. `Prepared` becomes `Sealing`; the Candidate cannot reopen for writes. Every writer epoch must be Released. Every prior dispatch must have a recorded, closed bounded-file completion and `bounded_file_drained` proof. A queued or dispatched process, unknown writer IO, or an unreleased lease prevents sealing. Process exit, cgroup disappearance, Pod deletion and lease expiry do not establish JuiceFS drain.

## 38.2 Capture, upload and retry

The trusted operator runs:

```sh
agent-computer-server artifact-publish-once \
  --database-url-file /private/control-url \
  --organization example --worker-id publisher \
  --commit-id artifact_example --config-file /private/artifact-worker.json
```

The private JSON configuration has `storage` (the existing file worker's `target` and `mount_root`), `spool` (an existing private directory), and `objects` (the S3 configuration described in [36](36-durable-execution-outputs.md)). S3 uses the separate `artifacts/v1` prefix, with organization, commit identity and content hash. Keep credentials and spool outside workload mounts. The exact storage target must match the prepared Candidate's durable binding.

Capture walks open directory/file descriptors without following links or crossing mounts. It accepts regular files and directories, retains empty directories and executable bits, and rejects symlinks, hard links, special files, special file permission bits and leftover file-save staging names. Identity, size, timestamps and directory entries are checked around traversal. Each file is fsynced and read in bounded 4 MiB chunks; its concatenated SHA-256 is also retained. Chunks and the manifest are durably spooled before their capture identity is recorded. Manifest limits remain 10,000 entries and the object client's bounded manifest size; the Candidate reservation remains 10 GiB. This chunk format is not S3 multipart-upload API certification.

The worker uploads immutable objects and reads every chunk and manifest back in full, checking byte counts, chunk hashes and complete file hashes. A 300-second worker lease is renewed while the operation runs. Finalization rechecks the lease, control revision, generation, publisher credential, grants and pinned catalog dependencies; the lease and credentials are checked after event/outbox work too. The new input version, optional default pointer, artifact result, `Sealed` state and event/outbox commit in one transaction.

Failed capture/upload/database work retains the sealed Candidate and completed spool objects. Retry the same operator command; recorded capture is reused, never silently replaced. A failed worker releases only its own lease; an interrupted worker's lease can expire before another claim. If the publisher credential expires, the original principal can repeat the public POST with the same key and input using a newly authorized credential. This invalidates old worker authority without changing the captured content. No error path reopens the Candidate for mutation.

With `publish_current=true`, publication compares the Workspace head with `base_revision`. A matching head advances; a changed head produces `Conflict`, preserving the fixed artifact/input version and Candidate while leaving the other publisher's head intact. With `false`, a committed branch leaves the head unchanged. Branches currently inherit the Workspace read ACL. Branch continuation, explicit fork/rebase, private per-artifact ACLs and conflict resolution are not yet exposed; those sealed Candidates remain retained.

## 38.3 Checkpoint stop and restore

After a successful current-head publication, the existing [stop endpoint](37-undispatched-computer-stop.md) accepts a `Sealed` file-only Computer. Its `artifact_checkpoint` receipt binds the artifact, input revision and manifest hash, Computer spec hash and original runtime snapshot hash. It has empty App-state and unfinished-execution lists. A Computer declaring any Apps cannot use this checkpoint path until the required App/profile capture exists. Branches, conflicts, a subsequently changed Workspace head and active human input also block checkpoint stop.

Stopping retains the old Candidate and its full storage reservation. A normal subsequent start creates a new generation and Candidate, pinning the current Workspace input. The Candidate worker's optional `artifacts` configuration supplies the same S3 store for restoration. It verifies remote chunks and the manifest, builds private digest-addressed cache files, then materializes independently writable files and rechecks their complete hashes. It never copies the retained old Candidate. Already prepared or previously dispatched preparation observations do not need to download objects again. Capacity must cover retained and replacement Candidates; garbage collection is pending.

This increment also corrects the prior stop receipt's event sequence to match the committed `computer.stopped` event. Computer `ready` remains false. Arbitrary-process storage fencing, general/forced stop, App/browser state, checkpoint driver-version negotiation, artifact GC, Presentation and full T01–T43 product acceptance remain pending.

## 38.4 Verification

PostgreSQL and HTTP tests cover sealing/acquisition races, unknown dispatch, immutable history, branch/CAS conflicts, fresh authorization, credential replacement, lease expiry during outbox writes, rollback, migration, checkpoint stop, WAL restart and new input admission. Filesystem tests cover multi-chunk files, independent inodes, empty directories, mode preservation, corruption and concurrent mutation. Database fixtures with synthetic storage receipts establish authority only.

The real disposable single-VM experiment uses K3s/CSI, JuiceFS, separate control/metadata PostgreSQL databases and SeaweedFS S3. It saves files through the bounded gateway, rejects wrong S3 credentials, injects a publication outbox failure, retries in a new process after removing the local spool, stops on a checkpoint, and restores a new Candidate after clearing the cache and deliberately changing the old directory. It checks original content, executable permission and independent inodes. This is component evidence, not multi-node fencing, power-loss, HA or full Computer certification. Source hashes, independent S3 readback and cleanup are recorded with the delivery evidence.

The 2026-10-10 [source-bound record](../evidence/workspace-artifact-checkpoints-2026-10-10.json) and [logs](../evidence/workspace-artifact-checkpoints-2026-10-10.log) contain 354 passing Cargo tests (162 PostgreSQL, 22 HTTP), eleven passing Bazel targets, the final real VM run, independent S3 readback and completed cleanup. Formatting, Clippy, OpenAPI and the existing Qualitygate policy pass.
