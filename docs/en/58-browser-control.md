# 58. Browser control, observations and command queue

The [durable launch controller](57-browser-controller.md) now consumes authenticated GUI requests serially. Migration 33 stores a single control epoch, a bounded queue and immutable action intents/receipts. Execution still requires the original private attach session and live node guard. This increment adds HTTP endpoints; ComputerView, WSS input and public Browser capability certification remain pending.

## Authority and handoff

The credential determines identity. Its active connection must belong to the same principal and credential and target the original Computer. Observation requires Computer/App read and observe plus explicit profile read. Control additionally requires Computer/App control and app.use plus profile app.use. The connection must request the corresponding capabilities. Workspace membership, read access and operator history never confer GUI control.

All pages share one controller. Acquisition compares `expected_epoch` (initially 0); competing claims have at most one winner. A 1–30000 ms lease is capped by the original App and node arm deadlines, with no renewal. Revocation, connection closure, expiry and explicit release drain the old owner irreversibly. Regranting access does not revive it.

After the current request finishes, the original worker sends a serial private barrier. The driver releases discrete keyboard/mouse inputs and invalidates observations. Store accepts Idle only with a response bound to the original App, operation, drain claim and live guard. Unknown actions or lost/failed barrier acknowledgements block handoff. A new owner increments the epoch and observes again; old frames cannot authorize input. Held input, drag and WSS input batches are outside this increment.

## HTTP contract

All paths have the `/v1alpha1` prefix. Supply a service Bearer credential; remote access uses the existing TLS proxy boundary. Origin requests remain rejected: these endpoints are not browser frontend authentication.

| Endpoint | Behavior |
| --- | --- |
| `GET /app-instances/{id}/browser/control` | Current metadata, or null before the first lease |
| `POST /app-instances/{id}/browser/control` | Idempotent acquisition |
| `POST /app-instances/{id}/browser/control/release` | Requests draining; 202 is not handoff completion |
| `POST /app-instances/{id}/browser/commands` | Idempotently queues pages, observe or act |
| `GET /browser-commands/{id}` | Reauthorizes access to the original request/result |

Acquisition takes `connection_session_id/app_generation/expected_epoch/lifetime_ms`; release takes `connection_session_id/control_epoch`. Commands take `connection_session_id/app_generation/control_epoch/command` and reject identity or dispatch-authority overrides. Example:

```json
{"connection_session_id":"session_example","app_generation":1,"control_epoch":1,"command":{"kind":"act","action":{"kind":"create_page"}}}
```

pages returns metadata; observe takes `page_id` and returns a correlated PNG, digest, DOM references and viewport. act uses the [driver contract](53-browser-driver.md), including page/observation references except for page creation. Limits are 128 pending and 1024 lifetime requests per instance, 64 KiB input, 3 MiB per result and 64 MiB total result data. Claims never adopt another attach session or retry transmission.

## Uncertainty and private results

A unique dispatch claim commits before transmission. Each action then commits its entire intent and rechecks current control before input. `delivered` means driver input completion, not website business success. Missing completion remains Unknown. Exact retries query the original request without repeating input. An immutable completed receipt can recover the query result when a later state acknowledgement is lost; it cannot restore control or redispatch.

Observations recheck authorization both at publication and retrieval, bind credential/connection, App/Computer, operation and epoch, and deduct processing time. Expired or handed-off frames cannot be retrieved. Action receipts remain readable during App draining under current read permissions. The control database contains sensitive page results and action input; events contain identifiers, epochs and states, without screenshots, DOM or entered text. This increment does not add result retention/cleanup administration.

The original node slice remains at most 30 seconds from arm. Running is a historical launch observation and `ready` remains false. The Kubernetes template still denies all network traffic, so public website navigation and controlled egress remain pending. End-to-end acceptance here covers blank-page creation, screenshots, closure and handoff; separate real Chromium driver tests cover DOM input. Profiles remain volatile; process stop proves neither checkpoint durability, reservation release nor a complete Computer lifecycle.

## Validation evidence

The [source and runtime record](../evidence/browser-control-2026-10-11.json) and [test output](../evidence/browser-control-2026-10-11-tests.log) bind full repository checks, three live control cases, twelve real Chromium cases and four worker cases to their artifacts. The record separately identifies the final rejected-observation query correction after the live run and its PostgreSQL regression. The disposable VM, private overlay and credentials were removed after original-process and Pod absence checks.
