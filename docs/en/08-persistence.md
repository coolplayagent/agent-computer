# 08. PostgreSQL declaration persistence

## 08.1 Delivered scope

The internal `Store::record` method records validated ComputerSet documents in an internal PostgreSQL registry. Each organization and document name has an append-only version history and a current revision. This preserves submitted intent for later planning; it does not resolve external references, create individual resource SpecVersions, authorize an apply, enqueue reconciliation, or start a Computer. This trusted internal method takes a separate registry precondition. The [authorized plan/apply path](10-plans-and-apply.md) now publishes resource versions and intents in the same registry transaction, using root `metadata.expectedRevision` for the declaration head and each resource's own revision condition. It never calls the unguarded record method from HTTP.

The service must authenticate and authorize every call, including reads, retirement and event acknowledgement. `Store` receives a trusted SQLx pool; deployment credentials, TLS, connection limits and timeouts remain the service's responsibility. The library is not an HTTP or CLI database endpoint. Database access is trusted; organization predicates are not PostgreSQL row-level security.

## 08.2 Transaction contract

`record` requires an organization, principal, idempotency key, validated document, and explicit `Create` or `Match(revision)` precondition. It hashes the operation, precondition and canonical document itself. Canonical bytes contain the name and every declaration field. The idempotency scope is organization + principal + operation + key.

One transaction acquires the organization's stream row lock, checks an existing request receipt, checks the current revision, and commits the new immutable version, head pointer, receipt, event and Outbox entry. A retry with the same intent returns the original receipt even if the head has since advanced. Changed intent returns `IdempotencyConflict`; a retired key returns `IdempotencyGone` and cannot be reused. Failed transactions leave no key reservation or partial version/event.

The sequence counter is an ordinary locked row, so later writes in the same organization cannot commit ahead of a lower sequence. Other organizations have independent counters. PostgreSQL retains row locks until transaction end; the tradeoff is serialized registry writes within each organization. See [PostgreSQL row locks](https://www.postgresql.org/docs/18/explicit-locking.html#LOCKING-ROWS).

Immutable version rows retain the exact canonical bytes and digest. SQL triggers reject UPDATE and DELETE; foreign keys ensure every current head names an existing version. Migrations are embedded for both Cargo and Bazel; SQLx checks migration history/checksums and locks concurrent migration runs. See [SQLx Migrator](https://docs.rs/sqlx/0.8.6/sqlx/migrate/struct.Migrator.html). Database owner privileges remain outside these application safeguards.

Database errors can leave a client uncertain whether COMMIT reached the server. Callers must retry the same intent/key to recover its receipt; an error is never translated into success or permission to issue a fresh key automatically.

## 08.3 Snapshot, replay and Outbox

`snapshot` reads current documents and a watermark in one read-only REPEATABLE READ transaction. `replay(after, limit)` reads the retention floor, watermark and ordered event page from one such snapshot. Pages accept 1–1000 events. The returned `next_cursor` is the last event actually returned, never the later watermark of a limited page. A cursor before the retention floor returns `CursorExpired`; negative or future cursors are invalid. The future API must bind cursors to the authenticated scope.

Events contain declaration name, revision, digest and sequence, without copying document contents. Pending Outbox reads repeat until explicitly acknowledged. A delivery worker must acknowledge only after its sink accepts the event, and the sink must deduplicate by organization + sequence. Multiple workers may deliver duplicates or out of order; no external delivery worker or exactly-once guarantee is implemented.

Explicit pruning removes only acknowledged events and their Outbox rows, advancing the retention floor atomically. It preserves version history and request records. Retiring a request erases its receipt but retains its permanent key tombstone. Snapshots currently materialize all current declarations for an organization; paginated production snapshots, storage quotas and retention scheduling remain pending.

## 08.4 Real database verification

Install PostgreSQL 18 server/client binaries and run as a non-root user. Tests default to `/usr/lib/postgresql/18/bin`; override `AGENT_COMPUTER_PG_BIN` for another installation. For unpacked packages, set `AGENT_COMPUTER_PG_SHARE` to their share directory and supply any required shared-library path. No existing database URL is used.

```bash
export AGENT_COMPUTER_PG_BIN=/usr/lib/postgresql/18/bin
bazel test //... --test_env=AGENT_COMPUTER_PG_BIN --test_output=errors
cargo test --workspace --locked
```

If set, also pass `--test_env=AGENT_COMPUTER_PG_SHARE` and `--test_env=LD_LIBRARY_PATH` to Bazel. Missing binaries fail the suite rather than skipping database checks. Each test starts its own temporary cluster under a private `/tmp` directory, opens only a Unix socket, retains fsync/synchronous_commit/full_page_writes, and stops/removes the cluster on exit. Bazel's test requires these locally provisioned external binaries; it is not a hermetic PostgreSQL distribution.

Eight integration cases cover concurrent migration and checksum rejection, immutable history and WAL crash recovery, scope/intent/tombstone isolation, simultaneous retries and CAS, transaction rollback after injected final-write failure, per-organization commit locking, snapshots during writes, and replay/Outbox retention. The tested baseline is PostgreSQL 18.6 on Linux x86_64. This establishes local database behavior; replication failover, disk/power loss, production database roles, backups, and full T01–T43 runtime acceptance remain untested.
