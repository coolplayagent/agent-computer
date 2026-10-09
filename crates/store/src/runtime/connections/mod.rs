//! Credential-bound logical connections; never implicitly activate compute.
mod types;
use super::*;
use crate::plans::types::{digest, random_id};
use agent_computer_core::identity::{ComputerId, IdempotencyKey};
use serde::de::DeserializeOwned;
pub use types::*;

const CONNECT: &str = "runtime.connect.v1";
const HEARTBEAT: &str = "runtime.connection-heartbeat.v1";

fn decode<T: DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| Error::InvalidStoredData)
}

async fn view(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    identity: &AuthenticatedPrincipal,
    id: &str,
) -> Result<ConnectionSession> {
    if ComputerId::new(id).is_err() {
        return Err(Error::InvalidRuntimeRequest);
    }
    let row = sqlx::query("SELECT s.*,c.scopes FROM connection_sessions s JOIN service_credentials c ON c.credential_id=s.credential_id AND c.organization=s.organization AND c.principal=s.principal WHERE s.organization=$1 AND s.principal=$2 AND s.credential_id=$3 AND s.session_id=$4")
        .bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(crate::auth::token_id(token)?).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeAccessUnavailable)?;
    let requested: Vec<RuntimePermission> = decode(row.try_get("requested")?)?;
    let scopes: Vec<String> = row.try_get("scopes")?;
    let computer: String = row.try_get("computer_id")?;
    let grants = sqlx::query("SELECT permission,max_runtime_seconds FROM runtime_grants WHERE organization=$1 AND principal=$2 AND kind='computer' AND resource_id=$3")
        .bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(&computer).fetch_all(&mut **tx).await?;
    let mut capabilities = Vec::new();
    let mut max_runtime_seconds = None;
    for grant in grants {
        let permission: RuntimePermission = grant
            .try_get::<String, _>("permission")?
            .parse()
            .map_err(|_| Error::InvalidStoredData)?;
        if requested.contains(&permission)
            && scopes.iter().any(|s| s == permission.scope().as_str())
        {
            capabilities.push(permission);
            if permission == RuntimePermission::Activate {
                let seconds: i32 = grant.try_get("max_runtime_seconds")?;
                if !(1..=86400).contains(&seconds) {
                    return Err(Error::InvalidStoredData);
                }
                max_runtime_seconds = Some(seconds as u32);
            }
        }
    }
    capabilities.sort_unstable();
    let now = transactions::now(tx).await?;
    let expires = row.try_get("expires_at_ms")?;
    let mut state: ConnectionState = decode(row.try_get::<String, _>("state")?.into())?;
    if state == ConnectionState::Active {
        if now >= expires {
            state = ConnectionState::Expired;
        } else if !capabilities.contains(&RuntimePermission::Connect) {
            state = ConnectionState::Revoked;
        }
    }
    if state != ConnectionState::Active {
        capabilities.clear();
        max_runtime_seconds = None;
    }
    Ok(ConnectionSession {
        session_id: id.into(),
        computer_id: computer,
        principal_id: identity.principal().as_str().into(),
        principal_kind: identity.kind().as_str().into(),
        revision: row.try_get("revision")?,
        revocation_revision: row.try_get("revocation_revision")?,
        state,
        requested_capabilities: requested,
        capabilities,
        max_runtime_seconds,
        created_at_ms: row.try_get("created_at_ms")?,
        expires_at_ms: expires,
        checked_at_ms: now,
        last_seen_at_ms: row.try_get("last_seen_at_ms")?,
        activity: decode(row.try_get::<String, _>("activity")?.into())?,
        visibility: decode(row.try_get::<String, _>("visibility")?.into())?,
    })
}

