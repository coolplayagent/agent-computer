# 18. Durable logical connection sessions

## 18.1 Identity and lifetime

Migration 9 adds durable ConnectionSessions for human and Agent principals. A session binds the stable Computer ID, authenticated organization/principal and exact creating service credential. Multiple principals can connect to one Computer; a principal can maintain separate connections to multiple Computers. Connecting creates no runtime control row, generation, storage preparation, Pod, lease or ViewerSession. Disconnecting leaves the Computer and its background work intact.

The requested lifetime defaults to 900 seconds and must be 1–3600 seconds. The database chooses the earlier of that deadline and the credential's original expiry. Heartbeats never extend either deadline. Sessions retain their logical identity across compute generations; a future effect must separately validate the current generation and its own authorization. Rotating credentials requires a new connection; a replacement credential cannot take over or inspect an old one, even for the same principal.

`Active` describes a logical connection, not a live transport, physical presence or Computer readiness. `Expired` is derived from database time. `Closed` and `Revoked` are terminal stored states. Their effective capabilities are empty. Closing advances both the connection revision and revocation revision; repeated closes leave events and revisions unchanged. Revoking a Computer's `connect` grant atomically marks its existing Active connections Revoked. Regranting access permits new sessions without reviving the old ones. Credential revocation, expiry and principal disable fail authentication on every endpoint.

## 18.2 API and permission intersection

All endpoints require an active service credential with `runtime.connect`. Only the original credential can read or mutate its own sessions. Browser `Origin` requests still fail closed while OIDC/browser authentication is pending. Session IDs are references, not bearer tokens.

| Endpoint | Contract |
| --- | --- |
| `POST /v1alpha1/computers/{id}/connection-sessions` | Exact Computer `connect` grant; required idempotency key; returns 201 with current session metadata |
| `GET /v1alpha1/connection-sessions/{id}` | Current own-session state and effective capability intersection |
| `POST /v1alpha1/connection-sessions/{id}/heartbeat` | Active session, expected connection revision and idempotency key; returns 200 |
| `DELETE /v1alpha1/connection-sessions/{id}` | Idempotent own-session close; no body/key required; returns 200 |

A minimal connection request:

```json
{
  "requested_capabilities": ["connect", "read", "observe", "modify"],
  "lifetime_seconds": 900
}
```

The capability list is bounded, duplicate-free and must include `connect`. Its order is normalized for idempotency. Organization, principal, credential, generation, AgentSpec and caller references are not accepted in the body. AgentSpec/caller extensions remain future work; neither is required for human or Agent connection.

Each response intersects the requested capabilities, the original credential's current scopes and current **Computer** runtime grants. Other requested permissions may be absent without failing creation. `max_runtime_seconds` is present only when `activate` survives that intersection, using the current grant cap. This response does not grant access to a Workspace, App or private profile; each effect must check all required resources independently. A permission also does not mean its backend operation is implemented. Use the service capability endpoint for that distinction.

```json
{
  "expected_revision": 1,
  "activity": "active",
  "visibility": "visible"
}
```

Heartbeat activity (`idle`/`active`) and visibility (`hidden`/`visible`) are explicitly self-reported. They are not verified input, frame acknowledgement, billing evidence or an idle-stop decision. Viewer frames and activity-based stop policy remain separate work.

## 18.3 Transactions, retries and bounds

Creation, heartbeat and close hold the organization lock and commit metadata, event and Outbox together. Credential/principal share locks serialize authentication with revocation; expiry is checked again after the writes. A failed write or late expiry rolls back the transaction. Heartbeats use connection revision CAS, independently of Computer control/definition revisions. Binding, requested capabilities and deadlines are immutable; terminal rows cannot be reopened or deleted by the normal store API.

Create and heartbeat retries retain the original session and return its **current** view. They do not return a cached Active permission snapshot, extend deadlines or repeat activity writes. Changed intent with the same key returns 409; new heartbeats for inactive sessions return 410; cross-credential/principal/organization access returns 404. Credential failure remains 401, missing scope 403. Unknown write outcomes require retrying the same key and input; DELETE is naturally idempotent.

New connections have fixed development ceilings of 256 live sessions per organization, 32 per principal and 64 per Computer. Counted sessions are Active, unexpired and bound to a currently valid credential/enabled principal. Admission is serialized; closing a connection frees only its connection slot. Session admission never releases compute/storage reservations. Historical session rows remain retained; retention/GC and configurable platform budgets are pending.

## 18.4 Verification and remaining work

Ten additional PostgreSQL cases cover durable/replayed connections, WAL restart, human/Agent ownership, credential/organization isolation, permission changes, terminal revocation, heartbeat races, expiry, immutable fields, rollback, admission bounds and migration checksums. Two additional HTTP cases verify lifecycle and rejected identity fields; the existing independent TCP-process test also exercises connect, heartbeat and close. These tests use actual PostgreSQL with durable settings.

Connection-owned Candidate writer lease authority is now implemented in [19 Writer leases](19-candidate-writer-leases.md). Closing atomically marks Held writer leases Draining; it does not prove physical completion. GUI control leases, physical draining/fencing, scoped connection tokens, OIDC, browser transport, ViewerSessions, activity-based stops or full Computer execution remain pending. T01–T43 runtime acceptance remains `not_run`. See [15 Runtime authorization](15-runtime-authorization.md), [16 Start admission](16-start-admission.md) and [17 Candidate preparation](17-candidate-preparation-worker.md).
