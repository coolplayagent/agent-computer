# 26. Bounded Kubernetes startup attach

## 26.1 Delivered behavior

The Kubernetes crate now provides `StartupSandboxPlan`, `Client::attach_startup` and a single-use `StartupChannel::run`. They carry the [25 startup protocol](25-execution-startup.md) over a real Kubernetes WebSocket attach connection. This increment uses an ephemeral workspace and an explicitly approved supervisor image. It does not connect the database dispatch worker to a Candidate-mounted Pod, and the public execution capability remains unsupported.

The plan fixes the supervisor entrypoint, immutable Bootstrap, full instance identity, image digest and security settings. The caller's operator-approved image must exactly match the validated sandbox image and contain both the trusted supervisor and its tools. A tenant image containing a file at the expected path is not sufficient. The bootstrap is visible in Pod arguments/annotations; it must not contain credentials. `stdin=true`, `stdinOnce=true`, `tty=false`, `restartPolicy=Never`, private process namespaces, UID/GID 1000, no capabilities, no privilege escalation and a read-only root are required. The workspace is a 64 MiB memory `emptyDir`; Candidate/CSI mounts remain separate work.

## 26.2 Startup sequence and identity

1. Re-read deployment prerequisites and the exact recorded Pod UID/spec; require Running.
2. Upgrade the existing explicitly trusted HTTPS client to HTTP/1.1 WebSocket, requiring `v5.channel.k8s.io`, the exact acceptance hash and no extensions. Redirects, retries, TTY, exec and older-protocol fallback are disabled.
3. Send a bounded, non-authorizing hello binding the Bootstrap digest on stdin. PID 1 waits for this hello before emitting its fresh challenge, avoiding a challenge written before attach connects stdout. Both hello and grant waits have a 30-second limit, and each input line is bounded by 64 KiB.
4. Read and validate the challenge, then re-read the same Pod UID/spec. Only then may the future trusted worker obtain a fresh database startup grant. Merely opening a channel supplies no database authority.
5. `run` validates the grant, rechecks the Pod once more, sends the grant once, then closes only stdin with the v5 `[255, 0]` message. A consumed channel cannot be cloned or deserialized. There is no reconnect or resend.

Kubernetes attach options have no Pod UID precondition. These observations therefore require an operator-exclusive namespace and attach RBAC; they do not create an atomic UID precondition or protect against a hostile cluster administrator. See the upstream [PodAttachOptions definition](https://raw.githubusercontent.com/kubernetes/api/v0.37.1/core/v1/types.go). The v5 stream-close format follows [Kubernetes wsstream](https://pkg.go.dev/k8s.io/streaming/pkg/httpstream/wsstream), with channel numbers from [remotecommand constants](https://github.com/kubernetes/apimachinery/blob/master/pkg/util/remotecommand/constants.go).

The supervisor still charges its grant budget from before challenge emission. The client conservatively charges from before attach starts, so rechecks and database/transport delay reduce usable time. A recovered database grant is not permission to reattach or replay. Cancellation/revocation reconciliation and the independent external watchdog still belong to the pending worker.

## 26.3 Bounds and uncertainty

| Input or operation | Bound |
| --- | --- |
| Attach, challenge and post-challenge identity check | 15 seconds total |
| WebSocket message/frame | 64 KiB; at most 4096 incoming messages, including control messages |
| Challenge | 4 KiB |
| Supervisor diagnostics | 64 KiB |
| Remote-command status | 4 KiB |
| Serialized raw report | `8 × per-stream output limit + 16384` bytes |
| Report collection | Original client budget deadline + TERM grace + 2 seconds |

Binary stream messages may split JSON across frames. Text messages, unsupported channels, mismatched bindings, additional non-whitespace after a complete line and over-limit data fail closed. JSON report parsing extracts correlation fields without expanding output byte arrays into a generic JSON tree.

Once a grant write is attempted, write, disconnect, timeout, malformed status and collection uncertainty return `MutationUnconfirmed`. The adapter never retries. Before-write failures do not mean the durable grant can be reissued. Dropping the future or socket is not process termination, storage drainage or lease release; a separate watchdog and recovery owner are necessary.

`StartupObservation` contains the Pod UID, bounded raw report and diagnostics. It checks challenge/grant/execution/generation/request digests only. Kubernetes remote-command `Success` indicates transport completion; application outcome and accepted durable completion remain separate. Even a valid local success report does not fence a paused supervisor or drain a Candidate writer.

## 26.4 Validation and reproduction

Eight new protocol tests cover the fixed plan, fragmented challenge, hello/grant ordering, ping/pong, one stdin close, UID replacement at three checks, invalid handshake/challenge/grant, redirects, response loss, failure status and output bounds. The default workspace contains 271 tests, including 26 Kubernetes cases. Cargo tests, fmt/Clippy and Bazel tests pass. The full Qualitygate policy still checks line endings only.

The manual `//crates/kubernetes:startup_live_test` ran four cases on a disposable K3s/gVisor VM: normal stdout/stderr, command timeout, lease expiry and 100,000-byte output with a 64-byte retained prefix. Node inspection confirmed `io.containerd.runsc.v1`, Systrap and the read-only OCI root. The grants are fixtures; these cases do not combine database authorization, CSI storage, external fencing or product completion.

Build a test image from trusted local binaries on Linux x86_64:

```sh
bazel build //crates/sandbox:agent-computer-sandbox //crates/kubernetes:startup_live_test --lockfile_mode=error
python3 crates/sandbox/tests/rootfs.py --supervisor bazel-bin/crates/sandbox/agent-computer-sandbox --destination /tmp/ac-attach-rootfs
python3 crates/kubernetes/tests/startup_image.py --rootfs /tmp/ac-attach-rootfs --output /tmp/ac-attach-image.tar
```

Import that archive into the disposable cluster's containerd and use the emitted digest reference. Apply [base test RBAC](../../deploy/testing/kubernetes-component.yaml) and [attach test RBAC](../../deploy/testing/startup-attach-rbac.yaml). The latter permits only `pods/attach`, not `pods/exec`. Supply a private JSON file containing `endpoint`, `ca_file`, `token_file`, `namespace`, `namespace_uid`, `runtime_class_uid`, `deny_policy_uid`, `image` and `result_file`. The optional `observation_file` pauses the first case for node inspection until a sibling `.inspected` marker appears, for at most 15 seconds.

```sh
bazel test //crates/kubernetes:startup_live_test --lockfile_mode=error --test_env=AGENT_COMPUTER_KUBE_STARTUP_CONFIG=/private/startup-config.json
```

The fixtures are not distributable supervisor packaging. Production image provenance, Candidate mounts, the database-backed dispatch worker, independent watchdog/fencing, output objects and accepted completion are still required. Runtime acceptance T01–T43 remains `not_run`.

The [source-bound record](../evidence/kubernetes-startup-attach-2026-10-10.json) and [raw output](../evidence/kubernetes-startup-attach-2026-10-10.log) pin commit `d25fdd5`, 200 verified inputs, the exact test image and supervisor binary. The same binary also passes all 12 startup and 13 supervisor regression cases. The VM, private disk, SSH keys and Kubernetes credentials were removed.

The subsequent [27 Candidate mount increment](27-candidate-pod-mounts.md) adds a prepared data-leaf mount; database worker integration and fencing remain pending.
