use super::{access, types::*};
use crate::{Error, Result, auth::AuthenticatedPrincipal};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use std::collections::BTreeSet;

pub(super) async fn resolve(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    kind: DefinitionKind,
    value: &str,
    planned: &[PlannedResource],
) -> Result<Dependency> {
    let id = value.strip_prefix("id:");
    if let Some(resource) = planned
        .iter()
        .find(|r| r.kind == kind && id.map_or(r.name == value, |id| r.resource_id == id))
    {
        if resource.expected_revision > 0
            && !access::allowed(
                tx,
                identity,
                kind,
                &resource.name,
                DefinitionPermission::Reference,
            )
            .await?
        {
            return Err(Error::ReferenceUnavailable);
        }
        return Ok(dependency(resource));
    }
    let org = identity.organization().as_str();
    let reference = if kind.catalog() {
        let row=sqlx::query("SELECT resource_id,name,revision FROM catalog_references WHERE organization=$1 AND kind=$2 AND enabled AND (($3::text IS NOT NULL AND resource_id=$3) OR ($3::text IS NULL AND name=$4))")
            .bind(org).bind(kind.as_str()).bind(id).bind(value).fetch_optional(&mut **tx).await?.ok_or(Error::ReferenceUnavailable)?;
        let resource_id: String = row.try_get("resource_id")?;
        let name: String = row.try_get("name")?;
        let revision: i64 = row.try_get("revision")?;
        let digest = digest(
            "agent-computer/catalog-reference-v1",
            &(kind, &resource_id, &name, revision),
        )?;
        Dependency {
            kind,
            resource_id,
            name,
            revision,
            digest,
        }
    } else {
        let id = id.ok_or(Error::ReferenceUnavailable)?;
        let row=sqlx::query("SELECT d.resource_id,d.name,d.revision,v.digest FROM resource_definitions d JOIN resource_spec_versions v USING (organization,resource_id,revision) WHERE d.organization=$1 AND d.kind=$2 AND d.resource_id=$3")
            .bind(org).bind(kind.as_str()).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::ReferenceUnavailable)?;
        Dependency {
            kind,
            resource_id: row.try_get("resource_id")?,
            name: row.try_get("name")?,
            revision: row.try_get("revision")?,
            digest: row.try_get("digest")?,
        }
    };
    if !access::allowed(
        tx,
        identity,
        kind,
        &reference.name,
        DefinitionPermission::Reference,
    )
    .await?
    {
        return Err(Error::ReferenceUnavailable);
    }
    Ok(reference)
}

pub(super) fn dependency(resource: &PlannedResource) -> Dependency {
    Dependency {
        kind: resource.kind,
        resource_id: resource.resource_id.clone(),
        name: resource.name.clone(),
        revision: resource.revision,
        digest: resource.digest.clone(),
    }
}

async fn field(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    kind: DefinitionKind,
    value: &mut Value,
    planned: &[PlannedResource],
    dependencies: &mut BTreeSet<Dependency>,
) -> Result<()> {
    let reference = resolve(
        tx,
        identity,
        kind,
        value.as_str().ok_or(Error::InvalidStoredData)?,
        planned,
    )
    .await?;
    *value = Value::String(format!("id:{}", reference.resource_id));
    dependencies.insert(reference);
    Ok(())
}

