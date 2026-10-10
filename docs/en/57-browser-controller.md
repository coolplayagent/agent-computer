# 57. Durable Browser launch controller

A trusted node operator can consume a [queued App admission](54-app-admission.md) through `browser-dispatch-once`. Migration 32 journals the immutable Pod plan before one creation attempt, the original Pod UID, live node guard, one launch authorization, launch observation and physical process stop. The fixed App admission deadline never moves. No historical record can recreate a creation permit or a live guard.

## Lifecycle and authority

`Queued → Dispatching → Running → Draining` is separate from undispatched `Queued/Blocked → Cancelled`. Dispatching, Running and Draining all retain the Computer/App, Sandbox and Profile reservations. Revocation of the original credential, principal, connection or admission grants is sticky and drains a dispatched instance. Restoring access does not revive it. A current read reconciles admission and registered watchdog expiry. Cancellation rejects any dispatched instance, including when creation, attach, launch or completion is uncertain.

The controller compiles CPU/memory, approved image and network reference from the immutable admitted Sandbox. Operator configuration binds namespace UID, Node UID/boot and viewport. It checks Kubernetes identities, attaches privately, arms the original node process domain and persists its evidence before granting launch. The callback checks the original credential, connection, grants, catalogs, Computer generation, exact actor/scope, and live watchdog again. Authorization commits before the grant is sent; losing a reply never permits another launch.

The first controller uses a fixed watchdog slice of at most **30 seconds after arm**, also bounded by the original App deadline. Longer admitted budgets do not imply renewal. Current authority is polled every 200 ms, with a two-second database call bound. A database stall stops further authorization; fixed node watchdogs remain independent of the controller. This command also bounds database-pool shutdown to two seconds, so canceled blocked queries cannot indefinitely delay operator exit. SIGTERM/SIGINT requests drain; a crash retains the journal and reservation. The server grants no observe/control callbacks and performs no GUI action.

## Operator commands

```bash
agent-computer-server browser-dispatch-once --database-url-file /private/database-url --organization acme --instance-id INSTANCE --expected-revision 1 --config-file /private/browser-controller.json
agent-computer-server browser-inspect --database-url-file /private/database-url --organization acme --instance-id INSTANCE
agent-computer-server browser-drain --database-url-file /private/database-url --organization acme --instance-id INSTANCE
```

The dispatch config has `api_url`, private `ca_file` and `token_file`, the existing Kubernetes `deployment` binding, and `browser` with `approved_image`, `viewport` and the [trusted node configuration](56-browser-runtime.md). Run on the bound root-owned node with its qualified runtime and reaper. No database or Kubernetes credentials enter the Browser Pod. Inspection is an operator-only historical journal, not a replay instruction. Drain is a monotone request; it never reports physical completion by itself. There is no automatic queue adoption or creation retry.

After every outcome the original guard attempts physical termination. Only `StoppedBrowser`, retaining the original kernel handles, can record a database process-stop entry. The controller requests ordinary UID/resourceVersion-conditional Pod deletion separately; API absence is not termination proof. A lost database stop acknowledgement leaves the reservation held. The private node spool supports operator diagnosis, but serialized receipts cannot manufacture live handles.

## Remaining scope and validation

`Running` records the last accepted launch observation. After controller loss, reads converge to Draining at the original registered watchdog expiry without asserting physical completion; `ready` remains false and public Browser capability remains unsupported. The profile is private volatile tmpfs. Process stop does not establish a durable profile checkpoint, release its slot or authorize Computer stop/recovery. Continuous scheduling, watchdog renewal, GUI control epochs/action journals, observation publication, profile checkpoint/restore and ComputerView remain pending.

PostgreSQL contracts cover concurrent one-shot dispatch, restart/lost acknowledgement, exact original UID, immutable history, grant/credential/principal/connection revocation, expiry, rejected cancellation and retained resources. The opt-in `manual_browser_controller_live` test uses the actual server process, Store, K3s, Chromium and node watchdogs for normal expiry, revocation, database stalls and controller death. Its existing PreparedComputer/storage prerequisite is explicitly synthetic; it does not certify Workspace/CSI or profile checkpoint semantics. Run it only with the explicit disposable-VM fixture and `--ignored --exact` filter. The controller-death case verifies absence of the original processes with the actual transport closure; it does not isolate EOF handling from watchdog expiry. Independent watchdog failure isolation is covered by the [node component validation](56-browser-runtime.md). Captured operator output is read only after its producer terminates.

[Source-bound validation record](../evidence/browser-controller-2026-10-11.json) · [Test log](../evidence/browser-controller-2026-10-11-tests.log)
