# 13. Durable JuiceFS volume provisioning

## 13.1 Delivered scope

`agent-computer-worker` connects admitted Volume intents to the Kubernetes HTTPS adapter. The operator command below executes one claim and exits; an external supervisor can repeat it. It selects only Volume intents and preserves dependency ordering. Other kinds remain pending. Publishing a Computer or Sandbox definition does not start a Pod.

This first path accepts revision 1, `Retain`, an explicitly registered and authorized StorageClass ID, and a quota in whole GiB. Expansion, deletion, quota changes and non-integral GiB provisioning remain unsupported. The static declaration schema continues to express byte quotas; backend limitations are reported by the worker, never silently rounded.

The operator pins namespace, RuntimeClass, deny policy, StorageClass and CSIDriver UIDs. Preflight requires the qualified `csi.juicefs.com` driver, `Immediate` binding, `Retain`, `writeback=false` and exact provisioner/node-publish Secret references. The controller cannot read those Secrets. Storage credentials remain in the trusted CSI deployment.

## 13.2 Dispatch and observation

Each claim holds a database lease and rechecks the initiating principal's current authority. Dispatch admission commits before the single PVC POST. PVC names derive from organization and stable Volume identity; the complete admitted spec and deployment binding are retained in an annotation. A conflicting or lost create response leads to readback of that same name. The worker never allocates a second name or automatically recreates a missing object after dispatch.

Migration 5 adds immutable reconciliation object records. The worker records the PVC UID as soon as it observes the claim, including while Pending. A later claim must observe the same UID. Replacing the namespace, StorageClass, driver, PVC or known PV blocks progress. Writes and exact retries preserve the lease, authority, event and Outbox transaction rules. Retry uses a two-second delay; uncertain effects remain observation work after lease expiry.

A Bound claim is cross-checked against the PV's reciprocal claim UID/name/namespace, capacity, retained reclaim policy, driver, filesystem, synchronous mount option, Secret reference and per-PV subdirectory. Unexpected CSI parameters, alternate volume sources, additional PVC labels/resources and annotations outside the finite Kubernetes binding set are rejected. PVC metadata can affect CSI mount configuration, so it is checked alongside the spec. Both object UIDs are journaled before completion. The receipt identifies the PVC and PV; it establishes provisioning only.

**PVC/PV Bound does not establish a usable Workspace or enforced directory quota.** The pinned JuiceFS CSI v0.33.0 controller schedules quota setup asynchronously before returning CreateVolume. Even its `juicefs/controller-quota-set` attribute is not proof of completion. Candidate preparation must independently verify quota, access, flush behavior and generation ownership before enabling a writer. Mounting the entire Volume into a product Sandbox is still unsupported. Physical fencing, multi-node recovery, snapshots, Artifacts and garbage collection remain pending.

## 13.3 Operator configuration

First migrate the database, issue the initiating credential, grant Volume creation and StorageClass reference permission, register the catalog reference, then plan/apply a declaration through [10 Plans and apply](10-plans-and-apply.md). Configure the worker with that catalog resource ID and actual cluster UIDs. This command is a trusted operator surface, not a tenant API.

```bash
agent-computer-server reconciliation-volumes-once \
  --database-url-file /private/control/database-url \
  --organization org_example --worker-id worker_1 \
  --config-file /private/control/volume-worker.json
```

Example JSON, replacing every deployment placeholder with the actual value:

```json
{
  "api_url": "https://127.0.0.1:16443/",
  "ca_file": "/private/control/ca.pem",
  "token_file": "/private/control/kubernetes-token",
  "deployment": {
    "namespace": "ac-kube-component",
    "namespace_uid": "actual-namespace-uid",
    "runtime_class_uid": "actual-runtimeclass-uid",
    "deny_policy_uid": "actual-networkpolicy-uid",
    "network_policy_ref": "deny-all"
  },
  "storage": {
    "reference": "id:actual-catalog-resource-id",
    "name": "ac-juicefs",
    "uid": "actual-storageclass-uid",
    "driver_uid": "actual-csidriver-uid",
    "secret_name": "ac-juicefs-secret",
    "secret_namespace": "kube-system"
  }
}
```

