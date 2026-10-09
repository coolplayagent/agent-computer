# 10. Definition plans and atomic apply

## 10.1 Delivered boundary

The control API can now plan and publish all six ComputerSet resource kinds: Volume, Workspace, Sandbox, App, Agent and Computer. Plans contain resolved before/after specs, stable resource IDs, pinned dependency revisions/digests and drain requirements. Apply commits the declaration version, changed resource SpecVersions, creator grants, an operation, reconciliation intents, an idempotency receipt and event/Outbox together in PostgreSQL.

The server advertises `definitions.plan` and `definitions.apply` as `control-plane`. Apply creates `Queued` operations and `Pending` intents. [Durable coordination APIs](11-reconciliation-coordination.md) can advance these states, and the [Volume worker](13-volume-provisioning.md) can provision retained PVCs. Computer/Workspace activation, browsers and hosted Agents remain pending. `requires_drain` records a future worker requirement. It does not prove that any process has stopped. The [runtime acceptance matrix](../../codespec/test/agent-computer.md) remains unexecuted.

## 10.2 Scope and resource authority

Use [09 Control service](09-control-service.md) to migrate/start the service and issue a credential with `definitions.manage`. Add `definitions.validate` separately if static validation is needed. Identity and organization come exclusively from the credential.

Trusted local administration grants definition permissions to an existing principal:

```bash
bazel run //:agent-computer-server -- definition-grant \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal planner \
  --kind declaration --name '*' --permission create

bazel run //:agent-computer-server -- definition-grant \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal planner \
  --kind agent --name '*' --permission create
```

These two grants suffice for a new ComputerSet containing only an external Agent. A complete ComputerSet also needs creation permission for each declared resource kind and reference permission for its catalog dependencies. Credential scope alone grants none of these permissions.

| Permission | Meaning |
| --- | --- |
| `create` | Create a new declaration/resource of the selected kind in this organization; name must be `*` |
| `manage` | Plan/update the exact name or all names with `*`; also permits referencing that resource |
| `reference` | Reference an existing resource/catalog record; does not permit changing it |

Kinds are `declaration`, `volume`, `workspace`, `sandbox`, `app`, `agent`, `computer`, plus catalog kinds below. Catalog creation uses the operator command, not `create` grants. Successful first apply gives the creator an exact-name `manage` grant on each new declaration/resource. Names remain reserved; deletion/name reuse is not implemented. Use `definition-revoke` with the same arguments to remove a grant. Runtime observe/execute/control, organization membership and OIDC grants remain separate future work.

Plan reads, operation reads and retries require current permissions for the original plan, including its original `create` grants. Removing those grants can deny access to the old receipt even after the creator received `manage`. Referencing an existing App/Sandbox also checks all transitive profile, Secret, network and Workspace dependencies; it cannot conceal a private dependency behind an accessible parent.

## 10.3 Catalog references and versions

| Kind | ComputerSet field |
| --- | --- |
| `storage_class` | Volume `storageClass` |
| `network_policy` | Sandbox `networkPolicyRef` |
| `browser_profile` | Browser App `profileRef` |
| `secret` | Agent `secretRefs` |

```bash
bazel run //:agent-computer-server -- catalog-register \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --kind storage_class --name juicefs-workspace

bazel run //:agent-computer-server -- definition-grant \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal planner \
  --kind storage_class --name juicefs-workspace --permission reference
```

Registration creates reference metadata only. It does not provision JuiceFS, validate a network policy, store a secret value, or create a browser profile. Repeating registration returns the stable ID. `catalog-disable --database-url-file PATH --organization acme --resource-id ID` disables that record and increments its revision; registration does not re-enable it. Backend configuration and capability probes are pending.

Local declaration names resolve to stable `res_…` IDs. Existing resources outside this declaration require `id:<resource_id>`; catalog references allow a name or `id:<resource_id>`. Resolution checks organization, kind and permission. Specs store resolved references and immutable dependency revisions/digests. App and Computer must select the same Sandbox version; writable Sandbox mounts must select the Computer's primary Workspace version. Changing a dependency therefore changes a dependent spec's digest when it is replanned. Previously published versions remain pinned.

