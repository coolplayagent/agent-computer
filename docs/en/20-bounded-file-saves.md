# 20. Bounded Candidate file saves

## 20.1 Implemented path

The trusted `candidate-file-save-once` worker now consumes a connection-owned modify lease to save one file. This connects [19 Writer leases](19-candidate-writer-leases.md) to actual storage IO. The capability endpoint advertises `candidate.file_save: trusted-worker`; HTTP upload/download, directory listing and the human file UI remain pending.

Before dispatch, the worker validates the current owner/generation/epoch/revision and compares its whole configured Volume target with the immutable preparation binding. The mounted JuiceFS filesystem must match the recorded UUID/PVC and synchronous-upload configuration. At the file boundary it checks the private preparation receipt, Candidate inode, data UID/GID and mode. A caller cannot select an absolute host path. This uses the same independent Computer and Workspace grants as lease acquisition.

Each save supports at most 1 MiB of binary content, and the existing file must also fit that bound. Paths use the existing relative-path limits: at most 1024 UTF-8 bytes, 32 components and 255 bytes per component. Parents must exist. Dot/dot-dot components, empty components, backslashes, NUL, symlink traversal, hardlinks, FIFOs, directories as files and mount crossings are rejected. Names beginning `.agent-computer-write-` are reserved.

## 20.2 Compare and replace

`expected: null` requires an absent target. Otherwise `expected` contains the exact previous `sha256`, `size` and `executable` values. This is a content/mode precondition; identical content and executable state have the same version. A conflict returns the current version and leaves the target untouched. It never silently overwrites a different version.

The adapter opens files relative to pinned directory descriptors, writes a new exclusive staging file, sets ownership/mode, synchronizes it, rechecks the old version, renames atomically and synchronizes the parent directory. It reads back the resulting bytes/hash/mode before reporting Applied. Replacement uses a new inode, so an existing reader retains its original open file. This gateway assumes every writer uses the exclusive Candidate lease; it does not isolate arbitrary processes already holding a writable mount.

The dispatch stores the digest of the normalized file intent and preparation receipt before IO. The permit is consumed exactly once and captures a conservative monotonic deadline that includes dispatch round-trip time. No process, task or writable file descriptor escapes the synchronous adapter. The worker awaits the actual blocking call; cancelling its async future is not drain evidence. A blocked FUSE operation may outlast the deadline, while ownership remains unavailable for takeover.

## 20.3 Command and results

Use private configuration, credential and request files. The configuration contains `mount_root` and the exact `target` from [17 Candidate preparation](17-candidate-preparation-worker.md). It contains no object-cache or quota configuration: the prepared Candidate already has its quota. The credential is the original connection credential. Requests are strict JSON, at most 5 MiB including the binary byte-array encoding.

```sh
agent-computer-server candidate-file-save-once \
  --database-url-file /run/secrets/agent-computer/database-url \
  --credential-file /run/secrets/agent-computer/connection-credential \
  --lease-id lease-example \
  --config-file /run/secrets/agent-computer/file-worker.json \
  --request-file /run/secrets/agent-computer/file-save.json
```

```json
{
  "lease": {
    "connection_session_id": "connection-example",
    "generation": 1,
    "epoch": 1,
    "expected_revision": 1
  },
  "dispatch_id": "save-example-1",
  "edit": {
    "path": "note.txt",
    "expected": null,
    "content": [72, 105, 10],
    "executable": false
  }
}
```

The command returns the current lease view; a successful command invocation alone does not mean the file was saved. Inspect `file_edit.state` and `state`:

| File result | Meaning and ownership |
| --- | --- |
| Applied | File and directory sync/readback confirmed, authority valid at database acceptance; bounded writer drained and lease Released |
| Conflict | Old content/mode precondition failed before mutation; bounded writer drained and lease Released |
| Expired | Local deadline already passed before mutation; bounded writer drained and lease Released |
| Unknown | IO or acknowledgement unconfirmed, or authority lost before accepting a completed write; inspect `drain_confirmed` |

For uncertain IO, `drain_confirmed=false`, no release proof is created and the lease remains Draining. For a completed synchronized write whose authority was subsequently revoked, the accepted result is Unknown but its sealed drain evidence can release ownership. It does not undo the file change. No client-supplied stopped flag is accepted. General process/node fencing is still separate work.

Each epoch permits one save. A confirmed save/conflict/pre-mutation expiry closes that epoch using `bounded_file_drained` proof; acquire again for the next save. `GET /v1alpha1/leases/{id}` and `writer-lease-reconcile` expose nullable `file_edit` metadata without file content or host paths. Exact command retries with the same dispatch ID and file intent return a recorded result without IO. A dispatch without completion never reissues a permit; changing the intent fails. Old-epoch requests fail after another acquisition.

## 20.4 Durability and verification

Migration 11 adds immutable completion rows bound to dispatch, epoch, input digest and preparation digest. The transaction records observed versus accepted outcome, event/Outbox, draining and any release proof together. Final authorization and expiry are rechecked; failed transactions do not change those records. A live worker can retry persistence with the same sealed outcome without repeating IO. If it dies before persisting that evidence, the dispatch remains unresolved and blocks takeover; recovery needs future authoritative drain/fence evidence. Temporary files from uncertain mutations are retained for diagnosis, without automatic cleanup.

Seven new local-filesystem cases cover atomic replacement, pinned old-reader inodes, binary/Unicode/empty/limit files, stale versions, hostile objects, Candidate identity and pre-mutation expiry. Two PostgreSQL cases verify unknown-dispatch replay and upgrade safety. Together with existing suites, default Cargo/Bazel checks contain 213 cases. These tests do not qualify JuiceFS behavior by themselves. The explicit `candidate_worker_live_test` additionally exercises real CSI/preparation, CLI save/retry, replacement, conflict, revocation after IO, failed Outbox persistence and blocked handoff after unconfirmed IO; run it only in a disposable environment using the configuration from section 17. Its execution evidence is recorded separately when available.

HTTP file APIs, larger streaming files, directory operations, supervised processes/watchdogs, physical fencing, Artifact publication and Computer Ready remain pending. T01–T43 full runtime acceptance remains `not_run`.
