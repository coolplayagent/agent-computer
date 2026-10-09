use super::*;
use crate::plans::{DefinitionKind, Dependency};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PinnedResource {
    pub reference: Dependency,
    pub spec: Value,
    pub dependencies: Vec<Dependency>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub resources: Vec<PinnedResource>,
    pub requirements: Vec<RuntimeRequirement>,
}

pub(super) struct Admission {
    pub snapshot: Snapshot,
    pub workspace: String,
    pub volume: String,
    pub volume_quota: i64,
    pub cpu: i64,
    pub memory: i64,
}

async fn load(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    reference: &Dependency,
) -> Result<PinnedResource> {
    if reference.kind.catalog() {
        let row = sqlx::query("SELECT kind,name,revision,enabled FROM catalog_references WHERE organization=$1 AND resource_id=$2")
            .bind(org).bind(&reference.resource_id).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeAccessUnavailable)?;
        let name: String = row.try_get("name")?;
        let revision: i64 = row.try_get("revision")?;
        if !row.try_get::<bool, _>("enabled")?
            || row.try_get::<String, _>("kind")? != reference.kind.as_str()
            || name != reference.name
            || revision != reference.revision
            || digest(
                "agent-computer/catalog-reference-v1",
                &(reference.kind, &reference.resource_id, &name, revision),
            )? != reference.digest
        {
            return Err(Error::ReferenceUnavailable);
        }
        return Ok(PinnedResource {
            reference: reference.clone(),
            spec: Value::Null,
            dependencies: vec![],
        });
    }
    let row = sqlx::query("SELECT d.kind,d.name,v.digest,v.spec,v.dependencies FROM resource_spec_versions v JOIN resource_definitions d USING (organization,resource_id) WHERE v.organization=$1 AND v.resource_id=$2 AND v.revision=$3")
        .bind(org).bind(&reference.resource_id).bind(reference.revision).fetch_optional(&mut **tx).await?.ok_or(Error::InvalidStoredData)?;
    let spec: Value = row.try_get("spec")?;
    let dependencies: Vec<Dependency> = serde_json::from_value(row.try_get("dependencies")?)
        .map_err(|_| Error::InvalidStoredData)?;
    if row.try_get::<String, _>("kind")? != reference.kind.as_str()
        || row.try_get::<String, _>("name")? != reference.name
        || row.try_get::<String, _>("digest")? != reference.digest
        || digest(
            "agent-computer/resource-spec-v1",
            &(reference.kind, &spec, &dependencies),
        )? != reference.digest
    {
        return Err(Error::InvalidStoredData);
    }
    Ok(PinnedResource {
        reference: reference.clone(),
        spec,
        dependencies,
    })
}

fn reference_id(value: &Value) -> Result<&str> {
    value
        .as_str()
        .and_then(|s| s.strip_prefix("id:"))
        .ok_or(Error::InvalidStoredData)
}

impl Snapshot {
    pub(crate) fn resource(&self, kind: DefinitionKind, id: &str) -> Result<&PinnedResource> {
        self.resources
            .iter()
            .find(|r| r.reference.kind == kind && r.reference.resource_id == id)
            .ok_or(Error::InvalidStoredData)
    }
}