async fn final_view(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    identity: &AuthenticatedPrincipal,
    id: &str,
) -> Result<ConnectionSession> {
    // Take time after all writes; neither a delayed heartbeat nor a retry renews
    // the original identity or revives an expired/closed session.
    let mut result = view(tx, token, identity, id).await?;
    Store::authorize_service_in(tx, token, ServiceScope::RuntimeConnect).await?;
    result.checked_at_ms = transactions::now(tx).await?;
    if result.state == ConnectionState::Active && result.checked_at_ms >= result.expires_at_ms {
        result.state = ConnectionState::Expired;
        result.capabilities.clear();
        result.max_runtime_seconds = None;
    }
    Ok(result)
}

impl Store {
    pub async fn create_connection_session(
        &self,
        token: &str,
        key: &IdempotencyKey,
        computer: &str,
        request: &ConnectRequest,
    ) -> Result<ConnectionSession> {
        if ComputerId::new(computer).is_err()
            || !(1..=3600).contains(&request.lifetime_seconds)
            || request.requested_capabilities.is_empty()
            || request.requested_capabilities.len() > RuntimePermission::ALL.len()
            || !request
                .requested_capabilities
                .contains(&RuntimePermission::Connect)
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        let mut requested = request.requested_capabilities.clone();
        requested.sort_unstable();
        requested.dedup();
        if requested.len() != request.requested_capabilities.len() {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let input = digest(
            "agent-computer/connection-v1",
            &(
                computer,
                &requested,
                request.lifetime_seconds,
                crate::auth::token_id(token)?,
            ),
        )?;
        if let Some(id) =
            transactions::retry::<String>(&mut tx, &identity, CONNECT, key, &input).await?
        {
            let result = final_view(&mut tx, token, &identity, &id).await?;
            tx.commit().await?;
            return Ok(result);
        }
        require_in(
            &mut tx,
            &identity,
            &RuntimeRequirement {
                kind: RuntimeKind::Computer,
                resource_id: computer.into(),
                permission: RuntimePermission::Connect,
                runtime_seconds: None,
            },
        )
        .await?;
        let now = transactions::now(&mut tx).await?;
        let capacity = sqlx::query("SELECT count(*)::bigint AS total,count(*) FILTER(WHERE s.principal=$2)::bigint AS actor,count(*) FILTER(WHERE s.computer_id=$3)::bigint AS computer FROM connection_sessions s JOIN service_credentials c ON c.credential_id=s.credential_id JOIN principals p ON p.organization=s.organization AND p.principal=s.principal WHERE s.organization=$1 AND s.state='Active' AND s.expires_at_ms>$4 AND NOT c.revoked AND c.expires_at>clock_timestamp() AND p.enabled")
            .bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(computer).bind(now).fetch_one(&mut *tx).await?;
        for (field, limit) in [("total", 256), ("actor", 32), ("computer", 64)] {
            if capacity.try_get::<i64, _>(field)? >= limit {
                return Err(Error::RuntimeCapacityUnavailable);
            }
        }
        let credential_expiry: i64 = sqlx::query_scalar("SELECT floor(extract(epoch from expires_at)*1000)::bigint FROM service_credentials WHERE credential_id=$1")
            .bind(crate::auth::token_id(token)?).fetch_one(&mut *tx).await?;
        let expires = now
            .checked_add(i64::from(request.lifetime_seconds) * 1000)
            .ok_or(Error::CounterExhausted)?
            .min(credential_expiry);
        if expires <= now {
            return Err(Error::ConnectionInactive);
        }
        let id = random_id("conn")?;
        sqlx::query("INSERT INTO connection_sessions (organization,session_id,computer_id,principal,credential_id,requested,created_at_ms,expires_at_ms,last_seen_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$7)")
            .bind(identity.organization().as_str()).bind(&id).bind(computer).bind(identity.principal().as_str()).bind(crate::auth::token_id(token)?)
            .bind(serde_json::to_value(&requested).map_err(|_|Error::InvalidStoredData)?).bind(now).bind(expires).execute(&mut *tx).await?;
        transactions::emit(&mut tx, identity.organization().as_str(), seq, "connection.created", serde_json::json!({"session_id":id,"computer_id":computer,"principal":identity.principal().as_str(),"expires_at_ms":expires})).await?;
        transactions::save_receipt(&mut tx, &identity, CONNECT, key, &input, &id).await?;
        let result = final_view(&mut tx, token, &identity, &id).await?;
        if result.state != ConnectionState::Active {
            return Err(Error::ConnectionInactive);
        }
        tx.commit().await?;
        Ok(result)
    }

    pub async fn connection_session(&self, token: &str, id: &str) -> Result<ConnectionSession> {
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let result = final_view(&mut tx, token, &identity, id).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn heartbeat_connection_session(
        &self,
        token: &str,
        key: &IdempotencyKey,
        id: &str,
        request: &ConnectionHeartbeat,
    ) -> Result<ConnectionSession> {
        if request.expected_revision < 1 {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let input = digest(
            "agent-computer/connection-heartbeat-v1",
            &(id, request, crate::auth::token_id(token)?),
        )?;
        let current = view(&mut tx, token, &identity, id).await?;
        if transactions::retry::<String>(&mut tx, &identity, HEARTBEAT, key, &input)
            .await?
            .is_some()
        {
            let result = final_view(&mut tx, token, &identity, id).await?;
            tx.commit().await?;
            return Ok(result);
        }
        if current.state != ConnectionState::Active {
            return Err(Error::ConnectionInactive);
        }
        if current.revision != request.expected_revision {
            return Err(Error::ConnectionRevisionConflict);
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(Error::CounterExhausted)?;
        let activity =
            serde_json::to_value(request.activity).map_err(|_| Error::InvalidStoredData)?;
        let visibility =
            serde_json::to_value(request.visibility).map_err(|_| Error::InvalidStoredData)?;
        sqlx::query("UPDATE connection_sessions SET revision=$3,last_seen_at_ms=$4,activity=$5,visibility=$6 WHERE organization=$1 AND session_id=$2")
            .bind(identity.organization().as_str()).bind(id).bind(revision).bind(current.checked_at_ms).bind(activity.as_str()).bind(visibility.as_str()).execute(&mut *tx).await?;
        transactions::emit(&mut tx, identity.organization().as_str(), seq, "connection.heartbeat", serde_json::json!({"session_id":id,"revision":revision,"activity":request.activity,"visibility":request.visibility,"self_reported":true})).await?;
        transactions::save_receipt(&mut tx, &identity, HEARTBEAT, key, &input, &id).await?;
        let result = final_view(&mut tx, token, &identity, id).await?;
        if result.state != ConnectionState::Active {
            return Err(Error::ConnectionInactive);
        }
        tx.commit().await?;
        Ok(result)
    }

    /// Idempotent terminal close. This revokes future connection admission; it
    /// neither stops compute nor claims that existing writers/actions drained.
    pub async fn close_connection_session(
        &self,
        token: &str,
        id: &str,
    ) -> Result<ConnectionSession> {
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let current = view(&mut tx, token, &identity, id).await?;
        if matches!(
            current.state,
            ConnectionState::Active | ConnectionState::Expired
        ) {
            let revision = current
                .revision
                .checked_add(1)
                .ok_or(Error::CounterExhausted)?;
            let revoked = current
                .revocation_revision
                .checked_add(1)
                .ok_or(Error::CounterExhausted)?;
            sqlx::query("UPDATE connection_sessions SET revision=$3,revocation_revision=$4,state='Closed' WHERE organization=$1 AND session_id=$2")
                .bind(identity.organization().as_str()).bind(id).bind(revision).bind(revoked).execute(&mut *tx).await?;
            transactions::emit(&mut tx, identity.organization().as_str(), seq, "connection.closed", serde_json::json!({"session_id":id,"revision":revision,"revocation_revision":revoked,"process_termination_confirmed":false})).await?;
        }
        let result = final_view(&mut tx, token, &identity, id).await?;
        tx.commit().await?;
        Ok(result)
    }
}
