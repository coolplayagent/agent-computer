# 05. Verification and delivery evidence

## 05.1 Evidence boundaries

| Check | What it establishes | What it does not establish |
| --- | --- | --- |
| Bazel build/test | Current Rust targets compile and executed contract assertions hold | External services, real isolation, storage durability |
| Cargo fmt/Clippy | Current source formatting and static diagnostics | Product acceptance |
| Documentation checks | Local links, paired language files and numbering | Implementation of the linked designs |
| Qualitygate full | Selected policy checks on the delivery snapshot | Unselected tests; current policy only checks line endings |
| relay-knowledge map validate | Map structure, routes, and digest integrity | Complete index coverage or runtime correctness |
| T01–T43 runtime acceptance | Observed behavior for each test in a pinned real environment | Unexecuted tests or different deployment combinations |

## 05.2 Current record

2026-10-09 (stages 01–02 and static declarations in 03): Bazel builds, CLI JSON output and error exit codes, Cargo fmt/Clippy, local documentation links, and bilingual numbering passed; Bazel passes 63 tests (28 domain, 28 declaration, 7 CLI process tests). Python independently verifies the Draft 2020-12 schema and example SHA-256. Cargo tests and Clippy also pass. Runtime acceptance T01–T43 is entirely `not_run`. Existing T00 design checks do not establish product delivery.

Local Qualitygate reports are retained in ignored `.qualitygate/`. Before each commit, run `check --worktree --profile full` on the final worktree. Require a nonempty delivery, no pending checks, `gate.complete: true`, `gate.decision: pass`, and exit code 0, plus separate Bazel and static checks. Retain the report's snapshot/policy digests; the default line-ending check is not functional verification.

Runtime evidence follows the [acceptance record schema](../../codespec/test/agent-computer.md): `test_id/status/source_commit/component_digests/environment/input_refs/expected/observed/evidence_refs/limits`. No synthetic `passed` runtime acceptance records are produced.
