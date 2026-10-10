# 45. Continuous Candidate preparation

The trusted operator can now continuously prepare admitted Computer starts on one qualified local Volume. This connects durable start admission to Candidate storage preparation without issuing a separate command for each request. Preparation still establishes storage facts only; it does not mark the Computer Ready or establish App health.

## Operator command

```sh
agent-computer-server candidate-worker --database-url-file /private/control-url --organization example --worker-id preparer --config-file /private/candidate.json --concurrency 2 --poll-ms 1000
```

Run database migrations first. The private configuration is the same as [the original Candidate worker](17-candidate-preparation-worker.md): a qualified `target`, `mount_root`, private `object_cache`, pinned JuiceFS `quota` command configuration, and optional `artifacts` store for restoring published inputs ([38](38-workspace-artifact-checkpoints.md)). The configured Volume must already have a durable successful reconciliation and be mounted on this host. This command does not provision or mount a Volume.

Before polling, the worker opens the actual mount/cache, checks quota configuration and validates any configured object client. The same local storage checks run again for each scheduled job. Filesystem setup runs on blocking tasks and remains joined; there is no timeout that discards an unfinished filesystem operation and silently opens another slot. These checks do not replace fresh database authorization, remote object integrity checks or actual directory quota verification.

Concurrency is 1–4 scheduled operations per process, default 1. Polling is 250–5000 milliseconds, default 250, with at most one scheduled operation per tick. These are shared option bounds with the [execution worker](44-queued-execution-worker.md), not deployment-wide capacity limits. Each process remembers its last request ID and rotates eligible requests in ID order. Failed, busy and storage-unknown jobs have a five-second local cooldown. A revoked credential or an unresolved preparation therefore cannot monopolize the list; database errors use the fixed polling interval.

SIGTERM and Ctrl-C stop scheduling and join all scheduled operations, including jobs waiting to claim database authority. Read-only discovery can be cancelled during shutdown. Joined jobs may still claim, prepare or record their result under the original authorization after the signal; they do not obtain a longer queue deadline. A forced process kill loses in-memory work, leaving durable leases and identities for later reconciliation.

[The systemd example](../../deploy/systemd/agent-computer-candidate-worker.service) reads root-owned `DATABASE_URL_FILE`, `ORGANIZATION`, `WORKER_ID` and `CONFIG_FILE` settings. It uses two slots, one-second polling and a 240-second stop timeout. Arrange the qualified JuiceFS mount before starting it. The service-manager timeout does not prove physical IO can always finish within that time. JSON-line logs report scheduling, returned outcomes, unconfirmed jobs and shutdown; they contain no user file bytes or credentials. Keep the log consumer draining because synchronous output can delay the scheduler.

## Discovery and authority

Discovery is read-only. It returns at most the existing platform ceiling of 64 active starts for the exact organization and Volume. Requests must still be current for the Computer generation, have committed Workspace input, and be Queued within their original fifteen-minute deadline or Preparing with a recorded dispatch. Prepared, Sealing, Sealed, Cancelled and Stopped requests are excluded. Active preparation leases are excluded; a previously bound preparation must match every target field. Migration 25 adds a partial index over Queued/Preparing starts.

An ID from this list is not a permit. The existing atomic claim checks original principal/credential authority, all pinned graph grants and catalog dependencies, successful Volume evidence, immutable input, complete target identity and coordination epoch. Concurrent processes can discover the same ID, but only one receives the current lease; others receive Busy. Permission loss leaves the request and reservations intact rather than manufacturing a cancellation or releasing resources.

Before any storage dispatch, an expired lease can be claimed again for execution while the original queue deadline remains valid. After the dispatch journal commits, any later claim is Observe only. Recovery checks the original directory publication, inode, filesystem identity and quota; it may record a real receipt whose database acknowledgement was lost. A missing or uncertain publication remains `storage_unknown` and is never recreated. The original 180-second coordination lease is unchanged; slow work that outlives it may require a later observation before its receipt can commit. Polling does not free retained reservations.

## Verification and remaining work

PostgreSQL tests cover read-only discovery, matching organizations/targets, competing claims, active leases, cancellation, WAL restart, observation-only recovery, fresh authorization and migration preservation. The real fixture schedules a new preparation alongside a previously published directory whose database acknowledgement was lost. It holds the organization lock, sends SIGTERM while both operations wait for admission, releases the lock, and requires both receipts to commit. The recovered directory must retain its inode and contents. Restart schedules no completed work; a later missing publication remains absent and unknown.

[Source-bound component record](../evidence/continuous-candidate-preparation-2026-10-10.json) · [validation log](../evidence/continuous-candidate-preparation-2026-10-10.log)

Volume orchestration, full Computer lifecycle scheduling, runtime/App health, automatic drain recovery and product acceptance remain incomplete. Computer `ready=false`, public execution remains unsupported, and T01–T43 remain `not_run`.
