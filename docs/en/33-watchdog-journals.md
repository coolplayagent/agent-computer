# 33. Durable watchdog observations

## 33.1 Intent before authorization, report after termination

The [redundant node guards](32-redundant-node-watchdogs.md) now retain a private journal for each watchdog under the configured node spool. Before arming, the watchdog publishes `intent.json` with its fixed request and diagnostic PID, synchronizes the file and directories, and returns a journal reference in its armed receipt. That reference includes the directory device/inode and the exact intent SHA-256. The node adapter reads it back and checks the request and child PID before returning a live pair. The existing immutable database arm digest covers both references; no database migration is needed. Historical arms remain readable without inventing local journals.

The original absolute deadline still governs termination. Journal setup consumes that budget and cannot yield startup authorization after expiry. Once the timer is armed, no journal IO runs on the wait/kill path. After killing the pinned cgroup and obtaining the bounded kernel observation, the watchdog publishes `report.json`, then attempts its final pipe frame. Closing the controller's pipe can lose that frame without losing a successfully synchronized report. A report write error leaves no successful CLI acknowledgement and grants no replacement execution.

Publication uses exclusive temporary files, file synchronization, `renameat2(RENAME_NOREPLACE)` and directory synchronization. Existing final or interrupted temporary records are never overwritten. Readers also synchronize the opened file/directory to close the window between rename visibility and writer directory sync. The Linux [fsync](https://man7.org/linux/man-pages/man2/fsync.2.html) and [rename](https://man7.org/linux/man-pages/man2/rename.2.html) contracts define these operations; storage must honor their guarantees. This is not a tested power-loss SLA.

## 33.2 Recovery output

The existing operator command now also reads the original arm's journals:

```bash
agent-computer-server execution-recover-once \
  --database-url-file /private/database-url \
  --organization ORG --execution-id EXECUTION \
  --config-file /private/execution-config.json
```

Run as root on the configured node with the original spool. The command first lowers database authority and conditionally cleans up the original Pod. It then returns `watchdog_journals.guards` with one state per original guard:

| State | Meaning |
| --- | --- |
| `recorded` | A bounded, identity-matching local report was read; its observation is `EmptyObserved` or `Unknown` |
| `recovered` | A matching report from the persistent expiry service exists; the original guard report is absent |
| `unconfirmed` | The original intent is valid but no final report exists; a `.pending` file does not count |
| `unavailable` | The local record, permissions, digest or binding cannot be verified |
| `legacy_unjournaled` | The historical arm did not contain a journal reference |

No arm yields a null `watchdog_journals`. Failure of the overall reader is reported through `node_error`. The asynchronous reader has a five-second wait budget after cleanup; a blocked filesystem syscall itself is not cancellable and runtime shutdown may still wait for that thread. Recovery reads can be repeated, but they never run a watchdog, signal a PID, reconstruct a startup grant, accept execution output or release a writer. Dispatch results do not wait for final journals and keep this field null.

The read API returns an observation snapshot, not a writable journal handle. The journal writer is consumed by the single run. A report must match the fixed request/reference, original armed time and cgroup device, ordered timestamps, and consistent observation/error fields. PIDs remain diagnostic metadata, not signalling authority.

## 33.3 Operator storage contract

Use the existing private `node.spool` configuration from [31](31-node-guarded-startup.md), on node-local durable storage. New journal directories are explicitly mode `0700`; files are mode `0600`. Every path ancestor must be root-owned and not group/world writable. Traversal and symlinks are rejected. Readers require regular, singly linked files, prohibit descendant mount crossings, and bound each record to 8 KiB. Directory identities and intent digests are checked on every read.

The CLI also supports trusted standalone `agent-computer-watchdog --request PATH --journal EXISTING_PRIVATE_DIRECTORY`; omitting `--journal` preserves the original standalone probe protocol. Production node startup always requests journals and rejects a watchdog that cannot return the matching reference. Update the configured watchdog binary hash when installing this version.

Journals survive controller/child exit, including partial setup failures. Do not remove active or unresolved journals. Automated retention, quotas and off-node replication remain pending; the optional persistent expiry service is delivered in [34](34-node-expiry-reaper.md). Reading an old boot's report is historical evidence; device/path changes can make it unavailable, and no old PID is used for recovery actions.

## 33.4 Verification boundary

Default tests cover atomic non-replacement, interrupted publication, record bounds, confined IDs and report identity/time/error consistency. The root-only pair fixture additionally checks durable survivor reports after killing/stopping either guard and closing receipt readers, plus altered intent/report identity, wrong PIDs, broad permissions, hardlinks, symlinks and incomplete report files.

In a disposable root-owned VM, run the pair fixture from [32](32-redundant-node-watchdogs.md) and:

```bash
python3 crates/watchdog/tests/journal_component.py \
  --watchdog /absolute/path/agent-computer-watchdog --output /private/journal-result.json
python3 crates/watchdog/tests/component.py \
  --watchdog /absolute/path/agent-computer-watchdog --output /private/kernel-result.json
```

The journal probe covers normal persistence, closed/full output pipes, report publication failure, duplicate intent and nonprivate directory rejection. These component tests do not rerun the full Kubernetes/CSI/Candidate worker path, certify storage drainage, or establish durable fencing. Execution remains Unknown, writer remains Draining, and T01–T43 remain `not_run`.

The 2026-10-10 [source-bound record](../evidence/watchdog-journals-2026-10-10.json) and [raw logs](../evidence/watchdog-journals-2026-10-10.log) record 310 passing default tests (including 136 PostgreSQL cases) and 22 root VM component scenarios: four redundant guard faults, six journal IO cases and twelve kernel regressions. The initial directory-permission failure and its explicit `0700` fix are retained. Final VM binaries matched local Bazel hashes, and the owned VM, private key and writable disk were removed.

The persistent expiry service and separate recovery reports are described in [34](34-node-expiry-reaper.md).
