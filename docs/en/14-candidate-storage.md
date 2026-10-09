# 14. Candidate storage preparation

## 14.1 Delivered scope

`agent-computer-storage` prepares independent Candidate files on a trusted JuiceFS mount. It validates a fixed manifest, installs the directory quota, copies and hashes each object, synchronizes files and directories, then publishes a private preparation receipt with an atomic non-replacing rename. This is a Linux operator library and command, not a tenant API or a runtime writer grant.

The control database must still supply and authorize the organization, Volume, Workspace, Candidate, Computer and generation. Runtime grants, modification leases, fencing, Pod mount admission and the transaction that consumes preparation evidence remain pending. The existing Workspace declaration schema is unchanged; this internal request's quota is not a newly supported public declaration field. Artifact authorization and object-store retrieval must populate the private input cache before invoking preparation. Knowing an object hash does not grant access.

## 14.2 Storage contract

The worker requires a full JuiceFS 1.4.1 filesystem mount with S3 storage, the expected filesystem UUID, and both JuiceFS writeback and FUSE writeback disabled. A regular local directory is rejected by the production constructor. The operator supplies a pre-existing, exclusively owned `0700` Volume preparation directory beneath that mount. Its parent paths and the configuration, executable and cache ancestors must remain operator-controlled. Each application receives only the eventual `data` leaf.

Within that Volume directory, preparation uses:

```text
organization/<org>/workspace/<workspace>/candidates/<candidate>/generation/
  staging_<random>/             # incomplete or losing attempts are retained
    data/
    receipt.json
  <generation>/                # atomically published, never overwritten
    data/                      # the only future application mount
    receipt.json               # outside that mount
```

IDs are bounded opaque strings. Requests and manifests reject unknown fields. Manifests support directories and regular files with an exact byte length, SHA-256 and executable flag. Paths are relative, at most 1,024 bytes, 32 components and 255 bytes per component; traversal, empty components, backslashes and NUL are rejected. Up to 10,000 entries and 10,000 distinct directories are accepted. A conservative 4 KiB allocation estimate must fit the whole-GiB quota. This is an input bound, not a reservation against concurrent aggregate usage.

Descriptor-relative `openat2` forbids symlinks and crossing mount boundaries. Source cache files must have one hard link and be regular files; special files are rejected before opening them for I/O. Output files are newly created independent inodes, with no hard-link or copy-on-write dependency. The future writer receives ownership and `0600` files, `0700` executables and directories. Control parents remain private to the worker.

The quota adapter checks the metadata filesystem UUID through `juicefs status` and waits for successful `juicefs quota set` before reading any source data. It uses an operator-pinned executable digest, structured arguments, a private password file and a bounded command timeout. It never treats rounded quota table output or CSI's asynchronous quota annotation as acknowledgement. Quota is applied again at the final published path before success is returned. JuiceFS clients cache quota accounting; this does not establish byte-perfect admission across nodes. See the upstream [quota guide](https://juicefs.com/docs/community/guide/quota/) and pinned [quota implementation](https://github.com/juicedata/juicefs/blob/v1.4.1/cmd/quota.go).

Each write, file `fsync` and directory synchronization must succeed. Publication uses `renameat2(RENAME_NOREPLACE)`. Exact retries verify the retained request/owner/Volume-path binding, filesystem UUID and data inode, then reconfirm quota; they preserve potentially modified working files without reading the base objects again. Changing the manifest, Computer, quota or writer identity for the same Candidate/generation conflicts. A new generation gets separate storage. The receipt is storage evidence only, even if a previous attempt renamed successfully but failed to confirm the final quota.

Failures and competing attempts retain their staging directories. There is no automatic deletion, adoption of partial files, orphan budget reservation or garbage collector in this increment. Operators must account for these retained bytes; repeated preparation attempts are not suitable for unbounded tenant dispatch until admission and cleanup are implemented.

## 14.3 Operator command

Build with `bazel build //crates/storage:agent-computer-storage` or `cargo build -p agent-computer-storage --locked`. Run on Linux with `openat2`, non-replacing rename and directory sync support. The worker needs privileges to assign the selected non-root UID/GID. Do not expose the complete filesystem or metadata credentials to application Pods.

Example operator configuration, with all deployment placeholders replaced:

```json
{
  "mount_root": "/private/mounts/juicefs",
  "volume_path": "actual-pv-subdirectory/preparation",
  "filesystem_uuid": "actual-filesystem-uuid",
  "volume_uid": "actual-volume-uid",
  "object_cache": "/private/objects",
  "writer_uid": 1000,
  "writer_gid": 1000,
  "quota": {
    "executable": "/private/bin/juicefs",
    "executable_sha256": "sha256:replace-with-64-lowercase-hex-digits",
    "metadata_url": "postgres://metadata@metadata.example/juicefs?sslmode=verify-full",
    "password_file": "/private/secrets/metadata-password",
    "timeout_seconds": 30
  }
}
```

This first adapter accepts a credential-free PostgreSQL URL with one `sslmode` parameter. Production TLS certificate distribution remains a deployment responsibility; the local component experiment uses `sslmode=disable` only inside its disposable VM. Configuration and password files must be private regular files owned by the invoking user, at most 8 KiB. Request and standalone manifest files have a 4 MiB limit. All require a single hard link; errors omit raw paths, backend output and credentials.

For a minimal empty manifest, save `{"entries":{}}` as a private file, then obtain its canonical digest:

```bash
agent-computer-storage manifest-digest --file /private/manifest.json
```

Use that digest in a private request file:

```json
{
  "organization": "org_example",
  "volume_uid": "actual-volume-uid",
  "workspace": "workspace_example",
  "candidate": "candidate_example",
  "computer": "computer_example",
  "generation": 1,
  "quota_bytes": 1073741824,
  "manifest_digest": "sha256:replace-with-manifest-digest",
  "manifest": {"entries": {}}
}
```

For files, each entry is `{"kind":"file","sha256":"sha256:<64 hex>","size":123,"executable":false}` and its cached bytes reside at `<object_cache>/<64 hex>`. A directory entry is `{"kind":"directory"}`. The manifest digest is domain-separated SHA-256 of the canonical JSON value; use the command rather than hashing the textual input file.

```bash
agent-computer-storage prepare \
  --config-file /private/storage-config.json \
  --request-file /private/prepare-request.json
```

Success prints one JSON receipt. Exit code 1 leaves the outcome unconfirmed and requires reconciliation of the same identity. The receipt does not certify that an old writer has stopped, or authorize a new writer.

## 14.4 Verification and limits

Twenty-two default storage tests cover independent files, preserved edits on retry, generation separation, conflicting bindings, input integrity/size bounds, quota failures before copying and after publication, concurrent publishers, symlink/hard-link/FIFO rejection, inode replacement, private control paths, mount qualification and quota command failure/timeout. Run `bazel test //crates/storage:storage_contracts_test` or `cargo test -p agent-computer-storage --locked`.

A disposable VM probe additionally exercised the real Rust command against JuiceFS/PostgreSQL/S3, including directory publication, non-root modification through a leaf-only bind mount, retry, generation isolation and `EDQUOT` at the published path. This component result does not establish runtime authorization, fencing, power-loss recovery, distributed quota admission or full T01–T43 acceptance. File sync failures caused by actual backend outages and multi-node recovery still require fault-injection evidence.
