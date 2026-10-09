//! Exact resource runtime grants, separate from declaration management.
//! No permission implies another, and successful checks do not establish a lease,
//! a generation, physical fencing, stopped processes, or actual runtime readiness.
pub mod connections;
pub(crate) mod inputs;
pub mod preparation;
mod start;
mod types;
pub mod writers;
use crate::{
    Error, Result, Store,
    auth::{AuthenticatedPrincipal, ServiceScope},
    plans::transactions,
};
use sqlx::{Postgres, Row, Transaction};
pub use start::{CancelQueuedStart, ComputerRuntime, StartReceipt, StartRequest, StartState};
pub use types::*;

async fn target(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    kind: RuntimeKind,
    id: &str,
) -> Result<()> {
    let query = if kind == RuntimeKind::BrowserProfile {
        "SELECT EXISTS(SELECT 1 FROM catalog_references WHERE organization=$1 AND resource_id=$2 AND kind=$3 AND enabled)"
    } else {
        "SELECT EXISTS(SELECT 1 FROM resource_definitions WHERE organization=$1 AND resource_id=$2 AND kind=$3)"
    };
    if sqlx::query_scalar::<_, bool>(query)
        .bind(org)
        .bind(id)
        .bind(kind.as_str())
        .fetch_one(&mut **tx)
        .await?
    {
        Ok(())
    } else {
        Err(Error::RuntimeAccessUnavailable)
    }
}
async fn begin<'a>(
    store: &'a Store,
    token: &str,
    scope: ServiceScope,
) -> Result<(Transaction<'a, Postgres>, AuthenticatedPrincipal, i64)> {
    let initial = store.authorize_service(token, scope).await?;
    let mut tx = store.pool.begin().await?;
    let seq = Store::lock_stream(&mut tx, initial.organization().as_str()).await?;
    let identity = Store::authorize_service_in(&mut tx, token, scope).await?;
    Ok((tx, identity, seq))
}

async fn require_in(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    requirement: &RuntimeRequirement,
) -> Result<()> {
    requirement.validate()?;
    target(
        tx,
        identity.organization().as_str(),
        requirement.kind,
        &requirement.resource_id,
    )
    .await?;
    let row = sqlx::query("SELECT max_runtime_seconds FROM runtime_grants WHERE organization=$1 AND principal=$2 AND kind=$3 AND resource_id=$4 AND permission=$5")
        .bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(requirement.kind.as_str()).bind(&requirement.resource_id).bind(requirement.permission.as_str()).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeAccessUnavailable)?;
    let max: Option<i32> = row.try_get("max_runtime_seconds")?;
    if let Some(seconds) = requirement.runtime_seconds {
        let max = max
            .filter(|n| (1..=86400).contains(n))
            .ok_or(Error::InvalidStoredData)?;
        if seconds > max as u32 {
            return Err(Error::RuntimeBudgetExceeded);
        }
    } else if max.is_some() {
        return Err(Error::InvalidStoredData);
    }
    Ok(())
}

/// Reusable transaction boundary for future runtime admission. The caller holds
/// the organization stream lock, persists the admitted effect in this transaction,
/// and rechecks the credential immediately before commit after any further work.
pub(crate) async fn authorize_in(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    requirements: &[RuntimeRequirement],
) -> Result<AuthenticatedPrincipal> {
    if requirements.is_empty() || requirements.len() > 32 {
        return Err(Error::InvalidRuntimeRequest);
    }
    let identity =
        Store::authorize_service_in(tx, token, requirements[0].permission.scope()).await?;
    let mut unique = std::collections::BTreeSet::new();
    for requirement in requirements {
        if !unique.insert((
            requirement.kind,
            &requirement.resource_id,
            requirement.permission,
        )) {
            return Err(Error::InvalidRuntimeRequest);
        }
        Store::authorize_service_in(tx, token, requirement.permission.scope()).await?;
        require_in(tx, &identity, requirement).await?;
    }
    Store::authorize_service_in(tx, token, requirements[0].permission.scope()).await?;
    Ok(identity)
}

