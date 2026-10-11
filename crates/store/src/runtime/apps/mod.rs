//! Durable App reservations. No launch, GUI authority or readiness is implied.
mod authority;
pub mod browser;
mod types;
use super::*;
use crate::plans::types::{digest, random_id};
use agent_computer_core::identity::{ComputerId, IdempotencyKey, OrganizationId, PrincipalId};
use serde_json::{Value, json};
use sqlx::postgres::PgRow;
pub use types::*;

fn valid(id: &str) -> Result<()> {
    ComputerId::new(id)
        .map(|_| ())
        .map_err(|_| Error::InvalidRuntimeRequest)
}
async fn row(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<PgRow> {
    valid(id)?;
    sqlx::query("SELECT * FROM app_instances WHERE organization=$1 AND instance_id=$2")
        .bind(org)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::RuntimeAccessUnavailable)
}
async fn own(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    identity: &AuthenticatedPrincipal,
    id: &str,
    permission: RuntimePermission,
) -> Result<PgRow> {
    let row = row(tx, identity.organization().as_str(), id).await?;
    if row.try_get::<String, _>("principal")? != identity.principal().as_str() {
        return Err(Error::RuntimeAccessUnavailable);
    }
    authorize_in(
        tx,
        token,
        &[
            authority::requirement(
                RuntimeKind::Computer,
                &row.try_get::<String, _>("computer_id")?,
                permission,
                0,
            ),
            authority::requirement(
                RuntimeKind::App,
                &row.try_get::<String, _>("app_id")?,
                permission,
                0,
            ),
        ],
    )
    .await?;
    Ok(row)
}
async fn view(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<AppInstance> {
    let row = row(tx, org, id).await?;
    let binding: Value = row.try_get("binding")?;
    if digest("agent-computer/app-binding-v1", &binding)?
        != row.try_get::<String, _>("binding_digest")?
    {
        return Err(Error::InvalidStoredData);
    }
    Ok(AppInstance {
        instance_id: id.into(),
        computer_id: row.try_get("computer_id")?,
        computer_generation: row.try_get("computer_generation")?,
        app_id: row.try_get("app_id")?,
        app_generation: row.try_get("app_generation")?,
        app_revision: binding["app"]["reference"]["revision"]
            .as_i64()
            .ok_or(Error::InvalidStoredData)?,
        sandbox_id: row.try_get("sandbox_id")?,
        profile_id: row.try_get("profile_id")?,
        profile_owner_id: row.try_get("principal")?,
        connection_session_id: row.try_get("session_id")?,
        binding_digest: row.try_get("binding_digest")?,
        revision: row.try_get("revision")?,
        state: serde_json::from_value(row.try_get::<String, _>("state")?.into())
            .map_err(|_| Error::InvalidStoredData)?,
        ready: false,
        created_at_ms: row.try_get("created_at_ms")?,
        expires_at_ms: row.try_get("expires_at_ms")?,
        checked_at_ms: transactions::now(tx).await?,
    })
}

pub(super) async fn invalidate_grant(
    tx: &mut Transaction<'_, Postgres>,
    grant: &RuntimeGrant<'_>,
    enabled: bool,
) -> Result<()> {
    // A reduced activation budget also permanently invalidates an existing reservation.
    let relevant = matches!(
        (grant.kind, grant.permission),
        (
            RuntimeKind::Computer,
            RuntimePermission::Connect
                | RuntimePermission::Read
                | RuntimePermission::AppUse
                | RuntimePermission::Activate
        ) | (
            RuntimeKind::App,
            RuntimePermission::Read | RuntimePermission::AppUse | RuntimePermission::Activate
        ) | (RuntimeKind::BrowserProfile, RuntimePermission::AppUse)
    );
    if relevant && (!enabled || grant.permission == RuntimePermission::Activate) {
        sqlx::query("UPDATE app_instances SET state=CASE WHEN state='Queued' THEN 'Blocked' ELSE 'Draining' END,revision=revision+1 WHERE organization=$1 AND principal=$2 AND state IN ('Queued','Dispatching','Running') AND (($3='computer' AND computer_id=$4) OR ($3='app' AND app_id=$4) OR ($3='browser_profile' AND profile_id=$4)) AND (NOT $5 OR runtime_seconds>$6)")
            .bind(grant.organization.as_str()).bind(grant.principal.as_str()).bind(grant.kind.as_str()).bind(grant.resource_id).bind(enabled).bind(grant.max_runtime_seconds.map(|n|n as i32)).execute(&mut **tx).await?;
    }
    if !enabled
        && matches!(
            grant.permission,
            RuntimePermission::Control
                | RuntimePermission::Observe
                | RuntimePermission::Read
                | RuntimePermission::Connect
                | RuntimePermission::AppUse
        )
    {
        sqlx::query("UPDATE browser_controls c SET state='Draining' FROM app_instances a WHERE c.organization=a.organization AND c.instance_id=a.instance_id AND c.organization=$1 AND c.principal=$2 AND c.state='Owned' AND (($3='computer' AND a.computer_id=$4) OR ($3='app' AND a.app_id=$4) OR ($3='browser_profile' AND a.profile_id=$4))")
            .bind(grant.organization.as_str()).bind(grant.principal.as_str()).bind(grant.kind.as_str()).bind(grant.resource_id).execute(&mut **tx).await?;
    }
    Ok(())
}

impl Store {
    /// Trusted administration only. Granting AppUse does not transfer a profile's subject.
    /// Ownership is immutable; migration/checkpoint transfer needs a separate protocol.
    pub async fn register_browser_profile_owner(
        &self,
        organization: &OrganizationId,
        profile: &str,
        principal: &PrincipalId,
    ) -> Result<()> {
        valid(profile)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, organization.as_str()).await?;
        target(
            &mut tx,
            organization.as_str(),
            RuntimeKind::BrowserProfile,
            profile,
        )
        .await?;
        let enabled: bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM principals WHERE organization=$1 AND principal=$2 AND enabled)").bind(organization.as_str()).bind(principal.as_str()).fetch_one(&mut *tx).await?;
        if !enabled {
            return Err(Error::RuntimeAccessUnavailable);
        }
        let owner: Option<String> = sqlx::query_scalar(
            "SELECT principal FROM browser_profile_owners WHERE organization=$1 AND profile_id=$2",
        )
        .bind(organization.as_str())
        .bind(profile)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(owner) = owner {
            if owner != principal.as_str() {
                return Err(Error::RuntimeConflict);
            }
        } else {
            sqlx::query("INSERT INTO browser_profile_owners (organization,profile_id,principal) VALUES ($1,$2,$3)").bind(organization.as_str()).bind(profile).bind(principal.as_str()).execute(&mut *tx).await?;
            transactions::emit(
                &mut tx,
                organization.as_str(),
                seq,
                "browser_profile.owner_registered",
                json!({"profile_id":profile,"principal":principal.as_str()}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn admit_app(
        &self,
        token: &str,
        key: &IdempotencyKey,
        computer: &str,
        input: &AdmitApp,
    ) -> Result<AppInstance> {
        for id in [computer, &input.app_id, &input.connection_session_id] {
            valid(id)?;
        }
        if input.computer_generation < 1
            || input.expected_app_revision < 1
            || !(1..=300).contains(&input.runtime_seconds)
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeActivate).await?;
        let org = identity.organization().as_str();
        let hash = digest(
            "agent-computer/app-admission-v1",
            &(computer, input, crate::auth::token_id(token)?),
        )?;
        let op = "runtime.app-admission.v1";
        if let Some(id) = transactions::retry::<String>(&mut tx, &identity, op, key, &hash).await? {
            let current = own(&mut tx, token, &identity, &id, RuntimePermission::Read).await?;
            authority::reconcile(&mut tx, org, &current, seq).await?;
            let result = view(&mut tx, org, &id).await?;
            Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeActivate).await?;
            tx.commit().await?;
            return Ok(result);
        }
        // Authorize the caller's known resource IDs before loading their pinned
        // definitions or reporting revision/generation details.
        authorize_in(
            &mut tx,
            token,
            &authority::requirements(computer, &input.app_id, None, input.runtime_seconds),
        )
        .await?;
        let session =
            connections::final_view(&mut tx, token, &identity, &input.connection_session_id)
                .await?;
        if session.computer_id != computer {
            return Err(Error::RuntimeAccessUnavailable);
        }
        if session.state != connections::ConnectionState::Active {
            return Err(Error::ConnectionInactive);
        }
        if !authority::CAPABILITIES
            .iter()
            .all(|p| session.capabilities.contains(p))
        {
            return Err(Error::RuntimeAccessUnavailable);
        }
        let (request, binding, sandbox, profile) =
            authority::capture(&mut tx, org, computer, input).await?;
        authorize_in(
            &mut tx,
            token,
            &authority::requirements(
                computer,
                &input.app_id,
                Some(&profile),
                input.runtime_seconds,
            ),
        )
        .await?;
        {
            let owner: bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM browser_profile_owners WHERE organization=$1 AND profile_id=$2 AND principal=$3)").bind(org).bind(&profile).bind(identity.principal().as_str()).fetch_one(&mut *tx).await?;
            if !owner {
                return Err(Error::RuntimeAccessUnavailable);
            }
        }
        let capacity=sqlx::query("SELECT count(*)::bigint AS total,count(*) FILTER(WHERE principal=$2)::bigint AS actor, count(*) FILTER(WHERE (computer_id=$3 AND app_id=$4) OR (request_id=$5 AND sandbox_id=$6) OR profile_id=$7)::bigint AS occupied FROM app_instances WHERE organization=$1 AND state<>'Cancelled'")
            .bind(org).bind(identity.principal().as_str()).bind(computer).bind(&input.app_id).bind(&request).bind(&sandbox).bind(&profile).fetch_one(&mut *tx).await?;
        if capacity.try_get::<i64, _>("occupied")? > 0 {
            return Err(Error::RuntimeConflict);
        }
        if capacity.try_get::<i64, _>("total")? >= 64 || capacity.try_get::<i64, _>("actor")? >= 8 {
            return Err(Error::RuntimeCapacityUnavailable);
        }
        let generation: i64=sqlx::query_scalar("SELECT COALESCE(max(app_generation),0) FROM app_instances WHERE organization=$1 AND computer_id=$2 AND app_id=$3").bind(org).bind(computer).bind(&input.app_id).fetch_one(&mut *tx).await?;
        let generation = generation.checked_add(1).ok_or(Error::CounterExhausted)?;
        let now = transactions::now(&mut tx).await?;
        let expires = now
            .checked_add(i64::from(input.runtime_seconds) * 1000)
            .ok_or(Error::CounterExhausted)?
            .min(session.expires_at_ms);
        if expires <= now {
            return Err(Error::ConnectionInactive);
        }
        let id = random_id("app")?;
        let binding_hash = digest("agent-computer/app-binding-v1", &binding)?;
        sqlx::query("INSERT INTO app_instances (organization,instance_id,request_id,computer_id,computer_generation,app_id,app_generation,sandbox_id,profile_id,principal,credential_id,session_id,runtime_seconds,binding,binding_digest,created_at_ms,expires_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)")
            .bind(org).bind(&id).bind(&request).bind(computer).bind(input.computer_generation).bind(&input.app_id).bind(generation).bind(&sandbox).bind(&profile).bind(identity.principal().as_str()).bind(crate::auth::token_id(token)?).bind(&input.connection_session_id).bind(input.runtime_seconds as i32).bind(&binding).bind(&binding_hash).bind(now).bind(expires).execute(&mut *tx).await?;
        transactions::emit(&mut tx,org,seq,"app.admitted",json!({"instance_id":id,"computer_id":computer,"computer_generation":input.computer_generation,"app_id":input.app_id,"app_generation":generation,"binding_digest":binding_hash,"dispatch_started":false})).await?;
        transactions::save_receipt(&mut tx, &identity, op, key, &hash, &id).await?;
        let current = row(&mut tx, org, &id).await?;
        authority::bound(&mut tx, org, &current).await?;
        let result = view(&mut tx, org, &id).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn app_instance(&self, token: &str, id: &str) -> Result<AppInstance> {
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeRead).await?;
        let current = own(&mut tx, token, &identity, id, RuntimePermission::Read).await?;
        authority::reconcile(&mut tx, identity.organization().as_str(), &current, seq).await?;
        let result = view(&mut tx, identity.organization().as_str(), id).await?;
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeRead).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Only an undispatched reservation can be cancelled. There is no caller-supplied
    /// stopped/fenced flag and this receipt is not evidence about any App process.
    pub async fn cancel_app(
        &self,
        token: &str,
        key: &IdempotencyKey,
        id: &str,
        input: &CancelApp,
    ) -> Result<AppInstance> {
        if input.app_generation < 1 || input.expected_revision < 1 {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeManage).await?;
        let current = own(&mut tx, token, &identity, id, RuntimePermission::Manage).await?;
        let org = identity.organization().as_str();
        let op = "runtime.app-cancel.v1";
        let hash = digest("agent-computer/app-cancel-v1", &(id, input))?;
        if transactions::retry::<String>(&mut tx, &identity, op, key, &hash)
            .await?
            .is_none()
        {
            if current.try_get::<i64, _>("app_generation")? != input.app_generation
                || current.try_get::<i64, _>("revision")? != input.expected_revision
                || !matches!(
                    current.try_get::<String, _>("state")?.as_str(),
                    "Queued" | "Blocked"
                )
            {
                return Err(Error::RuntimeConflict);
            }
            sqlx::query("UPDATE app_instances SET state='Cancelled',revision=revision+1 WHERE organization=$1 AND instance_id=$2").bind(org).bind(id).execute(&mut *tx).await?;
            transactions::emit(&mut tx,org,seq,"app.admission_cancelled",json!({"instance_id":id,"app_generation":input.app_generation,"dispatch_started":false})).await?;
            transactions::save_receipt(&mut tx, &identity, op, key, &hash, &id).await?;
        }
        let result = view(&mut tx, org, id).await?;
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeManage).await?;
        tx.commit().await?;
        Ok(result)
    }
}
