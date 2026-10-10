# 39. Parallel Candidates and Artifact continuation

Different Computers can now prepare and edit independent Candidates in the same Workspace. Each Computer still has one active generation, and each Candidate retains its own writer lease, directory, quota and storage reservation. Publishing uses the Workspace head's existing compare-and-swap rule; parallel admission does not grant shared writable directories or overwrite another publisher's result.

## Choose a fixed starting version

`POST /v1alpha1/computers/{id}/start` accepts an optional `input_artifact_id`:

```json
{
  "expected_revision": 5,
  "expected_spec_revision": 1,
  "max_runtime_seconds": 300,
  "input_artifact_id": "artifact_example"
}
```

Use the actual control revision from the stop/runtime receipt. The Artifact must be fully published in this Computer's Workspace, in `Committed` or `Conflict` state. A CAS conflict preserves valid immutable data; it means another publisher advanced the shared head. An unfinished, unknown, foreign-organization or different-Workspace Artifact is unavailable. Selection requires the complete current runtime graph authorization, including Workspace read/modify grants and their credential scopes. Artifact IDs, hashes and object paths do not confer authority.

Omitting the field, or supplying null, selects the current Workspace head. The immutable start receipt includes `input_artifact_id` whenever that input originated from an Artifact, even for default selection. Genesis input and legacy receipts omit it. Exact retries preserve their original fixed version and require fresh authority; changing the selector under the same idempotency key is a conflict. Admission never moves the Workspace head.

Migration 22 removes the Workspace-wide active-request uniqueness constraint while retaining the Computer constraint and all capacity ceilings. New input bindings must match the admission receipt's Workspace, revision, manifest digest and Artifact origin. Existing history is unchanged. Final graph authorization runs after transactional event/outbox and receipt writes; a failure rolls back the entire admission.

## Stop and continue a branch or conflict

The file-only [Artifact checkpoint](38-workspace-artifact-checkpoints.md) can now stop on a committed branch, a CAS conflict, or a published version that is no longer the current head. Its receipt binds that Computer's fixed Artifact version. The existing requirements remain: no declared Apps requiring capture, all prior writers verifiably drained, no dispatched process execution and no active human input. This extension supplies no new process-fencing proof.

To continue that exact checkpoint, start with its `checkpoint.artifact_id`. A default start follows the current Workspace head, which may be different. The worker restores verified remote objects into a new Candidate with independent files and a new generation. Old Candidates and their storage reservations remain retained.

For example, two Candidates start at revision 1. The first publishes revision 2; the second retains conflict revision 3. Both can stop. A default restart reads revision 2, while an explicit selection reads revision 3. Editing the latter and publishing a branch creates revision 4 without changing head 2. Selecting revision 3 or 4 does not reset its CAS base to head 2: a direct current-head publication still conflicts. To resolve the content deliberately, prepare a Candidate from current head 2, apply the chosen edits and publish against that base. The real fixture follows this explicit merge and advances the head to revision 5. No automatic merge or rebase API is introduced.

## Verification and remaining work

Six new PostgreSQL contracts cover independent inputs after WAL restart, selector idempotency, fresh authorization, unpublished and different-Workspace rejection, input-binding tampering, transactional rollback and migration preservation. The HTTP contract covers selector syntax, unavailable selection, null/default equivalence and unchanged legacy retries. The complete default suites pass 360 Cargo tests, including 168 PostgreSQL and 22 HTTP cases, and eleven Bazel targets.

The real disposable K3s/CSI + JuiceFS + S3 experiment runs simultaneous writer leases and distinct saves, rejects cross-Candidate authority, preserves both actual publisher results, restores the default and conflict versions, edits and restores a branch, and explicitly merges from the current head. Content and inode checks establish independent files. A separate Python SigV4 client reads all five Artifact manifests and six chunk objects from S3 and verifies seven complete files, including the existing empty-file case. This is single-VM component evidence; database fixtures alone do not establish storage behavior.

The [source-bound delivery record](../evidence/artifact-candidate-continuation-2026-10-10.json) and [logs](../evidence/artifact-candidate-continuation-2026-10-10.log) include final binary hashes, independent S3 readback and completed VM/private credential cleanup. Computer `ready` remains false, and T01–T43 product acceptance remains `not_run`. General process/CSI drain, forced stop, App/browser/profile checkpoints, cross-Workspace fork, private Artifact ACLs, automatic conflict resolution, Presentation and garbage collection remain pending. Retained and replacement Candidates both consume capacity.
