# 55. Browser worker bridge

The internal Browser Driver now has a finite stdio worker process. It carries authority and action-journal callbacks to a trusted controller, so the Browser Pod need not contain database, Kubernetes or service credentials. This is a process boundary component, not the App launch controller: the [queued App API](54-app-admission.md) still does not dispatch, and Browser capability remains unsupported.

## Entrypoint and binding

Build `//packages/browser-driver:worker`, or compile the pinned TypeScript package and invoke `dist/src/worker.js --stdio-binding-json '<binding JSON>'`. The operator supplies a persisted binding with exactly `scope`, `actor`, `viewport` and `runtime_ms`; their shapes are the [driver contract](../../packages/browser-driver/src/contracts.ts). Scope includes organization, Computer generation, App instance/generation and profile owner. Runtime is 1,000–300,000 ms. A future controller must pass the remaining admitted budget, not restart the original duration after queueing.

The image must supply matching Playwright/Chromium and `/browser-state` as a private local directory owned by the worker with mode 0700. Profile paths, executable paths, image names, Chromium flags, arbitrary scripts and credentials cannot be supplied over the wire. Each launch creates a new private profile beneath that fixed root. This worker does not restore or checkpoint it. `serveBridge` is the library form for a trusted embedding caller; its optional launch factory exists for controlled composition/testing and is not a protocol field.

## Framing and sequence

Each message is a strict UTF-8 JSON object followed by LF. Unknown/missing envelope fields, invalid operations and malformed frames terminate the session. Protocol version is 1. The controller starts with `hello` containing `version`, `kind` and `binding_digest`. Compute the digest using exported `bindingDigest`: SHA-256 of `agent-computer/browser-bridge/v1`, a NUL, and sorted-key JSON of the normalized binding. This is correlation, not authentication.

The worker answers with a random `session_id`, `kind: challenge`, `version` and the same binding digest. Every later message carries that session ID. Requests contain `version`, `kind: request`, `session_id`, a consecutive integer `request_id` starting at 1, and `operation`. Operations are:

| Method | Additional operation fields |
| --- | --- |
| launch | None; only once, before all other operations |
| pages | actor |
| observe | actor, page_id |
| act | actor, action_id, action |
| close | None; terminal best-effort closure |

Only one operation runs at a time; there is no request pipelining. A following request received during the final result write is buffered once to tolerate transport callback ordering. Page/observation/action validation remains in the driver. A screenshot result replaces the internal Buffer with `screenshot_base64`; the SHA-256 still covers decoded PNG bytes.

The worker sends `kind: callback` with version/session/request IDs, a monotonically increasing `callback_id`, `method` and `payload`. Methods are `authorize`, `begin` and `complete`; payloads use `AuthorizationRequest`, `ActionIntent` and `ActionReceipt`. The controller replies with exactly version, `kind: reply`, session/request/callback IDs, the same method, `ok` and `value`. A denial is `ok: false, value: null`, without an upstream error string. Success values are the driver's Grant, journal dispatch/replay decision, or `null` for completed persistence. Missing, duplicate, unsolicited and mismatched replies cannot release a pending callback.

A result has version/session/request IDs, `kind: result`, `ok` and `value`. Failures carry only a bounded driver error code or `bridge_failure`. A successful action envelope can still contain `recorded: false` or an `unknown` receipt; inspect both fields. `close` returns driver state and `physical_termination: false`, including when the driver reports closed. CLI exit 0 means this close request completed; other termination exits 1 (invalid CLI shape exits 2). Neither exit nor a serialized result proves node-wide fencing.

## Trusted controller obligations

The pipe is private to the already admitted controller. Wire IDs do not authenticate a principal. Before accepting any callback, the controller must compare scope, actor, permission, action ID, control epoch and complete normalized action with its own current operation. It must check original credential/connection, current grants, App/profile ACLs and remaining generation/budget on every authorization. It must commit a unique immutable intent before replying dispatch, and commit the result before acknowledging completion. An existing unresolved intent must return an Unknown/reconciliation result, never another dispatch.

Do not treat the driver digest alone as a database identity check. Retain full normalized input and a server-side digest; cross-language JSON numeric encodings may differ. A denied/lost completion acknowledgement produces `recorded: false` and Unknown, blocks new control, and cannot cause a repeat effect. Reconnecting, restarting this executable, changing request IDs or allocating a new action ID does not prove a prior effect was absent. The durable controller must reconcile its original intent and physical runtime before replacing it.

This increment has no PostgreSQL callback adapter, GUI control-epoch implementation, Kubernetes Browser plan/attach adapter or node process guard. Those are mandatory before connecting public App dispatch/Ready. The existing execution node guard requires a Candidate mount and cannot be reused by inventing Candidate identity for a Browser. A Browser needs its own Pod and private profile storage. No fixture callback grants production authority.

## Bounds and validation

Input is limited to 64 KiB per frame, 16 MiB per session and 16,384 frames; output to 3 MiB per frame and 64 MiB per session. There are at most 1,024 operations. Handshake has 15 seconds; the immutable runtime deadline begins at the hello and includes launch delay. The driver separately consumes launch/operation grants. Writes have a 10-second deadline. EOF, transport failure, protocol failure and timeout reject pending callbacks and start closure. Cleanup waits at most 20 seconds; an uncertain browser close still requires independent node fencing. The input pipe must not be reused.

`bazel test //packages/browser-driver:bridge_test` checks framing, correlation, concurrency, quotas, disconnection, callback abort and error redaction without Chromium. `//packages/browser-driver:bridge_browser_test` is manual and requires the same sandbox-enabled Chromium as the original browser target, plus a private `/browser-state`. It exercises the actual worker executable over OS pipes, PNG transport, journal ordering, replay, lost completion acknowledgement, wrong-scope authorization and disconnect. Its controller callbacks are test fixtures, not a deployed authority/journal service. The [source-bound validation record](../evidence/browser-worker-bridge-2026-10-11.json) records actual gates and environment limits.
