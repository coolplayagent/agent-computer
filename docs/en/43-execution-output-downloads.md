# 43. Authenticated execution output downloads

A credential that submitted an execution can download its retained stdout and stderr through the control service. The gateway reads the immutable output objects from S3 and verifies their size and SHA-256 on every request. It returns complete verified bytes, including binary or empty streams. A publication receipt alone does not establish current object availability.

## Configure and read

Start the service with a private output-store configuration:

```sh
agent-computer-server serve --database-url-file /private/control-url --listen 127.0.0.1:8080 --output-config /private/output-store.json
```

`output-store.json` uses the `outputs` object-store configuration documented in [36](36-durable-execution-outputs.md): endpoint, region, bucket, private credential file, optional CA file and explicit HTTP opt-in for isolated testing. It needs authenticated GET access to the original output objects. It contains no Kubernetes or node configuration and does not use the local publication spool. `--file-config` can enable the existing Candidate file gateway alongside downloads. The configured endpoint must match the store digest in the immutable publication; changing the endpoint is not an object migration.

| Request | Result |
| --- | --- |
| `GET /v1alpha1/executions/{id}/output` | Existing publication metadata, or null before capture |
| `GET /v1alpha1/executions/{id}/output/stdout` | Retained stdout bytes |
| `GET /v1alpha1/executions/{id}/output/stderr` | Retained stderr bytes |

Downloads set the exact `Content-Length` and use `application/octet-stream`, attachment filenames `stdout.bin` or `stderr.bin`, `Cache-Control: no-store` and `X-Content-Type-Options: nosniff`. `X-Output-Sha256` and `X-Output-Manifest-Digest` bind the returned bytes to the publication. `X-Output-Observed-Bytes`, `X-Output-Truncated` and `X-Output-Eof` preserve collection limits; truncated output contains only the retained prefix. The response never includes a bucket, object key, signed URL, supervisor envelope or diagnostics.

Each stream is bounded by the original command's output limit, at most 1 MiB. There are four shared download slots per gateway; clones share capacity. A slot covers the object fetch, final authorization and verified response body until the HTTP transport consumes or drops it. Excess requests receive `503 output_io_busy`; they do not wait in another queue. The existing ten-second request deadline applies. Cancellation drops asynchronous object IO; no detached filesystem job remains. Range requests, query parameters, offsets and live tailing are unsupported and rejected.

## Current authority and historical output

Downloads require the original valid submission credential with `runtime.connect` and `runtime.read`, current Computer `connect`/`read` grants and a `read` grant for the original Workspace. Another credential for the same principal cannot adopt the submission. Cross-organization and inaccessible executions return the same 404 response.

The original connection may be closed or expired, the writer may be released, and a later Candidate or writer epoch may exist. Historical output does not require active modification authority. The read follows the immutable execution and epoch to its original Workspace and checks current grants both before and after S3 IO. No database lock is held during that IO. Credential/principal revocation, expiry or grant removal during a download prevents disclosure, including when the object GET fails.

Pending publication, missing or corrupt objects return `503 execution_output_unavailable` without partial bytes. An absent gateway returns `503 outputs_unavailable`. Capability discovery advertises `execution.output_downloads=bounded-verified-streams` only when configured. Execution admission, metadata and cancellation keep their existing contracts. Downloading output changes no execution state, startup grant, completion or writer lease; an Unknown execution can retain readable observations without becoming Succeeded.

## Verification and limits

Default tests cover binary and empty streams, WAL recovery, pending publication, cross-credential/organization rejection, content corruption, seven mid-GET authority changes, historical reads, HTTP configuration and input rejection, and response-body slot retention. The real execution fixture launches a fresh service process and downloads both streams from the eight published outputs among its sixteen gVisor scenarios, comparing bytes and hashes to the original reports. This includes command failure, timeout, truncation and recovered publication after storage/database failures.

[source-bound component record](../evidence/execution-output-downloads-2026-10-10.json) · [validation log](../evidence/execution-output-downloads-2026-10-10.log)

This is a bounded output-reading capability. Automatic execution dispatch, browser/ComputerView, cross-node fencing and full product acceptance remain unfinished. Public `execution` remains unsupported, Computer `ready=false`, and T01–T43 remain `not_run`.
