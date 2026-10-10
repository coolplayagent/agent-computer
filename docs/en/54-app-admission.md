# 54. Durable App admission

The control service can reserve a Chromium App in a Prepared Computer generation, inspect the reservation, and cancel it before dispatch. Migration 31 stores immutable App/Sandbox versions, a connection and credential binding, App generations, deadlines and exclusive resource slots. This is the persistence prerequisite for connecting the [Browser Driver](53-browser-driver.md). The separate [Browser controller](57-browser-controller.md) can consume an admission; these HTTP requests do not launch a browser or provide GUI control. `app.admission` is `bounded-queued`, `browser` remains `unsupported`, and every App response has `ready: false`.

## Requests and authority

| Operation | Endpoint | Authority |
| --- | --- | --- |
| Admit | `POST /v1alpha1/computers/{id}/apps` | Bound active connection; Computer connect/read/app.use/activate; App read/app.use/activate; Profile app.use and explicit subject ownership |
| Inspect | `GET /v1alpha1/app-instances/{id}` | Original principal with current Computer/App read grants and runtime.read scope |
| Cancel | `POST /v1alpha1/app-instances/{id}/cancel` | Original principal with current Computer/App manage grants and runtime.manage scope |

Admission requires every corresponding credential scope and the connection's requested Computer capabilities. No permission implies another. Both POST operations require an idempotency key and uncompressed JSON of at most 4 KiB; unknown identity, owner, readiness, deadline and binding fields are rejected. Browser Origin credentials are not accepted. The [OpenAPI contract](../../schemas/openapi-v1alpha1.json) defines the response and errors.

```json
{
  "connection_session_id": "conn_example",
  "computer_generation": 1,
  "app_id": "app_definition_example",
  "expected_app_revision": 1,
  "runtime_seconds": 300
}
```

IDs are illustrative; use the actual connection, Computer generation and pinned App definition. Admission checks the App appears in the Computer's immutable start snapshot, the exact revision matches, the driver is `chromium-playwright`, and its Sandbox is `gvisor`. Catalog references are revalidated. Current Computer and App activation grants, the original Computer runtime reservation and the bound connection cap the requested interval (1–300 seconds). The fixed deadline starts at admission, including queue time; reads and retries never extend it. This is a reservation limit, not a physical process watchdog.

BrowserProfile catalog registration and app.use grants do not assign a profile subject. A trusted administrator must explicitly call `Store::register_browser_profile_owner` for an enabled principal and existing enabled profile. That association is immutable and cannot be transferred through this API. The authenticated principal must own the referenced profile as well as possess its current grant. Migration does not infer owners from creator privileges or organization membership. No cookies, profile contents, checkpoint or restore result are created by admission.

Trusted operators can register the subject without embedding a Store client:

```bash
agent-computer-server browser-profile-owner-register --database-url-file /private/database-url --organization acme --resource-id PROFILE_ID --principal alice
```

The private database URL file and existing enabled principal/profile are required. Repeating the same owner is idempotent; replacing the owner fails. The command does not grant app.use or launch a runtime.

## Reservation and recovery

The organization transaction lock serializes admission, cancellation and resource grant changes. One outstanding instance is allowed per Computer/App, one per prepared-request/Sandbox, and one per organization/Profile. Different Apps sharing a Sandbox cannot spend the same reserved CPU/memory twice; execution admission already excludes App Sandboxes. All records except Cancelled count toward limits of 64 per organization and eight per principal. App generations increase across cancellations and Computer generations; history cannot be deleted or rewritten through the Store or SQL mutation guards.

Undispatched reservation states are `Queued → Blocked → Cancelled` or `Queued → Cancelled`. Credential revocation, principal disable and connection close/revoke persist Blocked in the same transaction. Removing a required grant or reducing its activation budget blocks the reservation permanently, even if access is later restored. Reads and exact retries also check the original identity, connection, grants, catalogs, Computer generation and deadline; expiry or lost authority becomes Blocked. Blocked continues to reserve its slots until explicit cancellation. It is not proof of process termination; the separate controller uses Dispatching, Running and Draining, all of which retain the same slots. Dispatched instances cannot use admission cancellation.

Cancellation supplies `app_generation` and `expected_revision`. Exact retries return the original record in its current state without creating another instance. A fresh credential for the original principal may inspect or cancel using its current grants, but cannot inherit the original connection or revive a blocked admission. A Computer cannot stop or seal its Candidate while any App reservation remains; the database guard and Store preflight enforce this even for expired records. Cancel the undispatched App first.

Events contain identifiers, versions and binding digests. HTTP metadata omits pinned image/command bodies, service credentials, local profile paths and profile data. Database credentials and trusted Store administration stay outside the future Browser Pod.

## Verification boundary

PostgreSQL and HTTP contracts cover WAL restart, migration from version 30, idempotency, concurrent ownership, immutable profile ownership, stale generations, independent scopes, credential/grant/connection invalidation, fixed expiry, cancellation and Computer stop/seal blocking. A separate HTTP case uses two App definitions with different profiles and the same Sandbox to verify exclusive budget reservation. Storage preparation receipts in these fixtures are explicitly synthetic. They prove database and service behavior; they do not launch Chromium or certify a deployment.

The [controller](57-browser-controller.md) now journals an actual Pod/launch and records live node stop evidence. Persistent GUI control epochs and action journals, profile checkpoint/restore and ComputerView remain pending. Queued App records are not a production `Authority` or `ActionJournal` adapter. Full D09 and T04/T05/T10/T24 acceptance remain incomplete. This increment's [source-bound verification record](../evidence/app-admission-2026-10-11.json) keeps these limits explicit.

[Browser worker bridge](55-browser-worker-bridge.md) provides the private process transport; persistent controller and node integration remain pending.
