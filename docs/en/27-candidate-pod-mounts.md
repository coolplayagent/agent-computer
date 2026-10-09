# 27. Candidate data mounts for execution Pods

## 27.1 Delivered behavior

`CandidateMount` and `StartupSandboxPlan::with_candidate` extend [26 startup attach](26-kubernetes-startup-attach.md) to one prepared JuiceFS Candidate. The plan mounts only the fixed `organization/{org}/workspace/{workspace}/candidates/{candidate}/generation/{generation}/data` leaf at `/workspace`. The volume root, other Candidates and preparation receipt remain outside the application mount. Ephemeral startup remains available through its existing constructor.

The constructor takes a qualified Volume plan, namespace/PV UIDs, CSI volume path, the original `PrepareRequest` and its `Prepared` receipt. It validates the preparation digest with fixed writer UID/GID 1000, original manifest digest, quota, PVC UID, data path, generation and nonzero inode. The Volume organization must match, and its quota must cover the Candidate. The Sandbox declaration must contain exactly one writable mount of that Workspace's `id:` reference at `/workspace`; no path override, additional workspace or implicit mount is allowed. The Pod's fixed identity must match the preparation's organization, Computer and generation.

This is a trusted controller interface. A preparation receipt is storage evidence, not a writer grant. The caller must verify the actual trusted filesystem mount/receipt and obtain current dispatch/startup authorization before runtime use. No tenant HTTP input gains a constructor or arbitrary PodSpec capability.

## 27.2 Readback and permissions

Before creating the Pod, before attach, after receiving the startup challenge and before writing the grant, the client re-reads deployment, StorageClass, CSI driver, PVC and PV. It checks their fixed UIDs and both sides of the claim binding, requires synchronous `writeback=false`, retained storage and the qualified CSI secret reference, and requires the PV name, CSI handle and provisioned subdirectory to match the fixed volume path. A stale/replaced object prevents the next mutation. The plan embeds the compact storage binding and receipt in its immutable annotation; it does not embed the full input manifest or storage secrets.

The Pod omits `fsGroup`: its prepared data already belongs to UID/GID 1000, while the volume root, generation parent and receipt remain operator-owned. Kubernetes/CSI can apply ownership changes from `fsGroup`, potentially affecting the whole mounted volume. See [Kubernetes security contexts](https://kubernetes.io/docs/tasks/configure-pod-container/security-context/). Strict admission readback rejects injected `fsGroup`, supplemental groups, altered claim/subPath, mount expressions or extra volumes. Omitted `readOnly=false` is accepted only on the two fields whose Kubernetes default is false.

gVisor bind mounts must retain shared semantics; exclusive caching is unsuitable for this externally observed workspace. The adapter accepts no unplanned runtime annotations. Actual node configuration remains an operator qualification requirement; see [gVisor filesystems](https://gvisor.dev/docs/user_guide/filesystem/).

Kubernetes mounts name a PVC rather than atomically conditioning the mount on its UID. These checks depend on exclusive operator RBAC, trusted CSI/node configuration, and private immutable Candidate parent paths. API readback alone does not prove the actual mounted inode, filesystem UUID, quota enforcement or physical fencing. UID/resourceVersion conditional Pod deletion remains usable if storage becomes unavailable; deletion is still not drainage or a completion receipt.

## 27.3 Validation and reproduction

Seven new protocol cases cover mismatched preparation bindings, mount scope, admission mutations, changed storage before creation, replacement at all three attach checks, a successful one-shot channel and deletion independent of storage availability. The default workspace contains 278 tests, including 33 Kubernetes tests. Cargo/Bazel tests, fmt/Clippy and documentation checks are required separately from the existing line-ending Qualitygate policy.

The manual `//crates/kubernetes:candidate_mount_live_test` provisions a retained Volume through real CSI, prepares two independent Candidates, runs the trusted supervisor through v5 attach, checks their original file content, writes and fsyncs separate files, and revalidates preparation receipts and directory ownership/modes. It runs as the trusted root controller in a disposable VM; application processes remain UID/GID 1000 in gVisor. Grants are protocol fixtures, so this does not establish database-backed execution, durable result acceptance or physical fencing.

```sh
bazel build //crates/kubernetes:candidate_mount_live_test //crates/sandbox:agent-computer-sandbox --lockfile_mode=error
python3 crates/sandbox/tests/rootfs.py --supervisor bazel-bin/crates/sandbox/agent-computer-sandbox --destination /tmp/ac-candidate-rootfs
python3 crates/kubernetes/tests/startup_image.py --rootfs /tmp/ac-candidate-rootfs --output /tmp/ac-candidate-image.tar
```

Import the emitted digest image into the disposable cluster. Use the qualified storage environment and private `kubernetes`/`candidate` configuration from [17 Candidate preparation](17-candidate-preparation-worker.md), including a full trusted JuiceFS mount, verified filesystem UUID, object cache and quota executable. Set deployment `network_policy_ref` to `deny-all`, add top-level `image` and `result_file`, and apply [attach test RBAC](../../deploy/testing/startup-attach-rbac.yaml). The optional `observation_file` waits at most 15 seconds for its sibling `.inspected` marker before attach, allowing external node inspection.

```sh
sudo env AGENT_COMPUTER_CANDIDATE_MOUNT_CONFIG=/private/candidate-mount-config.json /path/to/candidate_mount_live_test --nocapture
```

The test deletes its execution Pods conditionally and leaves the retained Volume for independent inspection. Private VM teardown follows evidence collection. Production supervisor packaging, database dispatch worker integration, an independent watchdog, physical fencing, bounded output objects and accepted completion remain pending. Runtime acceptance T01–T43 stays `not_run`.

The explicit run passed both Candidate cases. Node inspection matched the actual kubelet subPath bind inode and UID/GID 1000 against the preparation receipt. A new read-only JuiceFS client with no disk cache read both fsynced files and recorded two S3 GETs. Volume root, generation parent, receipt and data ownership/modes were unchanged. The first attempt expired while the CSI helper image downloaded; the second stopped at an incorrect node-inspector path assumption. The corrected inspector and unchanged test binary passed on the third attempt; these earlier failures are retained with the evidence.

The [source-bound record](../evidence/candidate-pod-mounts-2026-10-10.json) and [raw test output](../evidence/candidate-pod-mounts-2026-10-10.log) pin `51600f9`, 203 verified source/build inputs, the tested binaries/image, node inode inspection and independent S3 reads. Both failed attempts remain explicit. The owned VM, private storage and credentials were removed after collection.
