# 32. Redundant node watchdogs

## 32.1 Single-process fault protection

The [node adapter](31-node-guarded-startup.md) now arms two watchdog processes before allowing a Candidate startup grant. Both execute the same pinned binary in independent sessions and acknowledge the same execution, boot, cgroup path/inode and absolute `CLOCK_BOOTTIME` deadline. Their cgroup device observations must also agree. Starting the second process consumes the original inspection and execution budgets; it never renews either timer.

The live handle requires both children to remain available before registration, grant issuance and each worker authority poll. `waitid` observes exited or stopped children with `WNOWAIT`, preserving their identities until reaping. Either observation fails authorization even if the other timer remains armed. If the controller disappears, one surviving watchdog can still terminate the original target after its peer receives SIGKILL or SIGSTOP. A failure while starting the second watchdog leaves the first timer armed and returns no live pair.

Dropping the handle closes its receipt readers and detaches waiters without sending termination or renewal signals to the watchdogs. Neither process depends on peer communication or a controller heartbeat. The single-process watchdog request/receipt protocol and fixed deadline remain unchanged.

## 32.2 Durable registration and migration

Migration 17 requires version 2 node evidence for new arm records: the original receipt, `backup_armed`, two distinct positive watchdog PIDs, and an observation time preceding their identical deadline. PID values are diagnostic metadata, not reusable signalling authority. Live handles continue to require the trusted Pod/runtime/Candidate binding from [31](31-node-guarded-startup.md); SQL fixtures alone cannot establish that binding.

The complete pair evidence is covered by the existing immutable arm digest. Both processes must still be live when the store registers the arm and authorizes startup. Database expiry and the startup grant remain capped by the original attempt and node deadline.

Migration preserves existing arm/grant records and their hashes. Historical single-process arms cannot authorize new grants after upgrade. Restart recovery cannot reconstruct a live pair, replace a failed timer, reset a deadline or release a writer. Drainage, output acceptance and writer release remain separate unfinished work.

## 32.3 Verification

Default tests exercise real child exit/stop observations, detached-handle behavior, PostgreSQL mismatched/missing backup evidence, duplicate/out-of-range PIDs, WAL readback and migration compatibility. The explicit kernel fixture uses the production pair launcher and four fresh owned cgroups: kill either watchdog, or stop either watchdog, then close both receipt readers and check that the surviving timer empties the stopped workload's cgroup at the original deadline.

Run the kernel fixture only as root in a disposable Linux VM with writable cgroup v2:

```bash
bazel build //crates/node:node_contracts_test //crates/watchdog:agent-computer-watchdog
AGENT_COMPUTER_WATCHDOG_BIN=/absolute/path/agent-computer-watchdog \
  /absolute/path/node_contracts_test --ignored --exact \
  guard::tests::redundant_timers_survive_either_guard_killed_or_stopped --nocapture
```

Upload the binaries into that VM before invoking the command. The default Cargo/Bazel suites leave this root-only fixture ignored. Its kernel observations are component evidence, not full Computer runtime acceptance.

The final source in `6cf9b82` passed all 307 default Bazel cases across ten targets, including 136 PostgreSQL cases, plus fmt and workspace Clippy. Its four explicit VM fault cases observed recursive empty state 1–9 ms after the original deadline. The [source-bound record](../evidence/redundant-watchdogs-2026-10-10.json) and [raw log](../evidence/redundant-watchdogs-2026-10-10.log) retain binary hashes, environment and boundaries. No watchdogs or fixture cgroups remained; the owned VM and ten private files were removed.

## 32.4 Remaining boundaries

This tolerates one watchdog process failure. It does not provide a persistent node service, automatic restart, multi-node routing, storage drainage or durable fencing. Both guards share a host and failure domains: a host failure, frozen parent cgroup, operator killing both, or correlated binary/kernel failure can defeat both timers. Independent sessions do not isolate service cgroup teardown. Operators must keep both guards outside the workload and controller teardown groups. A stopped guard needs trusted cleanup; a detached waiter does not resume or replace it.

`EmptyObserved` remains a point-in-time observation. It cannot prevent later process admission or certify asynchronous storage completion. Execution stays Unknown and its writer stays Draining; public execution capabilities and T01–T43 acceptance remain unchanged.
