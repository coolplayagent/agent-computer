# 34. Persistent node expiry recovery

## 34.1 Recovery of an existing termination intent

The [two node watchdogs](32-redundant-node-watchdogs.md) remain the primary fixed-deadline mechanism. A separate root service now scans the [durable journal spool](33-watchdog-journals.md), so both guards can exit or stop without losing the original termination intent. Restarting this service resumes expiry enforcement; it never starts a workload or issues a startup grant.

Each new journal gains an immutable `enrollment.json`. The watchdog writes it only after validating the current boot and opening the trusted original cgroup. It binds the journal device/inode and exact intent digest to the cgroup device. File and directory synchronization finish before the kernel timer is armed and before the armed receipt. Setup time consumes the original deadline. The node adapter requires matching enrollment before accepting the live guard. Unenrolled historical or partially initialized journals remain readable, but the service does not infer permission to act on them.

The service handles only `journal-*` directories in the configured private spool. On expiry it checks enrollment, intent digest, current boot, original cgroup path/device/inode, delegation permissions, domain type, and exclusion of its own cgroup/ancestors. Device validation occurs before constructing the cgroup handle whose drop performs best-effort termination. It never signals a stored PID. An old boot, replaced path, invalid record or unavailable cgroup cannot produce a successful recovery report. Already recorded empty observations are left unchanged.

After `cgroup.kill`, a populated tree remains unresolved and is retried in later passes. The service does not wait five seconds for each uninterruptible workload and thereby delay all other entries. Once the kernel reports empty, it synchronizes a separate immutable `recovery.json` with `trigger: Recovery`. Unique temporary names allow a later attempt after interrupted publication; incomplete files never count as success. Original `intent.json` and `report.json` are not overwritten. The Linux [cgroup v2 contract](https://docs.kernel.org/admin-guide/cgroup-v2.html) defines the recursive kill and populated observations; these do not seal future membership or drain the storage client.

## 34.2 Service lifecycle and observation

The service holds an exclusive lock on the spool directory. A second reaper fails rather than competing for final records; independently armed guards do not use that lock. A cursor advances through the entire directory in batches of at most 128 entries, then starts a new pass after 250 ms. A growing or slow spool increases scan latency; batches bound memory, not the duration of filesystem calls. Keep the spool on trusted node-local durable storage, retain unresolved records, and do not move or replace an active spool. Automated retention and capacity management remain pending.

The committed [systemd unit](../../deploy/systemd/agent-computer-expiry-reaper.service) uses `Type=notify`, `Restart=always`, 250 ms restart delay and a ten-second service watchdog. The scan loop sends readiness and watchdog notifications without depending on stdout or journald throughput. A crashed or stopped process can be replaced by the service manager; the unit uses SIGKILL for watchdog expiry. See the upstream [systemd service contract](https://raw.githubusercontent.com/systemd/systemd/v255/man/systemd.service.xml). Readiness means that the spool and lock opened; it is not a startup authorization or proof that every journal is healthy.

`systemctl show agent-computer-expiry-reaper.service -p ActiveState -p SubState -p StatusText -p NRestarts` exposes service state and unavailable entries in the current scan pass. `agent-computer-watchdog --reap-once --spool PATH` is a **mutating operator maintenance command**: it performs one complete scan, may kill expired targets, prints bounded JSON batches, and exits 2 when a record is unavailable or the lock cannot be acquired. It cannot run alongside the service. The normal `execution-recover-once` command remains observation-only for journals.

Recovery output adds `watchdog_journals.guards[].state: recovered` when the original guard has no report but the reaper has a matching report. When an original report also exists, `recorded` retains it and may include a separate `recovery` field. Bindings to the original database arm, request, original guard PID and cgroup device are rechecked. A recovery report's `armed_boottime_ms` marks the recovery attempt, not a reconstructed original armed receipt.

## 34.3 Operator installation

Run the service in the host namespaces, outside workload cgroups, on the same node and spool as the execution worker. Installation is explicit; there is no deployment side effect in the library or worker. Drain active dispatches before replacing the configured binaries. After building and reviewing the pinned artifacts:

```bash
sudo install -o root -g root -m 0755 \
  bazel-bin/crates/watchdog/agent-computer-watchdog /usr/local/bin/agent-computer-watchdog
sudo install -o root -g root -m 0644 \
  deploy/systemd/agent-computer-expiry-reaper.service \
  /etc/systemd/system/agent-computer-expiry-reaper.service
sudo systemctl daemon-reload
sudo systemctl enable --now agent-computer-expiry-reaper.service
sha256sum /usr/local/bin/agent-computer-watchdog
systemctl show agent-computer-expiry-reaper.service -p ActiveState -p SubState -p StatusText
```

The unit creates root-owned mode `0700` `/var/lib/agent-computer/watchdogs`. Set the execution worker's `node.spool` to that same path and update `node.watchdog.sha256` to the observed binary digest. If using another spool, update the unit's path/mount requirements and arrange equivalent private directory ownership before starting. Never expose this spool, binary invocation or service management to tenants. The validated component environment uses Linux 6.8 and systemd 255; other configurations require qualification.

## 34.4 Verification and limits

The root VM fixture exercises both guards killed/stopped, service SIGKILL and SIGSTOP with actual systemd replacement, interrupted report publication, duplicate reaper exclusion, device/inode/digest rejection, old boot, unenrolled journals, path reuse, nonprivate spool rejection and 302-entry scans. The node fixture additionally reads recovered results through the original two arm references and rejects a recovery report disguised as an original deadline report. Original pair, journal IO and kernel probes remain regression checks.

This is a fallback recovery mechanism, not a hard real-time deadline promise: a service watchdog must first detect a stopped process, and disk/kernel stalls or scan backlog can delay enforcement. The primary guard pair is still required. Service availability is not yet part of database startup admission. Full node reboot, power-loss durability, multi-node routing, storage fencing and the complete Kubernetes/CSI/worker path were not certified by these component tests. No report releases a writer or accepts execution output; Execution remains Unknown, writer remains Draining, and product T01–T43 remain `not_run`.
