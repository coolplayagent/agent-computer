# 59. Authenticated App and Browser CLI

The CLI now exposes the [App admission](54-app-admission.md) and [Browser control](58-browser-control.md) HTTP contracts. Configure `AGENT_COMPUTER_ENDPOINT` and `AGENT_COMPUTER_TOKEN_FILE`, or pass `--endpoint` and `--token-file`. Use `doctor` to read deployment capabilities. Local `client.apps` and `client.browser` describe HTTP client support; full Computer and Browser runtime certification remains pending.

| Command | HTTP request under `/v1alpha1` |
| --- | --- |
| `app start <computer-id>` | POST `/computers/{id}/apps` |
| `app show <app-instance-id>` | GET `/app-instances/{id}` |
| `app cancel <app-instance-id>` | POST `/app-instances/{id}/cancel` |
| `browser control show\|acquire <app-instance-id>` | GET/POST `/app-instances/{id}/browser/control` |
| `browser control release <app-instance-id>` | POST `/app-instances/{id}/browser/control/release` |
| `browser pages\|observe\|act <app-instance-id>` | POST `/app-instances/{id}/browser/commands` |
| `browser status <request-id>` | GET `/browser-commands/{id}` |
| `browser screenshot <request-id> --output <new-file>` | Same authorized GET, then local PNG export |

## Explicit requests

POST commands take `--request <file|-> --idempotency-key <key>`. The exception is `browser control release`: supply `--request` only; the service compares the original session and control epoch. Passing an unused key is rejected locally. App cancel only accepts Queued/Blocked instances before dispatch; cancelling Running returns a conflict. Stopping a running App through the CLI remains pending. Control release initiates draining. Their successful HTTP response does not prove physical termination or handoff.

Start request example (substitute actual IDs and revisions):

```json
{"connection_session_id":"session_example","computer_generation":1,"app_id":"app_example","expected_app_revision":1,"runtime_seconds":30}
```

Acquire control explicitly after the App is Running:

```json
{"connection_session_id":"session_example","app_generation":1,"expected_epoch":0,"lifetime_ms":25000}
```

A command request carries the original session, App generation and acquired epoch. The CLI requires `command.kind` to match its verb; the service validates the complete typed request and current grants. For `browser observe`:

```json
{"connection_session_id":"session_example","app_generation":1,"control_epoch":1,"command":{"kind":"observe","page_id":"page_example"}}
```

For `browser act`, use an action from the [driver contract](53-browser-driver.md). Page creation is the only action that does not need a current observation:

```json
{"connection_session_id":"session_example","app_generation":1,"control_epoch":1,"command":{"kind":"act","action":{"kind":"create_page"}}}
```

Release request:

```json
{"connection_session_id":"session_example","control_epoch":1}
```

Each invocation sends one request. There is no automatic acquisition, polling, retry, redirect or action replay. Exit 0 means the request succeeded: inspect `state`, including Queued, Draining, Rejected or Unknown. For lost mutation responses, query authoritative state. An explicit keyed retry keeps identical input and the original key. Release retries preserve the original session/epoch; never acquire new control to replay an uncertain operation. Read commands reject mutation flags.

## Screenshot export

`browser status` preserves the service JSON. `browser screenshot` requires a complete, currently authorized observation, a matching request ID, App instance and control epoch, and a result deadline within the next 30 seconds according to the local clock. Keep client/service clocks synchronized. A queued, expired or non-observation result creates no file.

The exporter bounds JSON to 4 MiB, decoded PNG to 2 MiB, and verifies PNG signature/IHDR dimensions, viewport, unit scale and SHA-256. It does not decompress or certify page-controlled pixels or DOM content. The file is published through a private temporary file in the target directory and never overwrites an existing path. JSON output retains observation identity and geometry, replaces inline base64 with `screenshot_file` metadata, and retains the original deadline. Saving a file does not extend authority for later input; every action is reauthorized by the service.

## Validation and remaining work

CLI protocol tests cover exact routes, original payload/key preservation, Queued/Unknown/Draining results, rejection before network access, uncertain unkeyed release, corrupt/expired/mismatched screenshots, private output permissions and overwrite protection. Real CLI + HTTP + PostgreSQL tests cover App idempotency/conflicts, explicit control, queued commands, release/cancel and revocation. Their Running precondition is synthetic; no physical worker runs in those tests. The actual browser execution evidence remains in [58](58-browser-control.md).

The original controller still has a fixed arm window of at most 30 seconds, deny-all networking and a volatile profile. App listing, controlled egress, profile checkpoints and ComputerView/WSS input remain pending. This increment does not certify the complete Browser capability.

Final source-bound validation: [evidence record](../evidence/browser-cli-2026-10-11.json), 20 Bazel targets, 535 Rust and 19 TypeScript tests. Clippy and formatting pass. Runtime evidence boundaries above remain explicit.
