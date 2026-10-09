# 12. Constrained Kubernetes adapter

## 12.1 Delivered scope

The Rust `agent-computer-kubernetes` library can create, inspect and conditionally delete an explicitly admitted **ephemeral** Sandbox Pod through the real Kubernetes HTTPS API. It is not wired to the reconciliation queue: applying a Sandbox definition still does not start a Pod. Computer runtime admission, durable instance allocation, Workspace Candidates, JuiceFS mounts, lease watchdogs and physical fencing remain pending. Public runtime capabilities remain unsupported.

`EphemeralSandboxPlan::new` accepts a statically validated ComputerSet, its Sandbox name, persisted organization/Computer/Sandbox/instance identities, generation, SpecVersion revision, namespace and command vector. These values convey no authority. A future worker must authorize runtime start, persist the allocation and obtain the existing dispatch permit before calling the adapter. No tenant HTTP endpoint exposes these arguments.

Only `gvisor` and a registered deployment mapping to a namespace-wide deny-all NetworkPolicy are supported. Workspace mounts are rejected. The Pod pins the declared image digest and CPU/memory requests and limits, runs UID/GID 1000, drops all capabilities, forbids privilege escalation and host namespaces, uses `RuntimeDefault` seccomp and a read-only root filesystem, and disables automatic service-account credentials. Two bounded 64 MiB memory volumes provide `/tmp` and `/dev/shm`. Restart policy is Never, the deadline is one hour, and deletion allows 30 seconds. This is not a browser image or a persistent Computer environment.

## 12.2 Identity and transport

The deterministic Pod name depends on organization and the persisted instance ID. Changing generation, revision or command for that same instance produces a conflict, not another Pod. A label and a complete binding annotation tie readback to all plan inputs. Adoption also checks the actual Pod UID once known, resourceVersion, and controlled spec fields. Only a finite set of Kubernetes defaults is allowed; injected containers, mounts, environment variables and weaker security settings are rejected. Security switches with enabled defaults must be explicitly disabled, and unexpected annotations that could affect the runtime are rejected.

Before create/observe, the adapter verifies the configured namespace UID and Active/restricted state, `gvisor` RuntimeClass UID and `runsc` handler, and the sole NetworkPolicy's UID and deny-all rules. The deployment operator must restrict namespace/policy mutation and configure an enforcing CNI. These API reads do not prove node execution or network isolation, and do not remove races with a trusted cluster administrator.

The client accepts an explicit HTTPS origin, CA bundle, bearer token and deployment UIDs. It has no kubeconfig discovery or executable credential helpers. It uses only the supplied CA, disables proxies, redirects and automatic retries, bounds connect/total time to 5/15 seconds and responses to 1 MiB, and excludes upstream bodies, URLs and credentials from errors.

Creation makes one POST. A conflict requires readback; a lost response or invalid successful response yields `MutationUnconfirmed`. Recovery must inspect the same persisted identity rather than reallocate or blindly redispatch. Delete includes both UID and resourceVersion preconditions and never uses zero-grace force deletion. API deletion/absence and Pod phase are observations only: they do not release a Workspace writer or prove a partitioned node stopped executing. See [11 Coordination](11-reconciliation-coordination.md) and the [Kubernetes Pod lifecycle](https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/).

## 12.3 Verification

The default suite includes 13 Pod adapter tests covering response loss without repeated POST, conflicting identities, injected spec fields, preflight rejection, conditional deletion, bounded responses and redacted errors. Four additional [Volume cases](13-volume-provisioning.md) bring the adapter suite to 17 tests. These are protocol fixtures, not runtime acceptance.

The explicit component target needs an isolated cluster with runsc and an enforcing CNI. The [test deployment](../../deploy/testing/kubernetes-component.yaml) provisions a dedicated namespace, RuntimeClass, deny policy and narrowly scoped service account. Apply it only to a disposable test cluster. Obtain an expiring token for `ac-adapter`, the cluster CA, and actual object UIDs as the operator; store credentials outside the repository with private permissions.

Create a private JSON configuration:

```json
{
  "endpoint": "https://127.0.0.1:16443/",
  "ca_file": "/private/component/ca.pem",
  "token_file": "/private/component/token",
  "namespace": "ac-kube-component",
  "namespace_uid": "actual-namespace-uid",
  "runtime_class_uid": "actual-runtimeclass-uid",
  "deny_policy_uid": "actual-networkpolicy-uid",
  "image": "docker.io/library/busybox@sha256:REPLACE_WITH_VERIFIED_DIGEST"
}
```

```bash
bazel test //crates/kubernetes:kubernetes_contracts_test
bazel test //crates/kubernetes:kubernetes_live_test \
  --test_env=AGENT_COMPUTER_KUBE_TEST_CONFIG=/private/component/config.json
AGENT_COMPUTER_KUBE_TEST_CONFIG=/private/component/config.json \
  cargo test -p agent-computer-kubernetes --features live-test --test live --locked
```

The live test checks TLS, actual create/readback/conflict, Pod Running observation and conditional deletion. An optional `observation_file` pauses for up to 60 seconds while an external inspector captures logs and node runsc evidence; the inspector then creates the same path with the `.inspected` extension. Check the `AC_SECURITY_PROBE_OK` container log and actual containerd runtime separately. The test fails without explicit configuration; it is excluded from default Cargo tests by a feature and from `bazel test //...` by the manual tag. It is a component test, not T16 combination certification. Chromium, JuiceFS/S3, recovery and the full runtime matrix remain `not_run`.

## 12.4 Recorded component run

On 2026-10-09, source commit `461d52c02e34750c3b6c761d8b6dc817aaa96cf7` passed the explicit Bazel live test in a disposable Ubuntu 24.04.5 KVM VM running K3s v1.37.1+k3s1, containerd 2.3.4-k3s1 and gVisor release-20261005.0. Independent node inspection confirmed `io.containerd.runsc.v1`, `systrap`, OCI root readonly, and the UID/token probe log. The test completed UID-bound creation, conflict/readback and conditional deletion followed by API absence. The [component record](../evidence/kubernetes-component-2026-10-09.json) pins source files and release/image digests and links the actual test log. This does not establish packet-level network isolation, physical fencing or the full runtime combination.

## 12.5 Deployment references

Use the upstream [gVisor installation guide](https://gvisor.dev/docs/user_guide/install/), [containerd setup](https://gvisor.dev/docs/user_guide/containerd/quick_start/) and [shim configuration](https://gvisor.dev/docs/user_guide/containerd/configuration/). Pin and verify release artifacts, including the complete gVisor archive. On containerd 2, use the version-3 runtime table; K3s supports an extension of its base template as described in [K3s advanced options](https://docs.k3s.io/advanced). Set the runsc platform explicitly to `systrap`; never substitute runc or grant privileged access when the component probe fails.
