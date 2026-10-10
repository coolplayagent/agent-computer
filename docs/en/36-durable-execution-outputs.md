# 36. Durable bounded execution outputs

The one-shot execution worker now retains a received final supervisor report after the controller exits. It stores the original envelope, retained stdout, retained stderr and supervisor diagnostics as content-addressed S3 objects. PostgreSQL holds their references and bounded stream metadata. This is output publication only: execution remains `Unknown`, the writer remains `Draining`, and no completion or drain certificate is created.

## 36.1 Capture and publication

Initial capture requires the original in-memory dispatch handle and private attach observation. The collector checks the recorded Pod UID, dispatch, startup grant, watchdog arm, challenge, request digest and generation. Strict report decoding checks retained byte limits and truncation counters. A reported success additionally requires exit code zero, no signal, reaped children and both stream EOFs. These checks do not make the process observation an authoritative completion.

Migration 19 adds immutable `execution_output_intents` and `execution_outputs`. An intent binds four references to the organization/execution and original dispatch/grant/arm/Pod. It is committed with `execution.output_pending` before external publication. Existing grants do not acquire outputs during migration.

The collector writes the exact bytes into a separate private local spool: exclusive files, file fsync, directory fsync, atomic non-replacing rename and parent fsync. Reads reject symlinks, foreign ownership, group/world permissions, hard links, incorrect sizes and hashes. A retry verifies an existing spool entry. The spool must be operator-owned durable local storage, independent of the tenant Candidate and watchdog journal directory.

Object keys are `execution-outputs/v1/<organization>/<execution>/<sha256-hex>`. The store identity hashes endpoint, region and bucket; credential rotation preserves it. The client uses explicit credentials, SigV4 and path-style addressing. HTTPS is required unless `allow_http` is explicitly enabled for an isolated fixture. Redirects, ambient proxies and automatic transport retries are disabled. Each request has a 2-second connection timeout and 10-second total timeout. Bucket names currently permit lowercase letters, digits and hyphens only; session credentials and endpoint path prefixes are unsupported.

A conditional PUT uses `If-None-Match: *`. A successful response or an existing-object response must be followed by a full bounded GET matching both length and SHA-256. ETag is not used as content proof. Missing objects can be recreated only from the original verified spool bytes. Conflicts, errors, incorrect objects and uncertain acknowledgements leave publication unconfirmed. Once all four references have been read back, one transaction registers publication and `execution.output_verified`. Repeated recovery returns the same timestamp and emits no duplicate output events.

The worker limits collection to 30 seconds. Dropping that future cannot cancel a running filesystem syscall or blocking spool task. A crash before the complete spool rename can leave an intent with no recoverable local bytes. In that case recovery may use already-present verified S3 objects, but it cannot fabricate missing bytes or rerun the workload. Spool/object retention and garbage collection remain operator responsibilities; no automatic deletion is implemented.

## 36.2 Configuration and retrieval

The private execution-worker configuration requires these additional fields inside `execution`:

```json
{
  "output_spool": "/var/lib/agent-computer/outputs",
  "outputs": {
    "endpoint": "https://objects.example.invalid",
    "region": "us-east-1",
    "bucket": "execution-outputs",
    "credentials_file": "/etc/agent-computer/output-credentials.json",
    "ca_file": null,
    "allow_http": false
  }
}
```

Create the spool directory with mode 0700. The credentials file is a private, owner-only regular JSON file containing `access_key` and `secret_key`; configure bucket access and retention before dispatching. An optional private `ca_file` replaces the HTTPS trust roots. Invalid configuration or an unavailable spool refuses dispatch before creating its durable execution intent. Existing configurations must add these fields when upgrading.

`GET /v1alpha1/executions/{id}/output` requires `runtime.connect` and the original valid connection credential. It returns null before capture, otherwise `pending`/`verified`, the manifest digest, observed outcome, stdout/stderr hashes, retained/observed counts, truncation/EOF flags and publication time. It does not expose object locations or bytes. `verified` describes the historical publication check, not present availability or accepted execution success.

Trusted operators with database and object-store access can use the same private worker configuration:

```sh
agent-computer-server execution-output-recover --database-url-file /private/database-url \
  --organization org_id --execution-id execution_id --config-file /private/worker.json
agent-computer-server execution-output-read --database-url-file /private/database-url \
  --organization org_id --execution-id execution_id --config-file /private/worker.json
```

Recovery rechecks every stored reference and retries missing uploads from the original spool. Read requires a publication record and verifies the full report object again before writing its original JSON bytes to stdout. Both commands run independently of Kubernetes connectivity and never create, attach or execute a Pod. Keep their output private: the report contains user stdout/stderr. Normal dispatch command JSON contains only output metadata and `output_unconfirmed`, with raw observations omitted.

## 36.3 Scope and verification

The retained stream cap is the original execution command cap, at most 1 MiB per stream. The JSON envelope is bounded by `8 * cap + 16384` bytes and diagnostics by 64 KiB. Observed byte counts may exceed the retained cap. The current collector captures final reports; live chunk streaming and reports lost with the controller before capture remain pending work.

Tests cover private spool reopening/tampering, explicit store binding, SigV4, conditional PUT, lost acknowledgements, corrupt/truncated/oversized GETs, redirects, authorization/service failures, SQL transplant rejection, immutable publication, WAL recovery and migration history. The real disposable PostgreSQL/SeaweedFS/K3s/CSI/gVisor fixture checks normal output, an operator dispatch, truncated streams, rejected S3 credentials and database publication failure. Fresh operator processes recover the two pending cases and repeat recovery without extra grants or events, using deliberately unavailable Kubernetes credentials.

Physical storage fencing and writer drain are still required before authoritative completion. As described in the [JuiceFS CSI architecture](https://juicefs.com/docs/csi/introduction/), a CSI client may be shared across Pods using a PV; Pod deletion, process death and an empty cgroup do not establish that its writes have drained. This increment does not claim product T01–T43 acceptance.

The 2026-10-10 [source-bound evidence](../evidence/durable-execution-outputs-2026-10-10.json) and [logs](../evidence/durable-execution-outputs-2026-10-10.log) record 327 passing default tests (141 PostgreSQL cases), eleven Bazel test targets and eleven real worker scenarios. Five publications cover twenty references verified again by an independent S3 client. Both injected publication failures recover without new grants or duplicate events. The owned VM, fixture service and private disk/key files were removed.
