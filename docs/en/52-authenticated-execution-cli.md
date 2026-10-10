# 52. Authenticated execution CLI

`agent-computer` now calls the existing bounded runtime APIs from a terminal or external Agent. It opens logical connections, acquires Candidate writer leases, submits structured commands, queries or cancels executions and downloads retained output. The service still owns authorization, revisions, idempotency, dispatch and completion. A successful submission returns Queued; it is not a successful process result.

## Configure and connect

An operator must configure the control service and workers, publish the Computer/Sandbox definitions, and issue the caller a scoped service credential and resource grants. The CLI never reads the control database or Kubernetes credentials. It does not create an identity, start workers or grant itself access.

```sh
export AGENT_COMPUTER_ENDPOINT=https://computer.example.org
export AGENT_COMPUTER_TOKEN_FILE=/private/computer-token
agent-computer doctor --json
agent-computer computer show cmp_example --json
agent-computer computer start cmp_example --request start.json --idempotency-key start_001 --json
```

`start.json` follows the existing start-admission API:

```json
{"expected_revision":1,"expected_spec_revision":1,"max_runtime_seconds":300}
```

Keep the returned request ID, Candidate ID and generation. Query `computer show` until the configured preparation worker reports Prepared. That state allows the bounded Candidate APIs; Computer `ready=false` remains accurate because general runtime/App readiness is incomplete. `doctor` reads the service capability advertisement, not a deployment certification or a worker health check.

Create `connect.json`, then open a connection explicitly:

```json
{"requested_capabilities":["connect","read","modify"],"lifetime_seconds":900}
```

```sh
agent-computer connect cmp_example --request connect.json --idempotency-key connect_001 --json
```

Use its session ID and the start receipt in `lease.json`:

```json
{"scope":"modify","connection_session_id":"session_example","candidate_id":"candidate_example","generation":1,"duration_seconds":30}
```

```sh
agent-computer lease acquire cmp_example --request lease.json --idempotency-key lease_001 --json
```

## Submit and observe

Use the returned lease ID, generation, epoch and revision, and the configured Sandbox ID, in `execution.json`. Prepare templates before acquiring the short lease and submit while it remains valid; renewal is explicit. These example identifiers are placeholders; the CLI does not discover or substitute authority fields. `argv` remains structured JSON and is never evaluated by a local shell.

```json
{"lease_id":"writer_example","lease":{"connection_session_id":"session_example","generation":1,"epoch":1,"expected_revision":1},"sandbox_id":"sandbox_example","command":{"argv":["/bin/echo","hello computer"],"cwd":"","timeout_seconds":10,"term_grace_ms":500,"output_limit_bytes":4096}}
```

```sh
agent-computer exec cmp_example --request execution.json --idempotency-key execution_001 --json
agent-computer status exec_example --json
agent-computer logs exec_example --json
agent-computer logs exec_example --stream stdout --output stdout.bin --json
agent-computer logs exec_example --stream stderr --output stderr.bin --json
agent-computer disconnect session_example --json
```

`logs` without a stream returns publication metadata, or null before capture. Downloads require the output gateway and original still-authorized submission credential. They verify exact length, SHA-256 and collection metadata before publishing a private new file. Existing targets, including symlinks, are never replaced. Output bytes never go to the terminal. The JSON receipt preserves observed byte count, retained byte count, truncation and EOF; an incomplete or truncated observation does not establish execution success.

Disconnect closes only the logical connection. New submissions default to background lifetime; a submitted background execution may continue after disconnect within its original authorization and budget. For connection lifetime, include `"lifetime":"connection"` in the execution request. No connection, writer or execution lease is automatically renewed by the CLI.

To cancel, write `{"expected_revision":1}` using the current execution revision into `cancel.json`, then run:

```sh
agent-computer cancel exec_example --request cancel.json --idempotency-key cancel_001 --json
```

Inspect the returned state: cancellation requested is not physical termination. `Unknown` remains Unknown until authoritative evidence resolves it.

## Commands and request contracts

| Command | Existing API |
| --- | --- |
| `doctor` | GET `/v1alpha1/capabilities` |
| `computer show/start/cancel-start/stop/checkpoint-stop ID` | GET `runtime` or POST `start`, `start/cancel`, `stop`, `checkpoint-stop` under `/computers/{id}` |
| `connect ID`, `connection show/heartbeat ID`, `disconnect ID` | Connection-session create, read, heartbeat, DELETE |
| `lease acquire/show/renew/release ID` | Candidate modify-lease endpoints; acquire takes Computer ID |
| `exec ID`, `status ID`, `cancel ID` | Execution submit, read, cancel; submit takes Computer ID |
| `logs ID` with optional `--stream stdout|stderr --output FILE` | Publication metadata or verified stream download |

Remote commands accept `--endpoint` and `--token-file` overrides. POST commands require `--request FILE|-` and an explicit `--idempotency-key`; request objects are bounded to 64 KiB. Field definitions are in the [OpenAPI](../../schemas/openapi-v1alpha1.json) and the linked [connections](18-connection-sessions.md), [writers](19-candidate-writer-leases.md), [executions](23-execution-admission.md), [outputs](43-execution-output-downloads.md) and [checkpoint stop](46-checkpoint-stop-worker.md) contracts. Request and token paths resolve against the original invocation directory under `bazel run`.

Success JSON goes to stdout, errors to stderr; `--json` makes either compact. Exit 0 means the HTTP operation was accepted, exit 1 means HTTP rejection, and exit 2 means invalid input/configuration or incomplete verification. HTTP error reports preserve server codes, details and request IDs, with `http_status` added. Transport failures omit URLs, request bodies and credentials. A write without a verified response, or with an HTTP 408, 5xx or redirect response, reports `request_may_have_been_applied=true`: query state or explicitly retry identical input with the original key. Never use a new key merely because a response was lost.

Only HTTPS origins and literal loopback HTTP origins are accepted; user information, path prefixes, query strings and fragments are rejected. Loopback bypasses ambient proxies. The client follows no redirects and performs no automatic retries. Connect timeout is 5 seconds and total HTTP timeout is 15 seconds. JSON responses are bounded to 4 MiB and each output stream to 1 MiB. There is no custom-CA, insecure TLS, raw API, automatic polling or live-output tail option in this increment.

## Verification scope

Wire tests cover routes, exact argv/key transmission, proxy bypass, redirect refusal, transport uncertainty, input/response bounds, binary and empty downloads, truncation, corruption, incomplete bodies and existing-file preservation. A real CLI subprocess talks over TCP to the actual Axum service and PostgreSQL: connection/lease admission, one durable execution despite retries, conflict, background disconnect, status, cancellation and principal revocation. Preparation receipts in that test are synthetic, and SQL confirms zero dispatch intents. It establishes client/control-plane behavior, not new gVisor or storage durability evidence. Full Computer lifecycle, Browser/ComputerView, OIDC and T01–T43 acceptance remain incomplete.