Configuration, database URL, CA and token files must be private regular files, at most 8 KiB. The Kubernetes client uses only the supplied CA, HTTPS origin and expiring token, with no implicit kubeconfig, redirects, proxy or HTTP retries. Renew operator-managed credentials without changing the pinned deployment identities. The JSON result reports idle or the persisted intent progress; process success alone does not mean the intent succeeded. Read Blocked/retry reasons and the operation endpoint.

## 13.4 Verification and deployment references

The database suite covers immutable/idempotent UID records, rollback of the record/event/Outbox, revocation, stale leases and kind-filtered claims with dependency ordering. Five volume protocol cases additionally cover changed deployment identity, destructive reclaim, asynchronous upload options, alternate data sources, reciprocal PVC/PV binding, extra backend parameters, capacity overflow and lost create acknowledgement without a second POST.

For an explicit live test, provision a disposable cluster using [the Kubernetes test namespace](../../deploy/testing/kubernetes-component.yaml), an operator-installed pinned JuiceFS CSI deployment, a separately formatted filesystem with PostgreSQL metadata and S3 data, then [the volume RBAC/StorageClass fixture](../../deploy/testing/juicefs-component-rbac.yaml). The fixture grants PVC get/create and read-only PV/StorageClass/CSIDriver access, with no volume deletion or Secret access. Trusted CSI node components require privileges inside the test cluster; application Pods must still use the restricted gVisor profile.

Use the private worker configuration above as `AGENT_COMPUTER_VOLUME_TEST_CONFIG`. The live test substitutes its own temporary catalog ID and can additionally accept `evidence_file` for non-secret observed UIDs. The operator command deliberately rejects that test-only field. Install the PostgreSQL test prerequisites from [08 Persistence](08-persistence.md).

```bash
bazel test //crates/worker:volume_worker_live_test \
  --test_env=AGENT_COMPUTER_VOLUME_TEST_CONFIG=/private/component/volume-config.json
AGENT_COMPUTER_VOLUME_TEST_CONFIG=/private/component/volume-config.json \
  cargo test -p agent-computer-worker --features live-test --test live --locked
```

The live target fails without explicit configuration and is excluded from default tests. It exercises a real authorized plan/apply, worker dispatch and CSI provisioning, then verifies durable object records and an idle subsequent worker. It intentionally retains the PVC/PV until disposal of the isolated environment. File persistence and actual quota checks require additional probes; this test alone does not certify T16 or T19.

Consult the upstream [dynamic provisioning guide](https://juicefs.com/docs/csi/guide/pv/), [CSI installation](https://juicefs.com/docs/csi/getting_started/), [PostgreSQL metadata guidance](https://juicefs.com/docs/community/databases_for_metadata/), and the pinned [v0.33.0 controller implementation](https://github.com/juicedata/juicefs-csi-driver/blob/v0.33.0/pkg/driver/controller.go). Production still requires the deployment and recovery certification described in CodeSpec.

## 13.5 Recorded component run

On 2026-10-09, commit `3cf8370` passed the explicit Bazel worker target with real PostgreSQL and JuiceFS CSI. A fresh, uncached run also passed after the PVC metadata guard in `30204c4`. The disposable KVM environment used K3s v1.37.1+k3s1, containerd 2.3.4-k3s1, gVisor release-20261005.0 with systrap, JuiceFS CSI v0.33.0, JuiceFS 1.4.1, PostgreSQL 16.15 for filesystem metadata and SeaweedFS 4.48 for S3 data. Control database tests used PostgreSQL 18.6.

A separate restricted UID 1000 gVisor Pod wrote a 1 MiB random file and completed `dd conv=fsync`. An over-quota write returned `Disk quota exceeded`. After normal writer Pod deletion, a new read-only gVisor Pod verified the same SHA-256. A fresh read-only JuiceFS client with an empty memory cache also read that digest; its metrics recorded one 1 MiB object GET. These are separate operator probes, not behaviors supplied by the Volume worker.

The [source-bound record](../evidence/juicefs-component-2026-10-09.json) contains deployment/image digests, actual object UIDs, probe manifests/commands, failed setup attempts and precise limits; the [log](../evidence/juicefs-component-2026-10-09.log) contains the real test and file/quota output. The record keeps the original source hashes and identifies the later guard verification separately. This single-node, non-HA environment does not certify power-loss durability, production TLS, recovery, physical fencing, Candidate readiness or T16/T19.
