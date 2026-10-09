use super::{references, transactions, types::*};
use crate::{Error, Result, Store, auth::AuthenticatedPrincipal};
use agent_computer_core::identity::OrganizationId;
use sqlx::{Postgres, Transaction};

pub(super) async fn allowed(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    kind: DefinitionKind,
    name: &str,
    permission: DefinitionPermission,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM definition_grants WHERE organization=$1 AND principal=$2 AND kind=$3 AND name IN ($4,'*') AND (permission=$5 OR ($5='reference' AND permission='manage')))")
        .bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(kind.as_str()).bind(name).bind(permission.as_str()).fetch_one(&mut **tx).await?)
}
pub(super) async fn require(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    kind: DefinitionKind,
    name: &str,
    permission: DefinitionPermission,
) -> Result<()> {
    if allowed(tx, identity, kind, name, permission).await? {
        Ok(())
    } else {
        Err(Error::Forbidden)
    }
}
pub(super) async fn authorize_plan(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    plan: &DefinitionPlan,
) -> Result<()> {
    require(
        tx,
        identity,
        DefinitionKind::Declaration,
        &plan.declaration_name,
        if plan.declaration_expected_revision == 0 {
            DefinitionPermission::Create
        } else {
            DefinitionPermission::Manage
        },
    )
    .await?;
    for resource in &plan.resources {
        require(
            tx,
            identity,
            resource.kind,
            &resource.name,
            if resource.expected_revision == 0 {
                DefinitionPermission::Create
            } else {
                DefinitionPermission::Manage
            },
        )
        .await?;
        for reference in &resource.dependencies {
            // New resources declared in this same plan receive creator grants at apply.
            if plan
                .resources
                .iter()
                .any(|r| r.resource_id == reference.resource_id && r.expected_revision == 0)
            {
                continue;
            }
            if !allowed(
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
        }
    }
    references::authorize_tree(tx, identity, &plan.resources).await?;
    Ok(())
}

fn valid_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
}

impl Store {
    /// Trusted local administration. HTTP callers cannot grant themselves access.
    pub async fn set_definition_grant(
        &self,
        grant: DefinitionGrant<'_>,
        enabled: bool,
    ) -> Result<()> {
        if !(valid_name(grant.name) || grant.name == "*")
            || (grant.permission == DefinitionPermission::Create
                && (grant.name != "*" || grant.kind.catalog()))
        {
            return Err(Error::InvalidCredentialParameters);
        }
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, grant.organization.as_str()).await?;
        let sql = if enabled {
            "INSERT INTO definition_grants (organization,principal,kind,name,permission) VALUES ($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING"
        } else {
            "DELETE FROM definition_grants WHERE organization=$1 AND principal=$2 AND kind=$3 AND name=$4 AND permission=$5"
        };
        let changed = sqlx::query(sql)
            .bind(grant.organization.as_str())
            .bind(grant.principal.as_str())
            .bind(grant.kind.as_str())
            .bind(grant.name)
            .bind(grant.permission.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if changed > 0 {
            transactions::emit(&mut tx,grant.organization.as_str(),seq,"definition.permission_changed",serde_json::json!({"principal":grant.principal.as_str(),"kind":grant.kind,"name":grant.name,"permission":grant.permission,"enabled":enabled})).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Register an external definition reference; no secret payload or backend is
    /// created. Re-registering a name returns its stable ID without enabling it.
    pub async fn register_catalog_reference(
        &self,
        org: &OrganizationId,
        kind: DefinitionKind,
        name: &str,
    ) -> Result<String> {
        if !kind.catalog() || !valid_name(name) {
            return Err(Error::InvalidCredentialParameters);
        }
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let previous:Option<String>=sqlx::query_scalar("SELECT resource_id FROM catalog_references WHERE organization=$1 AND kind=$2 AND name=$3").bind(org.as_str()).bind(kind.as_str()).bind(name).fetch_optional(&mut *tx).await?;
        let id = if let Some(id) = previous {
            id
        } else {
            let id = random_id("ref")?;
            sqlx::query("INSERT INTO catalog_references (organization,resource_id,kind,name) VALUES ($1,$2,$3,$4)").bind(org.as_str()).bind(&id).bind(kind.as_str()).bind(name).execute(&mut *tx).await?;
            transactions::emit(
                &mut tx,
                org.as_str(),
                seq,
                "catalog.registered",
                serde_json::json!({"resource_id":id,"kind":kind,"name":name}),
            )
            .await?;
            id
        };
        tx.commit().await?;
        Ok(id)
    }

    pub async fn disable_catalog_reference(&self, org: &OrganizationId, id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let changed=sqlx::query("UPDATE catalog_references SET enabled=FALSE,revision=revision+1 WHERE organization=$1 AND resource_id=$2 AND enabled AND revision<9223372036854775807").bind(org.as_str()).bind(id).execute(&mut *tx).await?.rows_affected();
        if changed > 0 {
            transactions::emit(
                &mut tx,
                org.as_str(),
                seq,
                "catalog.disabled",
                serde_json::json!({"resource_id":id}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(changed > 0)
    }
}
