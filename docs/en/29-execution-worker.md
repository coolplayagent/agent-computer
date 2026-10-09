# 29. Database-backed Candidate execution worker

## 29.1 Delivered path

The trusted `execution::execute_once` worker connects [dispatch admission](24-execution-dispatch.md), the [Pod identity journal](28-execution-pod-journal.md), [Candidate mounts](27-candidate-pod-mounts.md) and [startup grants](25-execution-startup.md). It consumes one existing queued execution. A duplicate call fails at the dispatch journal without interrupting an active controller. There is no general polling scheduler or background execution lifetime.

`candidate_execution_runtime_inputs` reconstructs the original preparation request, manifest, receipt, Sandbox snapshot and successful Volume effect. It checks the frozen hashes, input revision, generation/Candidate identity and both recorded PVC/PV UIDs. It does not resolve current resource heads or adopt the original starter's credential. This trusted read remains available for recovery after credential/catalog revocation and grants no authority by itself.

The worker compiles the same normalized Volume plan used for provisioning and a fixed Candidate startup Pod. Operator-approved image, StorageClass mapping, actual namespace UID, recorded PVC/PV names and owner UID/GID 1000 must match. It opens the real local JuiceFS mount and rechecks the prepared receipt, inode, filesystem UUID and directory quota before registration. It performs the check again after receiving the supervisor challenge and before requesting fresh database authorization.

The Pod plan commits before the single POST. The returned UID is recorded before waiting for Running. The controller polls current execution authority while waiting and collecting the bounded report. Attach is one-shot; it never reconnects, retries a grant or falls back to exec. The original dispatch budget bounds the flow, including compilation, storage checks, scheduling, attach and authorization. The supervisor independently charges the grant budget from before its challenge.

## 29.2 Outcome and recovery

Every attempted flow ends by lowering execution authority to Unknown and retaining a Draining writer. A correlated local report is returned only as private raw bytes for a future collector; it is not accepted completion and is omitted from operator JSON. `interrupted_at` identifies the stage where no report was obtained. This includes input/storage rejection before Pod creation.

Cleanup observes the original plan and known UID and performs UID/resourceVersion conditional deletion. If the creation response was lost, observation may adopt only the exact persisted plan. A missing object is `api_absent`; deletion requested is `delete_requested`; an uncertain observation/deletion remains `unconfirmed`. None releases the writer. Database lowering is bounded to five seconds, cleanup to fifteen seconds and an optional UID record during cleanup to one second, so a database outage cannot indefinitely prevent an attempted conditional delete. A timeout still proves no process stop. In-flight quota verification runs in the blocking pool with the configured bounded quota subprocess; cancellation does not turn it into a drain certificate.

`recover_once` is an explicit stop/reconciliation operation: it may interrupt an existing controller by lowering authority, recompiles and compares the exact original manifest, then observes/deletes. It never creates a Pod, attaches, reads a stored grant for replay or accesses the local data mount. Changed operator configuration or mismatched identities prevent cleanup rather than selecting a replacement instance.

## 29.3 Operator commands

These commands require private database/Kubernetes configuration and trusted operator access:

```sh
agent-computer-server execution-dispatch-once --database-url-file /private/control-url --organization ORG --execution-id EXECUTION --expected-revision 1 --config-file /private/execution.json
agent-computer-server execution-recover-once --database-url-file /private/control-url --organization ORG --execution-id EXECUTION --config-file /private/execution.json
```

The private JSON contains `api_url`, `ca_file`, `token_file`, `deployment` and `execution`. The transport/deployment fields follow [12 Kubernetes](12-kubernetes-adapter.md). `execution` contains `approved_supervisor_image`, the qualified `storage` mapping from [13 Volume provisioning](13-volume-provisioning.md), and the `candidate` configuration from [17 preparation](17-candidate-preparation-worker.md). The latter's object cache is unused by execution; no inputs are materialized again. Secrets are file references and never enter the Pod or command output.

## 29.4 Verification and limits

Three PostgreSQL contracts add fixed-input reconstruction, WAL recovery, read-only recovery after revocation and rejection of missing/mismatched Volume evidence. Default Cargo/Bazel suites contain 288 tests, including 131 PostgreSQL cases. Clippy, formatting, paired documentation and full existing Qualitygate checks are separate gates.

The manual `//crates/worker:execution_worker_live_test` uses real control PostgreSQL, Kubernetes/CSI, JuiceFS and gVisor. Its five cases exercise library execution, the operator command, cancellation after a child writes a marker, lost creation acknowledgement with observation-only cleanup, and a rejected supervisor image. It asserts durable startup grants, retained writer exclusion, no replacement dispatch and no fabricated drain. Run only in a disposable root-owned environment with `AGENT_COMPUTER_EXECUTION_TEST_CONFIG`; configuration extends [17's fixture](17-candidate-preparation-worker.md) with `image`, `server_binary` and `result_file`. A test target's presence alone is not a live pass; source-bound evidence must record the actual run.

The public runtime still advertises execution as unsupported. Production supervisor packaging, a controller-independent watchdog, physical fencing, durable bounded output objects, accepted completion, automatic crash reconciliation and background lifetimes remain pending. Controller or PID 1 suspension is not solved by local polling. T01–T43 remains `not_run`.

The live fixture reserves a 50 GiB Volume for five fixed 10 GiB Candidates. Its first 4 GiB configuration was correctly rejected by runtime capacity admission before creating an execution Pod. Optional `node_observation_file` enables an eight-second cancellation barrier for the independent [node inspector](../../crates/worker/tests/execution_node.py): run it concurrently with `--observation PATH --output PATH` in the disposable VM. It verifies the actual runsc container and kubelet Candidate bind inode before releasing the barrier; the execution budget continues to run.

The explicit run passed all five cases on `bacaef9`. Actual database readback retained five dispatches, four Pod plans/UIDs and three startup grants, all correlated to their original identities; all five executions remained Unknown and writers Draining, with zero accepted completions or drains. Node inspection matched the cancellation Pod's runsc container and Candidate inode 25. A fresh read-only JuiceFS client read both persisted files through two S3 GETs (18 bytes). The [source-bound record](../evidence/candidate-execution-worker-2026-10-10.json) and [raw output](../evidence/candidate-execution-worker-2026-10-10.log) pin 223 source/build inputs, exact binaries/image and the earlier capacity rejection. The owned VM and private credentials/storage were removed after collection.