## 10.4 HTTP workflow

All four endpoints require one bearer header and `definitions.manage`, use JSON, reject browser Origin, and return `Cache-Control: no-store`. The [OpenAPI contract](../../schemas/openapi-v1alpha1.json) describes the exact response schemas.

| Request | Body / header | Result |
| --- | --- | --- |
| `POST /v1alpha1/plans` | ComputerSet; `Idempotency-Key` | 201 immutable plan |
| `GET /v1alpha1/plans/{id}` | No body | 200 caller-owned plan |
| `POST /v1alpha1/plans/{id}/apply` | `{"plan_digest":"sha256:…"}`; `Idempotency-Key` | 202 operation with current state |
| `GET /v1alpha1/operations/{id}` | No body | 200 caller-owned operation |

A minimal initial plan body:

```json
{
  "apiVersion": "agent-computer/v1alpha1",
  "kind": "ComputerSet",
  "metadata": {"name": "research"},
  "spec": {
    "agents": [{"name": "external", "mode": "external", "adapter": "tools-api"}]
  }
}
```

Inspect the returned `resources`, `before`, `after`, `dependencies` and `requires_drain`, then submit its exact `plan_digest` to its apply path with a separate request key. Plans are immutable and valid for first apply for 15 minutes by database clock. The body limit is 1 MiB, serialized preview limit 8 MiB, resource limit 1024, and transitive dependency traversal limit 16384 distinct versioned references. Planning records metadata and an event but publishes no resources or intents.

For an update, set root `metadata.expectedRevision` to the current declaration revision and each existing resource's `expectedRevision` to its current revision. Successful apply increments the declaration revision even if all resource specs are unchanged. Each changed resource gets its next immutable revision; unchanged resources keep their revision and digest. New resources omit `expectedRevision`. Missing existing revisions return 428; mismatches at planning or first apply return 412. Plans also pin external resource heads and catalog versions; changing them before first apply requires replanning.

Request keys contain 1–128 ASCII letters/digits/underscores/hyphens and are scoped by organization, principal and operation. Reusing a key with changed input returns 409; a retired receipt returns 410. Retrying a plan returns the same immutable preview. Retrying an applied plan returns the same operation and current progress even with a new key; expiry does not republish or invalidate completed admission. Permissions are always rechecked. A timeout/disconnection can leave the commit outcome unknown: retry the same key/input, then query the returned ID.

Unapplied expired plans return 410, wrong digests 409, inaccessible plans 404, missing top-level grants 403, and unavailable/wrong-kind/unauthorized dependencies 422. Invalid definitions or unsupported data migrations also return 422. Errors use the common request-ID envelope.

## 10.5 Atomicity, safety and verification

One organization sequence-row lock serializes grant/catalog/definition writes and event publication. After acquiring it, planning/apply recheck the credential while holding shared credential/principal row locks through commit; revocation or principal disable that wins admission prevents the write. Expiry is checked again at transaction completion. This covers control database admission; future workers must separately reauthorize runtime dispatch.

Apply checks every selected revision, then writes all metadata in one transaction. It queues one intent for every declared resource, including unchanged resources, in dependency order. No cross-Kubernetes/S3 transaction is claimed. Omitted resources are preserved. No data is deleted. Volume storage-class changes/quota shrink and Workspace volume moves are rejected pending an explicit data migration/deletion contract.

Twelve additional real PostgreSQL cases cover seven-resource publication, stable identities/immutable history, dependency updates, concurrent retries/competing plans, final-write rollback, principal isolation, reference grants, exact pinned compatibility, revocation lock races, expired plans and retired keys. The expiry case uses a controlled database fixture to age an immutable plan; it does not wait 15 minutes. HTTP tests cover plan/apply errors and retries, and an independent TCP server exercises operator grants, catalog administration, plan/apply and credential revocation. Run the [database-enabled test commands](08-persistence.md). These establish local control-plane behavior; runtime reconciliation, distributed authorization, physical fencing and production deployment remain pending.