pub(super) async fn capture(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    computer: &str,
    request: &StartRequest,
) -> Result<Admission> {
    let row = sqlx::query("SELECT d.name,d.revision,v.digest FROM resource_definitions d JOIN resource_spec_versions v USING (organization,resource_id,revision) WHERE d.organization=$1 AND d.resource_id=$2 AND d.kind='computer'")
        .bind(org).bind(computer).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeAccessUnavailable)?;
    let revision: i64 = row.try_get("revision")?;
    if revision != request.expected_spec_revision {
        return Err(Error::RevisionConflict);
    }
    let mut queue = vec![Dependency {
        kind: DefinitionKind::Computer,
        resource_id: computer.into(),
        name: row.try_get("name")?,
        revision,
        digest: row.try_get("digest")?,
    }];
    let mut seen: BTreeMap<String, PinnedResource> = BTreeMap::new();
    let mut bytes = 0;
    while let Some(reference) = queue.pop() {
        if let Some(previous) = seen.get(&reference.resource_id) {
            // One resource cannot have two different pinned versions in a run.
            if previous.reference != reference {
                return Err(Error::ReferenceUnavailable);
            }
            continue;
        }
        if seen.len() >= 256 {
            return Err(Error::PlanTooLarge);
        }
        let resource = load(tx, org, &reference).await?;
        bytes += serde_json::to_vec(&resource)
            .map_err(|_| Error::InvalidStoredData)?
            .len();
        if bytes > 1024 * 1024 {
            return Err(Error::PlanTooLarge);
        }
        queue.extend(resource.dependencies.clone());
        seen.insert(reference.resource_id.clone(), resource);
    }
    let mut snapshot = Snapshot {
        resources: seen.into_values().collect(),
        requirements: vec![],
    };
    let root = snapshot.resource(DefinitionKind::Computer, computer)?;
    let workspace = reference_id(&root.spec["workspaceRef"])?.to_owned();
    let workspace_spec = snapshot.resource(DefinitionKind::Workspace, &workspace)?;
    let volume = reference_id(&workspace_spec.spec["volumeRef"])?.to_owned();
    let volume_quota = snapshot.resource(DefinitionKind::Volume, &volume)?.spec["quotaBytes"]
        .as_i64()
        .ok_or(Error::InvalidStoredData)?;
    let mut cpu = 0i64;
    let mut memory = 0i64;
    let mut requirements = BTreeSet::new();
    requirements.insert((
        RuntimeKind::Computer,
        computer.to_owned(),
        RuntimePermission::Activate,
    ));
    requirements.insert((
        RuntimeKind::Workspace,
        workspace.clone(),
        RuntimePermission::Modify,
    ));
    for resource in &snapshot.resources {
        let id = &resource.reference.resource_id;
        match resource.reference.kind {
            DefinitionKind::Workspace => {
                requirements.insert((RuntimeKind::Workspace, id.clone(), RuntimePermission::Read));
            }
            DefinitionKind::App => {
                requirements.insert((RuntimeKind::App, id.clone(), RuntimePermission::AppUse));
                requirements.insert((RuntimeKind::App, id.clone(), RuntimePermission::Activate));
            }
            DefinitionKind::BrowserProfile => {
                requirements.insert((
                    RuntimeKind::BrowserProfile,
                    id.clone(),
                    RuntimePermission::AppUse,
                ));
            }
            DefinitionKind::Sandbox => {
                cpu = cpu
                    .checked_add(
                        resource.spec["resources"]["cpuMillis"]
                            .as_i64()
                            .ok_or(Error::InvalidStoredData)?,
                    )
                    .ok_or(Error::CounterExhausted)?;
                memory = memory
                    .checked_add(
                        resource.spec["resources"]["memoryMiB"]
                            .as_i64()
                            .ok_or(Error::InvalidStoredData)?,
                    )
                    .ok_or(Error::CounterExhausted)?;
            }
            DefinitionKind::Computer
            | DefinitionKind::Volume
            | DefinitionKind::StorageClass
            | DefinitionKind::NetworkPolicy => {}
            // No credential/Secret material is captured or implicitly authorized.
            _ => return Err(Error::ReferenceUnavailable),
        }
    }
    if requirements.len() > 32 {
        return Err(Error::PlanTooLarge);
    }
    snapshot.requirements = requirements
        .into_iter()
        .map(|(kind, resource_id, permission)| RuntimeRequirement {
            kind,
            resource_id,
            permission,
            runtime_seconds: (permission == RuntimePermission::Activate)
                .then_some(request.max_runtime_seconds),
        })
        .collect();
    Ok(Admission {
        snapshot,
        workspace,
        volume,
        volume_quota,
        cpu,
        memory,
    })
}

pub(super) async fn reauthorize(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    org: &str,
    request_id: &str,
) -> Result<()> {
    let row = sqlx::query("SELECT snapshot,snapshot_digest FROM runtime_start_requests WHERE organization=$1 AND request_id=$2")
        .bind(org).bind(request_id).fetch_one(&mut **tx).await?;
    let snapshot: Snapshot =
        serde_json::from_value(row.try_get("snapshot")?).map_err(|_| Error::InvalidStoredData)?;
    if digest("agent-computer/start-snapshot-v1", &snapshot)?
        != row.try_get::<String, _>("snapshot_digest")?
    {
        return Err(Error::InvalidStoredData);
    }
    for resource in &snapshot.resources {
        if resource.reference.kind.catalog() {
            load(tx, org, &resource.reference).await?;
        }
    }
    authorize_in(tx, token, &snapshot.requirements).await?;
    Ok(())
}

/// Recheck the original admitted credential, all grants and catalog versions.
pub(crate) async fn authorize_bound(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    request: &str,
) -> Result<Snapshot> {
    let row = sqlx::query("SELECT r.snapshot,r.snapshot_digest,r.credential_id,r.principal FROM runtime_start_requests r JOIN runtime_controls c ON c.organization=r.organization AND c.computer_id=r.computer_id AND c.active_request=r.request_id AND c.generation=r.generation WHERE r.organization=$1 AND r.request_id=$2 AND r.state IN ('Queued','Preparing','Prepared')")
        .bind(org).bind(request).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeConflict)?;
    let snapshot: Snapshot =
        serde_json::from_value(row.try_get("snapshot")?).map_err(|_| Error::InvalidStoredData)?;
    if digest("agent-computer/start-snapshot-v1", &snapshot)?
        != row.try_get::<String, _>("snapshot_digest")?
    {
        return Err(Error::InvalidStoredData);
    }
    let credential: String = row.try_get("credential_id")?;
    let principal: String = row.try_get("principal")?;
    if snapshot.requirements.is_empty() || snapshot.requirements.len() > 32 {
        return Err(Error::InvalidStoredData);
    }
    for requirement in &snapshot.requirements {
        let identity = Store::authorize_bound_runtime_in(
            tx,
            org,
            &principal,
            &credential,
            requirement.permission.scope(),
        )
        .await?;
        require_in(tx, &identity, requirement).await?;
    }
    for resource in &snapshot.resources {
        if resource.reference.kind.catalog() {
            load(tx, org, &resource.reference).await?;
        }
    }
    Ok(snapshot)
}
