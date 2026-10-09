# 09. Control service and service credentials

## 09.1 Start the development service

`agent-computer-server` is the Rust/Tokio/Axum HTTP entry point, built with Bazel. It currently serves health, schema readiness, version/capabilities, OpenAPI, and authenticated static ComputerSet validation. It does not start a Computer or expose the internal declaration registry as an apply endpoint.

Provision a PostgreSQL database and put its connection URL in a private regular file, mode `0600` or `0400`, under a trusted directory. Symlinks and group/other access are rejected; URL contents are limited to 8 KiB. The URL is supplied through a file, never a command-line value. Run migrations explicitly before serving:

```bash
bazel run //:agent-computer-server -- migrate \
  --database-url-file /run/secrets/agent-computer/database-url
bazel run //:agent-computer-server -- serve \
  --database-url-file /run/secrets/agent-computer/database-url \
  --listen 127.0.0.1:8080
```

The default listener is `127.0.0.1:8080`. The process speaks HTTP; network clients require an HTTPS reverse proxy and a private backend connection. TLS termination, header/connection limits, production database roles and deployment certification remain deployment work. The server handles SIGINT/SIGTERM with graceful shutdown. It uses a 16-connection pool with a 5-second acquisition timeout. It does not run migrations during requests or silently upgrade at startup.

## 09.2 Credential lifecycle

Credential administration is a local operator command using trusted database access. A service token cannot mint credentials, choose a request's principal, or disable another principal through HTTP. Issuance creates or reuses an enabled principal with a fixed organization and `human`/`agent` kind. A kind mismatch or disabled principal is rejected.

```bash
bazel run //:agent-computer-server -- credential-issue \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal validator --kind agent \
  --scopes definitions.validate --ttl-seconds 3600 \
  --output /run/secrets/agent-computer/validator-token

bazel run //:agent-computer-server -- credential-revoke \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --credential CREDENTIAL_ID

bazel run //:agent-computer-server -- principal-disable \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal validator
```

The new credential file is created exclusively with mode `0600`; an existing path is never overwritten. Stdout reports the credential ID and metadata, not the bearer secret. Each issuance creates a distinct credential. Rotation means issuing a new one, switching the caller, then revoking the old ID. An interrupted issuance may leave an unusable credential or incomplete file; inspect the database and revoke unused IDs before retrying. No remote credential administration or automated rotation is implemented.

Tokens use `acsk_<128-bit random ID>_<256-bit random secret>`. Random bytes come from the operating system; the database stores a domain-separated SHA-256 digest, not the token. The secret type redacts Debug output and does not implement Serialize. Digest comparison uses constant-time comparison. See [getrandom](https://docs.rs/getrandom/0.4.3/getrandom/fn.fill.html) and [subtle](https://docs.rs/subtle/2.6.1/subtle/trait.ConstantTimeEq.html).

Lifetimes are whole seconds from 1 to 86400. Expiry uses database time. Every protected request checks token validity, expiry, revocation, principal status and exact scope; validation rechecks before constructing its response. `definitions.validate` permits only static validation. `definitions.manage` is reserved for the future planner and does not imply validate, resource manage, or any runtime permission. Disabling a principal invalidates all its credentials for subsequent checks; a cached identity value is not an authorization permit for a later transaction. Requests already past their final check may finish; ongoing SSE/WSS revocation is not implemented because those transports are not available yet.

## 09.3 HTTP contract

| Method/path | Authentication | Behavior |
| --- | --- | --- |
| `GET /health` | Public | Process is alive; no database dependency |
| `GET /ready` | Public | Database reachable and exact embedded migration history/checksums match |
| `GET /v1alpha1/version` | Public | Build and API versions |
| `GET /v1alpha1/capabilities` | Public | Actual available and unsupported capabilities |
| `GET /v1alpha1/openapi.json` | Public | [OpenAPI 3.1 contract](../../schemas/openapi-v1alpha1.json) |
| `POST /v1alpha1/definitions/validate` | Bearer + `definitions.validate` | Static ComputerSet JSON validation; no writes or resource apply |

Send the token in exactly one `Authorization: Bearer …` header. Cookies, query strings, caller headers and body principal fields never establish identity. The current endpoint rejects browser Origin headers; human OIDC/CSRF/embedded login is pending. Use an uncompressed `application/json` body, at most 1 MiB; the existing duplicate-key, depth/node and semantic bounds also apply. YAML remains a local CLI input format. A valid report returns 200; invalid declarations return 422 with `details.validation`. Static validation is a query and does not require an Idempotency-Key.

Errors use `code/message/retryable/request_id/details`, with 401 for invalid credentials and 403 for missing scope. Database failures return a generic 503. Unknown endpoints return 404, wrong methods 405, unreadable bodies 400, oversize bodies 413, unsupported media 415, and request timeouts 408. Server-generated request IDs ignore client-supplied IDs. Responses use `Cache-Control: no-store`; submitted values, database URLs and tokens are not reflected in error messages.

The request handler permits 64 concurrent requests, has a 10-second timeout, and allows at most eight simultaneous blocking validations. Validation permits remain held until parsing finishes even if its request is cancelled. Overload returns 503. These are initial bounds, not throughput or production availability claims.

## 09.4 Verification and remaining scope

The shared `crates/test-support` starts real private PostgreSQL clusters for store and server tests. Three credential cases cover random issuance, stored hashes, principal binding, scope separation, tampering, expiry, revocation, disable, lifetime bounds and readiness checks. Six service cases cover authorization before parsing, identity spoofing, no-side-effect validation, duplicate headers, protocol limits, readiness failures, private operator files, and an actual server process over TCP with issue/validate/revoke/SIGTERM. Run the [database-enabled Bazel/Cargo suite](08-persistence.md).

OIDC, organization membership and Workspace/Computer/App grants, ConnectionSession/ViewerSession, protected streaming, transactional resource authorization, plan/apply, deployment and Computer runtime are still pending. This increment does not satisfy full T10/T18/T22 acceptance or establish production security certification.