impl Store {
    /// Trusted administration only. No service credential can grant itself access.
    /// Revocation prevents future admissions; it does not assert a running process stopped.
    pub async fn set_runtime_grant(&self, grant: RuntimeGrant<'_>, enabled: bool) -> Result<()> {
        types::validate_target(grant.kind, grant.resource_id, grant.permission)?;
        if enabled {
            RuntimeRequirement {
                kind: grant.kind,
                resource_id: grant.resource_id.into(),
                permission: grant.permission,
                runtime_seconds: grant.max_runtime_seconds,
            }
            .validate()?;
        } else if grant.max_runtime_seconds.is_some() {
            return Err(Error::InvalidRuntimeRequest);
        }
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, grant.organization.as_str()).await?;
        // A disabled profile may still have its grants revoked.
        if enabled {
            target(
                &mut tx,
                grant.organization.as_str(),
                grant.kind,
                grant.resource_id,
            )
            .await?;
        }
        let changed = if enabled {
            let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM principals WHERE organization=$1 AND principal=$2 AND enabled)")
                .bind(grant.organization.as_str()).bind(grant.principal.as_str()).fetch_one(&mut *tx).await?;
            if !active {
                return Err(Error::RuntimeAccessUnavailable);
            }
            sqlx::query("INSERT INTO runtime_grants (organization,principal,kind,resource_id,permission,max_runtime_seconds) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (organization,principal,kind,resource_id,permission) DO UPDATE SET max_runtime_seconds=EXCLUDED.max_runtime_seconds WHERE runtime_grants.max_runtime_seconds IS DISTINCT FROM EXCLUDED.max_runtime_seconds")
                .bind(grant.organization.as_str()).bind(grant.principal.as_str()).bind(grant.kind.as_str()).bind(grant.resource_id).bind(grant.permission.as_str()).bind(grant.max_runtime_seconds.map(|n| n as i32)).execute(&mut *tx).await?.rows_affected()
        } else {
            sqlx::query("DELETE FROM runtime_grants WHERE organization=$1 AND principal=$2 AND kind=$3 AND resource_id=$4 AND permission=$5")
                .bind(grant.organization.as_str()).bind(grant.principal.as_str()).bind(grant.kind.as_str()).bind(grant.resource_id).bind(grant.permission.as_str()).execute(&mut *tx).await?.rows_affected()
        };
        let revoked_connections = if !enabled
            && grant.kind == RuntimeKind::Computer
            && grant.permission == RuntimePermission::Connect
        {
            sqlx::query("UPDATE connection_sessions SET state='Revoked',revision=revision+1,revocation_revision=revocation_revision+1 WHERE organization=$1 AND principal=$2 AND computer_id=$3 AND state='Active'")
                .bind(grant.organization.as_str()).bind(grant.principal.as_str()).bind(grant.resource_id).execute(&mut *tx).await?.rows_affected()
        } else {
            0
        };
        let draining_writers = if !enabled {
            writers::invalidate_grant(&mut tx, &grant).await?
        } else {
            0
        };
        if changed > 0 || revoked_connections > 0 || draining_writers > 0 {
            transactions::emit(&mut tx, grant.organization.as_str(), seq, if enabled { "runtime.permission_changed" } else { "access.revoked" }, serde_json::json!({"principal":grant.principal.as_str(),"kind":grant.kind,"resource_id":grant.resource_id,"permission":grant.permission,"max_runtime_seconds":grant.max_runtime_seconds,"enabled":enabled,"revoked_connection_count":revoked_connections,"draining_writer_count":draining_writers,"process_termination_confirmed":false})).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Read-only current check for trusted integration. An external effect must
    /// use a fresh check in its own admission transaction and validate its lease.
    pub async fn check_runtime_permissions(
        &self,
        token: &str,
        requirements: &[RuntimeRequirement],
    ) -> Result<()> {
        if requirements.is_empty() || requirements.len() > 32 {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, _, _) = begin(self, token, requirements[0].permission.scope()).await?;
        authorize_in(&mut tx, token, requirements).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Inspect only the authenticated principal's effective permissions. Reading
    /// another resource's grants, contents or profile is never implied by this view.
    pub async fn runtime_access(
        &self,
        token: &str,
        kind: RuntimeKind,
        id: &str,
    ) -> Result<RuntimeAccess> {
        let (mut tx, _, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        let identity = authorize_in(
            &mut tx,
            token,
            &[RuntimeRequirement {
                kind,
                resource_id: id.into(),
                permission: RuntimePermission::Read,
                runtime_seconds: None,
            }],
        )
        .await?;
        let scopes: Vec<String> =
            sqlx::query_scalar("SELECT scopes FROM service_credentials WHERE credential_id=$1")
                .bind(crate::auth::token_id(token)?)
                .fetch_one(&mut *tx)
                .await?;
        let rows = sqlx::query("SELECT permission,max_runtime_seconds FROM runtime_grants WHERE organization=$1 AND principal=$2 AND kind=$3 AND resource_id=$4 ORDER BY permission")
            .bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(kind.as_str()).bind(id).fetch_all(&mut *tx).await?;
        let mut permissions = Vec::new();
        let mut max_runtime_seconds = None;
        for row in rows {
            let permission: RuntimePermission = row
                .try_get::<String, _>("permission")?
                .parse()
                .map_err(|_| Error::InvalidStoredData)?;
            if permission.accepts(kind) && scopes.iter().any(|s| s == permission.scope().as_str()) {
                permissions.push(permission);
                if permission == RuntimePermission::Activate {
                    max_runtime_seconds = Some(
                        row.try_get::<i32, _>("max_runtime_seconds")?
                            .try_into()
                            .map_err(|_| Error::InvalidStoredData)?,
                    );
                }
            }
        }
        let result = RuntimeAccess {
            kind,
            resource_id: id.into(),
            permissions,
            max_runtime_seconds,
            checked_at_ms: transactions::now(&mut tx).await?,
        };
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeRead).await?;
        tx.commit().await?;
        Ok(result)
    }
}
