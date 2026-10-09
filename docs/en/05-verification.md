# 05. Verification and delivery evidence

## 05.1 Evidence boundaries

| Check | What it establishes | What it does not establish |
| --- | --- | --- |
| Bazel build/test | Current Rust targets compile and executed contract assertions hold | External services, real isolation, storage durability |
| PostgreSQL integration tests | Local transactions, concurrency, replay, and WAL recovery | Replication failover, power/disk loss or production deployment certification |
| Cargo fmt/Clippy | Current source formatting and static diagnostics | Product acceptance |
| Documentation checks | Local links, paired language files and numbering | Implementation of the linked designs |
| Qualitygate full | Selected policy checks on the delivery snapshot | Unselected tests; current policy only checks line endings |
| relay-knowledge map validate | Map structure, routes, and digest integrity | Complete index coverage or runtime correctness |
| T01–T43 runtime acceptance | Observed behavior for each test in a pinned real environment | Unexecuted tests or different deployment combinations |

## 05.2 Current record

2026-10-09 (stages 01–02 and partial 03–06): Bazel builds, CLI JSON output and error exit codes, Cargo fmt/Clippy, local documentation links, and bilingual numbering passed; Bazel passes 116 tests (28 domain, 28 declaration, 7 CLI process tests, 33 PostgreSQL cases, 7 server cases and 13 Kubernetes adapter cases). Python independently verifies the Draft 2020-12 schema and example SHA-256. Cargo tests and Clippy also pass. The database suite uses PostgreSQL 18.6 on Linux x86_64 in private temporary clusters with durable settings enabled; it verifies concurrent idempotency/CAS, rollback, immutable versions, migration checksums, snapshot/replay consistency, Outbox retention and WAL crash recovery. See [08 Persistence](08-persistence.md) for reproducible commands and limits. The server suite exercises an independent TCP process through credential issuance, validation, definition grants, catalog administration, plan/apply retries, revocation and graceful shutdown; store tests cover expiry, scope separation and disabled principals. The OpenAPI artifact passes the official 3.1 meta-schema and embedded ComputerSet reference checks. See [09 Control service](09-control-service.md) and [10 Plans and apply](10-plans-and-apply.md). Plan tests also cover seven-resource publication, dependency version/permission checks, revocation lock races and complete rollback at the final write; they publish only control metadata and queued intents. Ten coordination cases additionally exercise exclusive claims, lease/dispatch recovery, revocation races and atomic completion; adapter receipts are synthetic metadata evidence only. See [11 Coordination](11-reconciliation-coordination.md). The Kubernetes protocol suite covers lost responses, identity/spec conflicts, conditional deletion and bounded transport; the explicit live component target is documented in [13 Kubernetes adapter](12-kubernetes-adapter.md). Runtime acceptance T01–T43 is entirely `not_run`. Existing T00 design checks do not establish product delivery.

Local Qualitygate reports are retained in ignored `.qualitygate/`. Before each commit, run `check --worktree --profile full` on the final worktree. Require a nonempty delivery, no pending checks, `gate.complete: true`, `gate.decision: pass`, and exit code 0, plus separate Bazel and static checks. Retain the report's snapshot/policy digests; the default line-ending check is not functional verification.

Runtime evidence follows the [acceptance record schema](../../codespec/test/agent-computer.md): `test_id/status/source_commit/component_digests/environment/input_refs/expected/observed/evidence_refs/limits`. No synthetic `passed` runtime acceptance records are produced.
