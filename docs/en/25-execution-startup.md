# 25. One-shot execution startup authorization

## 25.1 Delivered behavior

The [24 dispatch journal](24-execution-dispatch.md) now has a separate startup boundary. Migration 14 records one grant for one execution, Pod UID and supervisor challenge. The isolated supervisor supports `--startup BOOTSTRAP_PATH`: it emits a fresh challenge before starting any application process, waits for one bounded JSON response, and then runs the fixed command with the remaining budget. The original `--request` entry remains a component interface, not an authenticated product API.

The trusted controller method `authorize_candidate_execution_startup` rechecks the connection's original principal/credential, current scopes and grants, Candidate/catalog binding and fixed deadline after receiving the challenge. This prevents a delayed Pod or stale dispatch attempt from creating a fresh execution window. The service still advertises execution as unsupported: Kubernetes attach, trusted supervisor delivery and actual Candidate mounts are not connected yet.

## 25.2 Protocol and time accounting

The immutable Bootstrap contains protocol version 1, the committed intent digest and the fixed supervisor Request. Its `lease_budget_ms` is only the original ceiling; it cannot start a process. PID 1 verifies its namespace/non-root/no-capability conditions and generates a 256-bit random nonce. Its stdout challenge binds that nonce, execution/generation and the bootstrap digest. It accepts one newline-terminated grant of at most 64 KiB; unknown/duplicate fields, bad versions, mismatched challenge digests, zero/excess budgets, oversize input and additional complete frames are rejected. Child stdin is null, and startup bytes/control credentials are not inherited.

The timer starts **before** PID 1 emits its challenge. Only after receiving that challenge does the trusted controller read fresh database time and compute the remaining duration. PID 1 charges that duration from its earlier timer anchor. Thus waiting for attach, authorization or response delivery consumes the budget; receiving a grant never resets the timer. No comparison between Pod wall time and database wall time is needed. A response that arrives after its budget is rejected before command launch. Waiting without a grant is limited to 30 seconds. TERM, INT or HUP while waiting also prevents launch.

Once accepted, the command timeout starts at execution setup, while the lease deadline stays anchored before the challenge. The earlier deadline wins. `StartupReport` binds the challenge/grant digests and contains the ordinary local supervisor report; its elapsed time includes startup waiting. Reports are streamed directly to stdout without first expanding bounded byte arrays into a JSON value tree. Application stdout/stderr remain bounded and independently drained.

The challenge is public correlation data, not a credential or signed authorization token. This protocol requires an authenticated, exclusive runtime attach channel to the exact verified Pod UID and an operator-controlled supervisor/bootstrap. An untrusted image or arbitrary stdin writer cannot be made trustworthy merely by this handshake. The actual controller must enforce these deployment conditions.

## 25.3 Database lifecycle and failure handling

The startup API is internal to trusted store clients and cannot accept a tenant-selected principal, credential, command or budget. It derives the Bootstrap from the immutable dispatch inputs and compares the challenge. A single transaction records the Pod UID, challenge, fixed grant, digest and database timestamp together with an event/Outbox entry. Credential/principal share locks serialize revocation; permission and expiry checks are repeated after writes. The execution remains Dispatching: `execution.startup_authorized` explicitly carries `process_started_confirmed: false`.

Only the winner receives `ExecutionStartupAttempt`; it cannot be cloned or deserialized. A second authorization call returns `DispatchAlreadyStarted`, including a changed Pod UID/nonce or a lost acknowledgement. Read-only recovery returns the original record and never renews it. A replacement runtime needs physical reconciliation, not another startup grant. The controller must not treat recovery data as permission to repeat an external mutation.

A cancellation that wins before startup blocks the grant. Credential loss or the original fixed deadline expiring moves dispatched work to Unknown and leaves its writer Draining. Writer renewal cannot extend the startup deadline. Failed Outbox writes or credentials expiring during the transaction roll back the grant. Existing dispatches gain no invented grant during upgrade. Startup records are immutable and do not provide process-success or drain evidence.

## 25.4 Verification and remaining work

Three new supervisor contract tests verify digest/nonce/budget binding, strict inputs and host refusal. Six new real PostgreSQL cases cover exclusive grants, WAL recovery, changed runtime identity, bootstrap/generation mismatch, revision conflict, revocation/cancellation after dispatch, delayed challenges after renewal, Outbox/late-expiry rollback and migration 14. The default workspace has 263 tests: 121 PostgreSQL, 20 server, 8 supervisor and 114 other cases. Cargo tests, fmt/Clippy, Bazel build/test and bilingual documentation checks pass; full Qualitygate retains the existing line-ending policy only.

Twelve real gVisor/Systrap startup cases additionally cover a valid command, delayed fragmented grant, expired grant, wrong challenge, excessive budget, EOF, cancellation before grant, duplicate/oversize frames, anchored lease expiry, null child stdin/environment isolation and a withheld grant. The 13 existing supervisor runtime scenarios also pass with the same binary. These explicit component runs use fixture grants and ordinary isolated workspaces; they do not combine database admission with a Kubernetes/CSI runtime and do not satisfy T01–T43.

Reproduce only in a disposable Linux VM with a verified runsc installation:

```sh
bazel build //crates/sandbox:agent-computer-sandbox --lockfile_mode=error
python3 crates/sandbox/tests/rootfs.py --supervisor bazel-bin/crates/sandbox/agent-computer-sandbox --destination /tmp/ac-startup-rootfs
sudo python3 crates/sandbox/tests/startup_component.py --runsc /usr/local/bin/runsc --rootfs /tmp/ac-startup-rootfs --work-dir /var/tmp/ac-startup-component
sudo python3 crates/sandbox/tests/component.py --runsc /usr/local/bin/runsc --rootfs /tmp/ac-startup-rootfs --work-dir /var/tmp/ac-supervisor-regression
```

Each work directory must be new. The rootfs script runs `ldd` only on explicitly trusted local binaries; it produces a verification fixture, not a product image. Component JSON records exact OCI inputs, runtime/binary/library digests and local observations.

Still required: the strict Kubernetes Candidate Pod/attach adapter and worker, trusted supervisor packaging, CSI mount identity validation, an external watchdog and physical fencing, bounded output objects and accepted completion. The suspended-init fault remains a reason that local reports, startup grants and Pod API status cannot release the writer lease. Background execution remains a separate pending lifetime.
