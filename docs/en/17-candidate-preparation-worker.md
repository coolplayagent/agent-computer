# 17. Durable Candidate preparation worker

## 17.1 Input authority and migration

Migration 8 adds immutable Workspace input versions, start-to-input bindings and durable Candidate preparation records. Creating a new Workspace through apply now records revision 1 with an explicit empty manifest. Start admission pins that committed input revision in the same transaction as its generation, Candidate and capacity reservation. Missing input is an error, not an instruction to reset files.

Legacy Workspaces and already queued requests are not backfilled. For a legacy Workspace that the operator has explicitly determined should begin empty, trusted database administration can run:

```bash
agent-computer-server workspace-initialize-empty \
  --database-url-file /private/control/database-url \
  --organization org_example --workspace-id res_actual_workspace
```

This creates only an absent input head; it never replaces an existing version. Exact repeats return the existing revision without another event. An already Preparing/Prepared legacy request prevents initialization. Cancel and readmit old queued requests after initialization; their missing input binding is never changed retroactively.

Only the initial empty input is currently published by product code. Nonempty Artifact publication, object-store read authorization and cache population remain unfinished; the worker accepts no tenant-supplied manifest or digest as authority. The storage component's separate nonempty-file tests do not establish those missing product services.

## 17.2 Claims, dispatch and completion

The trusted worker operates on a specific admitted request and an operator-bound local Volume. It rechecks the original credential ID, enabled principal, exact scopes, all runtime grants and pinned catalog dependencies at claim, dispatch and completion. A new credential does not take over the old request. A successful Volume reconciliation must match the pinned Volume revision/digest and both immutable PVC/PV UID records, including namespace UID.

The first claim freezes the filesystem UUID, Volume path, writer UID/GID, preparation request and its digest. Changing these on retry fails. One worker owns a 180-second database lease; contenders receive `busy`. Expiry before dispatch allows another execute claim. Immediately before copying, the worker durably records dispatch, moves the request from `Queued` to `Preparing`, advances control revision and rechecks the queue deadline. Cancellation can win before this transition; it cannot release a Preparing or Prepared request.

After dispatch, lease expiry or a missing acknowledgement allows **observation only**. `observe_prepared` reads the original final directory, validates its receipt and inode, reconfirms quota and synchronizes metadata. It never creates staging directories, recopies input or resets edited data. A missing publication or storage error leaves the request Preparing with `storage_unknown`; resource reservations remain held. Repeated observations may later discover the original publication. There is no automatic cleanup, alternate generation or physical-fencing claim.

A valid storage receipt binds request digest, filesystem UUID, PVC UID, exact data path, inode, manifest digest and quota. The database commits the receipt, `Prepared` state, control revision, event and Outbox together. Replaying the exact completion is idempotent; stale leases cannot replace a result. The public runtime query now accepts `Prepared`, while `ready` remains false. Prepared means files and quota were confirmed by the trusted adapter; writer ownership is implemented separately in [19 Writer leases](19-candidate-writer-leases.md); Pod creation, physical drain, driver health and Computer Ready remain pending.

## 17.3 Local worker deployment

The operator supplies a qualified full JuiceFS 1.4.1 mount with PostgreSQL metadata and S3 data, writeback disabled, and a pre-existing private Volume directory matching the recorded CSI provision. Only the prepared data leaf may later be mounted into an application. Namespace/PVC/PV IDs are checked against the journal; mapping the actual local mount/path and filesystem UUID to that Volume remains an operator deployment responsibility. Root access, configuration and ancestors must be trusted.

```bash
agent-computer-server candidate-prepare-once \
  --database-url-file /private/control/database-url \
  --organization org_example --worker-id storage_worker \
  --request-id start_actual_request \
  --config-file /private/control/candidate-worker.json
```

An illustrative configuration, with real IDs and executable digest substituted:

```json
{
  "target": {
    "volume_id": "res_actual_volume",
    "namespace_uid": "actual_namespace_uid",
    "pvc_uid": "actual_pvc_uid",
    "pv_uid": "actual_pv_uid",
    "filesystem_uuid": "actual_filesystem_uuid",
    "volume_path": "actual_csi_volume_directory",
    "writer_uid": 1000,
    "writer_gid": 1000
  },
  "mount_root": "/srv/juicefs",
  "object_cache": "/private/authorized-objects",
  "quota": {
    "executable": "/usr/local/bin/juicefs",
    "executable_sha256": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "metadata_url": "postgres://juicefs_meta@metadata.internal/juicefs?sslmode=verify-full",
    "password_file": "/private/juicefs-password",
    "timeout_seconds": 30
  }
}
```

Configuration/database/password files obey the existing private-file rules. Control and JuiceFS metadata use separate databases, roles and credentials. The command returns `"busy"`, `"prepared"` or `"storage_unknown"`. A database failure has an unknown commit outcome; retain the original request identity. The command uses no HTTP worker endpoint or tenant-supplied worker handle.

Filesystem work runs on a blocking worker thread. A slow or stuck FUSE operation can outlast the coordination lease; it is not killed or certified stopped by lease expiry. A late completion cannot commit under that lease. Reservations and private staging remain until authoritative observation or a future fenced cleanup protocol. This is one-request orchestration, not a fair queue scheduler, sustained lease watchdog or production deployment certification.

## 17.4 Verification

Seven additional real PostgreSQL cases cover input/Volume binding, migration without implicit reset, credential/grant rechecks, stale claims, observation-only takeover, cancellation, changed receipts, atomic rollback and WAL restart. Two storage cases verify side-effect-free absence observation, retained edits and binding validation. This increment had 178 default Cargo/Bazel tests; see [current totals](05-verification.md). The live test is separate.

The explicit `//crates/worker:candidate_worker_live_test` target requires a disposable root-owned Linux environment with actual Kubernetes/CSI, a full JuiceFS mount, separate control/metadata PostgreSQL databases and S3. `AGENT_COMPUTER_CANDIDATE_TEST_CONFIG` names its private test configuration. The test provisions a real Volume, invokes the operator command, checks the published inode and owner, injects a lost database acknowledgement after actual publication, and verifies observation without recopy. Another dispatched-but-absent case must remain unknown without creating files. See [the test source](../../crates/worker/tests/candidate_live.rs); building this manual target alone is not execution evidence.

On 2026-10-09 this explicit test passed on commit `f4bff87` in a disposable K3s/CSI VM with PostgreSQL 16.15, JuiceFS 1.4.1 and SeaweedFS 4.48. The two published requests reached Prepared with `ready=false`; recovery retained inode 18 and the synchronized file. The absent request remained Preparing with its reservation and `storage_unknown`. A separate fresh read-only client retrieved the 27-byte file through a recorded S3 GET. [Fixed evidence](../evidence/candidate-worker-2026-10-09.json) and [execution log](../evidence/candidate-worker-2026-10-09.log) include source/binary hashes, actual PVC/PV identities, the corrected test-fixture attempt, scripts and limitations. The VM, writable overlay and private credentials were removed after collection. This is one additional manual test, outside the 178 default tests.

Product Pod mounting, fencing, nonempty Artifact input, stop/recover, cleanup and T01–T43 acceptance remain pending.
