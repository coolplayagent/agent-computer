# 21. Candidate file HTTP gateway

## 21.1 Enable a bounded gateway

The control server can expose the prepared Candidate's files to service-credential clients. Start it with `serve --database-url-file ... --file-config /run/secrets/agent-computer/file-gateway.json`. The private JSON file is an array of 1–16 [file worker configurations](20-bounded-file-saves.md): each has `mount_root` and the exact immutable preparation `target`. Duplicate Volume IDs and relative mount roots are rejected. The existing private-file loader limits this configuration to 8192 bytes. Mount roots and their ancestors are controlled by the operator; users cannot supply host paths or storage bindings.

Without this option, `files.read` and `files.save` remain `unsupported`; with it, the capability response reports `bounded-candidate`. A request still has to match a configured Volume and the qualified live JuiceFS mount. Configuration does not establish mount health or Computer Ready. TLS termination remains a deployment requirement. Browser Origin requests are rejected until the separate browser authentication flow is implemented.

## 21.2 Read with an independent connection

`GET /v1alpha1/workspaces/{id}/files` requires these query parameters exactly once: `connection_session_id`, `generation`, `candidate_id`, and `path`. Unknown parameters are rejected. It reads one file; directory listing and pagination are not implemented.

```text
GET /v1alpha1/workspaces/workspace-example/files?connection_session_id=connection-example&generation=1&candidate_id=candidate-example&path=note.txt
Authorization: Bearer <original-connection-credential>
```

The credential needs `runtime.connect` and `runtime.read`. The active, unexpired connection must belong to that exact credential and must have requested read capability. Current Computer connect/read grants and an independent Workspace read grant are required. Modification permission and a writer lease are not required. The service checks the current generation, Candidate, preparation binding and pinned catalogs before IO, then checks authority and bindings again before disclosing bytes. It holds no database transaction while waiting for the filesystem. Revocation after disclosure cannot retract bytes already delivered.

A successful response is JSON with `version` (`sha256`, `size`, `executable`) and `content` as an array of bytes. Reads are limited to 1 MiB and use a pinned regular inode, so an atomic gateway replacement yields the old or new file. They reject traversal, symlinks, hardlinks, FIFOs, mount crossings, reserved staging names and replaced Candidate roots. Missing or unsupported file objects return `404 file_unavailable`. This assumes every writer uses the Candidate gateway; unmanaged in-place writers are outside the current guarantee.

## 21.3 Save and query an uncertain result

`POST /v1alpha1/leases/{id}/file` accepts the same strict JSON as [20 File saves](20-bounded-file-saves.md): `lease`, stable `dispatch_id`, and `edit`. It requires uncompressed JSON of at most 5 MiB, allowing a byte-array encoding of a file of at most 1 MiB. `edit.expected` carries the content version returned by a prior read; null requires absence. Modification still requires both Computer and Workspace read/modify grants and the original connection's current writer lease.

The stable `dispatch_id` is the effect identity for this endpoint; a separate Idempotency-Key header is not used. Exact retries return the recorded result without IO. Changed content/path/precondition under the same ID conflicts; an unresolved journal returns `409 dispatch_unresolved` and never reissues the write. After a timeout, query `GET /v1alpha1/leases/{id}` or retry the same intent. Do not generate a new dispatch ID to bypass uncertainty. A later epoch makes old-epoch retries conflict.

A `200` response contains the lease metadata, not an unconditional success claim. Inspect `file_edit.state`: Applied, Conflict, Expired or Unknown. The previously documented drain rules still apply. There is no new HTTP endpoint accepting a caller's claimed drain proof. File bytes are not copied into the database, events or Outbox.

## 21.4 Capacity, cancellation and validation

Each configured gateway has four shared IO slots. A slot is reserved before reading a save body, and exhaustion returns `503 file_io_busy`. File jobs run independently of the HTTP waiting future; the existing ten-second HTTP deadline does not release their slot, cancel a FUSE call or declare a writer drained. Blocking mount/file operations execute outside Tokio's async workers. The slot remains held until IO and result acceptance actually return. The global 64-request HTTP limit is counted separately from the four retained IO slots, so four blocked file jobs do not occupy all control request capacity. There is no automatic retry of an unresolved file effect.

The two new storage cases cover bounded reads and hostile objects. Five PostgreSQL cases verify independent read grants/scopes, exact connection credentials, prepared generation/catalog bindings, close/expiry/revocation, and requested read capability. Four server cases verify opt-in capability reporting, request bounds, unknown/duplicate query fields, browser rejection, and retaining IO capacity after cancellation while readiness still responds. Default suites now contain 224 cases. The explicit real Candidate component target additionally exercises a separate TCP server with read-only reads, saves, exact/changed retries, cross-credential rejection, symlink rejection, Workspace revocation and closed connections. An injected eleven-second Outbox delay also tests HTTP timeout followed by completed-job query and an exact retry without another write. Its execution evidence is recorded separately.

Directory operations, streaming/large files, OIDC/human browser sessions, a file UI, product Pod execution, general process fencing, Artifact publication and Computer Ready remain pending. T01–T43 remain `not_run`.
