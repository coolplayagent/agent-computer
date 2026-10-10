# 44. Continuous queued execution worker

[48 Bounded background execution](48-background-execution.md) adds a default background lifetime while preserving the original fixed deadlines and identity requirements.

The trusted operator can run a persistent worker for one organization and one qualified local Volume. It automatically claims queued Candidate executions and runs the existing guarded execution, output publication, completion and cleanup path. Callers still submit through the durable execution admission API; they do not select nodes or supply worker configuration.

## Run and stop

```sh
agent-computer-server execution-worker --database-url-file /private/control-url --organization example --config-file /private/execution.json --concurrency 2 --poll-ms 250
```

The private configuration has the same shape as [the single-execution worker](29-execution-worker.md), including the node bindings from [31](31-node-guarded-startup.md) and output store/spool from [36](36-durable-execution-outputs.md). It pins the Kubernetes namespace UID, Volume ID, PVC/PV UIDs, filesystem UUID, Volume path and writer UID/GID. The node's boot ID and executable hashes must match the current qualified host. Refresh operator configuration after a node reboot or binary replacement. Credentials are loaded at process startup; restart after rotation.

Startup checks local root authority, boot identity, pinned executables, private spool, runtime socket parent and mounted storage configuration before claiming work. It does not prove current Kubernetes, S3, CSI or reaper availability; each execution still checks its own actual runtime identity and fresh authority. Configuration failure exits before admission. Run migrations with `agent-computer-server migrate --database-url-file /private/control-url` before starting this command.

Concurrency is 1–4 active jobs per process, default 1. Polling is 250–5000 milliseconds, default 250, and claims at most one request per tick. These are local bounds, not deployment-wide capacity or fairness guarantees. Existing admission ceilings and writer exclusion still apply. Saturation skips polling until a job leaves its slot. Poll failures retain the fixed interval and report an event.

SIGTERM or Ctrl-C stops new claims and waits for admitted jobs. A claim already in progress may finish and enter the join set. The worker does not cancel a running job just because its operator requested shutdown. Execution deadlines, caller cancellation, fresh authority checks and independent node watchdogs still apply. A forced process kill can lose live completion proofs and leave Unknown/Draining, as with one-shot dispatch.

[The systemd example](../../deploy/systemd/agent-computer-execution-worker.service) requires the CSI and expiry-reaper services. Its root-owned environment file supplies `ORGANIZATION`, `DATABASE_URL_FILE` and `CONFIG_FILE`. Review paths against the qualified host before installation. It uses four slots and permits 180 seconds for graceful shutdown, after which systemd may force termination. This is a service-manager bound; it does not certify that blocked physical filesystem IO can always finish within that period.

The command emits JSON lines for readiness, claims, cancellations, completed jobs, unconfirmed jobs, poll failures and shutdown. `finished` counts returned worker results, including Unknown and interrupted outcomes; it is not a successful-execution counter. Raw stdout/stderr and credentials are excluded. Logs are operational observations; the durable database journals remain authoritative. Keep the output consumer draining: synchronous log output can delay polling and signal handling.

## Atomic claims and recovery boundary

The store selects the oldest `Queued` execution by creation time and execution ID for the exact organization and complete storage target. Selection and the existing single-use dispatch admission share one transaction and organization stream lock. Competing workers and explicit one-shot dispatch therefore cannot both obtain an attempt for the same execution. Migration 24 adds a partial polling index over queued rows without changing historical requests.

The transaction rechecks the original credential, current grants, submitted connection/background lifetime, Candidate and writer before committing the dispatch intent and Outbox. Revoked or expired requests become Cancelled without external dispatch. An Outbox failure rolls back the claim. Cancellation does not invent a writer-release proof; ordinary lease reconciliation retains its existing semantics. The original queue deadline is never renewed by polling, lock waits, dispatch or process restart.

Restart selects only requests still Queued. Dispatching, CancelRequested, Unknown and terminal requests are excluded. An ambiguous claim timeout is not retried by identity: a committed dispatch remains excluded, while a rolled-back transaction may be claimed later. Persistent records cannot recreate a live process/IO seal. Explicit observation-only recovery remains available; automatic recovery of previously dispatched jobs is outside this worker's scope.

## Verification and limits

PostgreSQL contracts exercise full target isolation, concurrent claims, WAL restart, authority loss, expiry, Outbox rollback and migration preservation. The real fixture runs two gVisor executions concurrently through the operator command, sends SIGTERM after both startup grants exist, and requires both completion seals, output publications and writer releases. Restart must claim zero completed requests; invalid options and a stale boot configuration must consume no queued work. These checks run alongside the existing execution fault scenarios.

[Source-bound component record](../evidence/queued-execution-worker-2026-10-10.json) · [validation log](../evidence/queued-execution-worker-2026-10-10.log)

This delivers node-local automatic dispatch for prepared Candidates. Full Computer lifecycle scheduling, automatic drain recovery, cross-node fencing, browser/ComputerView and product acceptance remain pending. Computer `ready=false`, public `execution` remains unsupported, and T01–T43 remain `not_run`.

[Renewable execution leases](49-renewable-execution-leases.md) extend the running phase only; queue and setup deadlines remain fixed.
