# 15. Resource runtime authorization

## 15.1 Delivered scope

Migration 6 adds resource runtime grants independently of declaration creation, management and reference grants. An operator can grant or revoke access to an exact Computer, Workspace, App or browser profile. A credential must also carry the corresponding API scope. Neither creating a definition nor sharing its organization grants runtime access. Existing credentials retain their original scopes after migration.

The store provides a transaction-scoped authorization function for subsequent lifecycle admission, a standalone current-permission check, and a current effective-access view. The server exposes that view and trusted local grant commands. These surfaces do not allocate a generation, prepare a Candidate, start a Pod, reserve a modification lease or certify that a process has stopped. [16 Start admission](16-start-admission.md) subsequently adds generation allocation and capacity reservations. Immutable Artifact input selection, Candidate receipt consumption and physical fencing remain unfinished.

## 15.2 Permissions and credential limits

The credential scopes are `runtime.connect`, `runtime.read`, `runtime.observe`, `runtime.app.use`, `runtime.activate`, `runtime.execute`, `runtime.modify`, `runtime.control`, `runtime.publish`, `runtime.manage` and `runtime.delete`. They are distinct from `definitions.validate` and `definitions.manage`; issuance accepts a nonempty subset of these thirteen scopes. Scope names are exact and case-sensitive.

| Target kind | Supported grant permissions |
| --- | --- |
| `computer` | `connect`, `read`, `observe`, `app.use`, `activate`, `execute`, `modify`, `control`, `publish`, `manage`, `delete` |
| `workspace` | `read`, `modify`, `publish`, `manage`, `delete` |
| `app` | `read`, `observe`, `app.use`, `activate`, `control`, `manage`, `delete` |
| `browser_profile` | `read`, `app.use`, `manage`, `delete` |

No permission implies another. In particular, `manage` does not grant read, control or execution, and GUI `control` does not grant shell execution or file modification. There is no wildcard, name-based matching, inheritance to a related resource, or organization-wide default. Operations touching multiple resources must check every required target/action; each check batch accepts 1–32 distinct requirements. A Computer grant does not disclose a profile; profile use and profile reading are separately granted. Disabled catalog profiles are unavailable even while old grant rows remain.

An `activate` grant requires `max_runtime_seconds` between 1 and 86,400. The authorization request must supply a positive duration no larger than that cap. Other grants carry no duration. This is a per-activation authorization limit, not an aggregate CPU/memory budget, accounting reservation, elapsed-time watchdog or guarantee that activation is currently possible. The eventual runtime must separately enforce those conditions.

## 15.3 Transaction and revocation contract

Authorization derives organization and principal from the credential, takes the organization stream lock, locks and rechecks the credential/principal rows, verifies each target and grant, and checks the credential again before returning. Admission code must call the transaction-scoped function inside the same transaction that persists the authorized effect. A returned Boolean or effective-access response must never be reused as a permit for a later write or backend dispatch.

Runtime grant changes use the same stream lock. Credential revocation and principal disable serialize through the shared row locks. A revocation that wins the relevant lock prevents the waiting check from succeeding. A request already admitted may finish; downloaded data cannot be recalled. Existing execution cancellation, active session closure and termination verification require later runtime workers.

Grant changes, the ordered event and Outbox entry commit together. Repeating an identical grant, cap or revocation emits no additional event. Cap changes emit `runtime.permission_changed`; removal emits `access.revoked`. Events explicitly report `process_termination_confirmed: false`. An Outbox failure rolls back the grant change and event. These administrative commands require trusted database access and are not exposed as self-service HTTP writes.

## 15.4 Operator commands

First publish the resource through [plan/apply](10-plans-and-apply.md), or register a browser profile catalog reference. Use the resulting stable resource ID, without the declaration syntax's `id:` prefix. The principal must already exist and be enabled; credential issuance registers that identity. Grant metadata contains no bearer token or profile content.

```bash
agent-computer-server credential-issue \
  --database-url-file /private/control/database-url \
  --organization org_example --principal person_example --kind human \
  --scopes runtime.read,runtime.observe,runtime.activate \
  --ttl-seconds 3600 --output /private/control/runtime-token

agent-computer-server runtime-grant \
  --database-url-file /private/control/database-url \
  --organization org_example --principal person_example \
  --kind computer --resource-id res_actual_computer \
  --permission read

agent-computer-server runtime-grant \
  --database-url-file /private/control/database-url \
  --organization org_example --principal person_example \
  --kind computer --resource-id res_actual_computer \
  --permission activate --max-runtime-seconds 3600

agent-computer-server runtime-revoke \
  --database-url-file /private/control/database-url \
  --organization org_example --principal person_example \
  --kind computer --resource-id res_actual_computer \
  --permission activate
```

Revocation omits the duration. Reducing a cap uses `runtime-grant` with the new cap. A removed or disabled profile may still have its stale grants revoked. Private credential/database file rules remain those in [09 Control service](09-control-service.md).

`GET /v1alpha1/runtime-access/{kind}/{id}` requires both `runtime.read` and a read grant on that exact target. The result lists only the caller's grants intersected with the scopes of the supplied credential; it exposes an activation cap only when that credential can activate. An illustrative response is:

```json
{
  "kind": "computer",
  "resource_id": "res_actual_computer",
  "permissions": ["activate", "read"],
  "max_runtime_seconds": 3600,
  "checked_at_ms": 1791550000000
}
```

Organization and principal cannot be selected through the path, query or body. Invalid credentials return 401, missing `runtime.read` or a browser Origin returns 403, and missing/inaccessible/disabled targets return the same 404. Responses are not cached. This development endpoint accepts service credentials; browser login and OIDC remain pending. The schema is included in [OpenAPI](../../schemas/openapi-v1alpha1.json). `auth.runtime_grants` is advertised as `control-plane`; `computer` remains unsupported.

## 15.5 Verification and remaining work

Ten PostgreSQL cases cover scope/grant independence, exact target/organization/principal matching, private profiles, cap validation and tightening, multi-resource checks, invalid or duplicate requests, atomic event/Outbox rollback, no-op retries, principal disable and competing grant/credential revocation, and migration from the preceding constraint/history with existing credentials unchanged. One additional HTTP case covers scope intersection, hidden targets, Origin rejection, forged principal queries and revoked credentials. The existing independent TCP process case now also exercises real `runtime-grant`, access inspection and `runtime-revoke` commands.

Run the [database-enabled Cargo/Bazel suites](08-persistence.md). These checks establish current authorization metadata behavior. Runtime admission must still bind current permission, generation, immutable input, storage evidence, leases and confirmed fencing before permitting a writer. None of these tests promotes T01–T43 runtime acceptance from `not_run`.