pub(super) async fn bind(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    kind: DefinitionKind,
    spec: &mut Value,
    planned: &[PlannedResource],
) -> Result<Vec<Dependency>> {
    use DefinitionKind::*;
    let mut dependencies = BTreeSet::new();
    let fields: &[(&str, DefinitionKind)] = match kind {
        Volume => &[("storageClass", StorageClass)],
        Workspace => &[("volumeRef", Volume)],
        Sandbox => &[("networkPolicyRef", NetworkPolicy)],
        App => &[("sandboxRef", Sandbox), ("profileRef", BrowserProfile)],
        Agent => &[("sandboxRef", Sandbox)],
        Computer => &[("workspaceRef", Workspace)],
        _ => return Err(Error::InvalidStoredData),
    };
    for (name, target) in fields {
        if let Some(value) = spec.get_mut(*name) {
            field(tx, identity, *target, value, planned, &mut dependencies).await?;
        }
    }
    for (name, target) in [
        ("secretRefs", Secret),
        ("sandboxRefs", Sandbox),
        ("appRefs", App),
    ] {
        if let Some(values) = spec.get_mut(name).and_then(Value::as_array_mut) {
            for value in values.iter_mut() {
                field(tx, identity, target, value, planned, &mut dependencies).await?;
            }
            values.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
    }
    if let Some(mounts) = spec.get_mut("mounts").and_then(Value::as_array_mut) {
        for mount in mounts {
            field(
                tx,
                identity,
                Workspace,
                &mut mount["workspaceRef"],
                planned,
                &mut dependencies,
            )
            .await?;
        }
    }
    Ok(dependencies.into_iter().collect())
}

async fn referenced_spec(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    reference: &Dependency,
    planned: &[PlannedResource],
) -> Result<(Value, Vec<Dependency>)> {
    if let Some(resource) = planned
        .iter()
        .find(|r| r.resource_id == reference.resource_id && r.revision == reference.revision)
    {
        return Ok((resource.after.clone(), resource.dependencies.clone()));
    }
    let row=sqlx::query("SELECT spec,dependencies FROM resource_spec_versions WHERE organization=$1 AND resource_id=$2 AND revision=$3")
        .bind(org).bind(&reference.resource_id).bind(reference.revision).fetch_one(&mut **tx).await?;
    Ok((
        row.try_get("spec")?,
        serde_json::from_value(row.try_get("dependencies")?)
            .map_err(|_| Error::InvalidStoredData)?,
    ))
}

pub(super) async fn compatibility(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    resource: &PlannedResource,
    planned: &[PlannedResource],
) -> Result<()> {
    if resource.kind != DefinitionKind::Computer {
        return Ok(());
    }
    let sandboxes = resource.after["sandboxRefs"]
        .as_array()
        .ok_or(Error::InvalidStoredData)?;
    for reference in &resource.dependencies {
        if !matches!(
            reference.kind,
            DefinitionKind::App | DefinitionKind::Sandbox
        ) {
            continue;
        }
        let (spec, dependencies) = referenced_spec(tx, org, reference, planned).await?;
        if reference.kind == DefinitionKind::App && !sandboxes.contains(&spec["sandboxRef"]) {
            return Err(Error::ReferenceUnavailable);
        }
        if reference.kind == DefinitionKind::App
            && !dependencies
                .iter()
                .filter(|d| d.kind == DefinitionKind::Sandbox)
                .all(|d| resource.dependencies.contains(d))
        {
            return Err(Error::ReferenceUnavailable);
        }
        if reference.kind == DefinitionKind::Sandbox {
            for mount in spec["mounts"].as_array().ok_or(Error::InvalidStoredData)? {
                if mount["readOnly"] == false
                    && mount["workspaceRef"] != resource.after["workspaceRef"]
                {
                    return Err(Error::ReferenceUnavailable);
                }
                if mount["readOnly"] == false
                    && !dependencies
                        .iter()
                        .filter(|d| {
                            d.kind == DefinitionKind::Workspace
                                && mount["workspaceRef"] == format!("id:{}", d.resource_id)
                        })
                        .all(|d| resource.dependencies.contains(d))
                {
                    return Err(Error::ReferenceUnavailable);
                }
            }
        }
    }
    Ok(())
}

/// Referencing an existing App/Sandbox never skips the private profile, Secret,
/// or workspace grants of the immutable versions behind it.
pub(super) async fn authorize_tree(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    planned: &[PlannedResource],
) -> Result<()> {
    let mut queue: Vec<Dependency> = planned
        .iter()
        .flat_map(|r| r.dependencies.clone())
        .collect();
    let mut seen = BTreeSet::new();
    while let Some(reference) = queue.pop() {
        if !seen.insert(reference.clone()) {
            continue;
        }
        if seen.len() > 16384 {
            return Err(Error::PlanTooLarge);
        }
        let new = planned
            .iter()
            .any(|r| r.resource_id == reference.resource_id && r.expected_revision == 0);
        if !new
            && !access::allowed(
                tx,
                identity,
                reference.kind,
                &reference.name,
                DefinitionPermission::Reference,
            )
            .await?
        {
            return Err(Error::ReferenceUnavailable);
        }
        if reference.kind.catalog() {
            if resolve(
                tx,
                identity,
                reference.kind,
                &format!("id:{}", reference.resource_id),
                &[],
            )
            .await?
                != reference
            {
                return Err(Error::RevisionConflict);
            }
        } else {
            let (_, children) =
                referenced_spec(tx, identity.organization().as_str(), &reference, planned).await?;
            queue.extend(children);
        }
    }
    Ok(())
}

pub(super) async fn check_dependencies(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    plan: &DefinitionPlan,
) -> Result<()> {
    for resource in &plan.resources {
        for reference in &resource.dependencies {
            if plan
                .resources
                .iter()
                .any(|r| r.resource_id == reference.resource_id)
            {
                continue;
            }
            let current = resolve(
                tx,
                identity,
                reference.kind,
                &format!("id:{}", reference.resource_id),
                &[],
            )
            .await?;
            if &current != reference {
                return Err(Error::RevisionConflict);
            }
        }
    }
    Ok(())
}
