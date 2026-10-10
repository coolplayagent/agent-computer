# 56. Browser Pod and node runtime

The [private worker bridge](55-browser-worker-bridge.md) now has a Rust controller protocol, Kubernetes Browser plan/attach adapter, and a separate node process guard. These are internal components. The public App API queues admissions for the [durable launch controller](57-browser-controller.md); GUI authority/action journals and profile checkpoint/restore remain pending. Browser readiness is unchanged.

## Separate Pod

`BrowserPlan` requires the admitted organization, Computer/App generation, actor, profile owner, viewport, remaining runtime, approved image digest and exact Node UID/boot. Its Pod has no Candidate/PVC mounts or service-account token. It uses gVisor, a non-root identity, no capabilities, a read-only root, fixed CPU/memory limits and the deployment's deny-all policy. The plan is data for review; it does not grant permission to create a Pod.

The approved image supplies Node at `/usr/local/bin/node` and the compiled worker at `/opt/agent-computer/browser-driver/worker.js`, with matching Playwright and sandbox-enabled Chromium. The same image runs `--prepare-profile-volume` as a non-root init container. It creates one fresh 0700 leaf in a 512 MiB memory volume; the main container mounts that leaf at `/browser-state`. Preparation fails on an existing leaf or a foreign/symlinked root. `/tmp` is a separate 512 MiB memory volume and `/dev/shm` is 64 MiB. This private profile is volatile.

Strict readback binds the manifest, successful non-restarted init, running main container, Pod UID and Node UID/boot. Known Kubernetes init-container defaults are accepted; unknown security fields remain rejected. Binary memory quantities are emitted in Kubernetes canonical form so `4096Mi` and its API representation `4Gi` cannot cause false identity failures.

## Private controller and live guard

The adapter opens Kubernetes v5 attach once, validates the nonce exchange and rechecks the original runtime identity. It never reconnects or retries writes. The Rust `Session` bounds bytes, frames, calls and elapsed time; it correlates callback IDs, scope, actor, permission, epochs and complete action input before invoking its mandatory `Policy`. Read results cannot cross control epochs. Screenshots require bounded base64, matching PNG header/dimensions and SHA-256; DOM observations remain untrusted, bounded data. Unknown action completion blocks new control and preserves retry identity.

`Policy` must authenticate the original caller, check current database authority and the live node guard, persist a unique intent before dispatch, and persist the result before acknowledging completion. Protocol strings and callback fixtures do not provide those permissions. A production caller must also recheck publication authority; this library does not supply a PostgreSQL policy adapter.

`arm_browser` checks exact CRI/containerd/runsc identities, the profile subPath's private tmpfs inode, original process start times, cgroup and pidfds. It arms two independent watchdogs and obtains reaper admission before any launch callback may be granted. Shared guard machinery retains the Candidate API; Browser evidence has its own type and cannot produce a Candidate IO seal.

`stop_browser` terminates and observes the original process domain, retains the private profile inode, and atomically records a distinct `browser-stop` receipt in the private node spool. The receipt establishes historical process termination only. It cannot restore live handles, grant GUI control, publish a checkpoint or release a database reservation by itself. Worker `close`, EOF and Pod deletion remain insufficient proof.

## Component verification

`//crates/browser:browser_contracts_test`, Kubernetes and node unit targets cover hostile callbacks, grant/order mismatches, uncertain acknowledgements, manifest changes, profile mounts and incompatible receipt kinds. The opt-in `//crates/node:browser_live_test` runs on a disposable K3s node with the [additional scoped RBAC](../../deploy/testing/browser-component-rbac.yaml). It exercises an actual init container, sandboxed Chromium, private attach, node guards and physical stop. Its policy is explicitly a fixture; it does not certify production database authorization or App readiness.

The fixture performs UID-preconditioned immediate API cleanup only after retaining a live process-stop proof. This prevents stale kubelet accounting from rejecting the next 3-CPU fixture; it is not a product force-delete path. The deadline case keeps its idle pipe open until the original watchdog deadline, so EOF cannot substitute natural shutdown for expiry.

See the [source-bound validation record](../evidence/browser-runtime-2026-10-11.json) for all test results, actual runtime readback and limitations.

## Controller integration

[The durable controller](57-browser-controller.md) now supplies one-shot Pod creation and database launch authorization using these components. GUI control and profile checkpoint/release remain unavailable.
