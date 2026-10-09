//! Trusted credential administration and request-time service authentication.
//! Service scopes cap an API operation; they never grant resource ACLs or leases.
use crate::{Error, Result, Store};
use agent_computer_core::identity::{OrganizationId, PrincipalId};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{fmt, time::Duration};
use subtle::ConstantTimeEq;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    Human,
    Agent,
}
impl PrincipalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub enum ServiceScope {
    #[serde(rename = "definitions.validate")]
    DefinitionsValidate,
    #[serde(rename = "definitions.manage")]
    DefinitionsManage,
    #[serde(rename = "runtime.connect")]
    RuntimeConnect,
    #[serde(rename = "runtime.read")]
    RuntimeRead,
    #[serde(rename = "runtime.observe")]
    RuntimeObserve,
    #[serde(rename = "runtime.app.use")]
    RuntimeAppUse,
    #[serde(rename = "runtime.activate")]
    RuntimeActivate,
    #[serde(rename = "runtime.execute")]
    RuntimeExecute,
    #[serde(rename = "runtime.modify")]
    RuntimeModify,
    #[serde(rename = "runtime.control")]
    RuntimeControl,
    #[serde(rename = "runtime.publish")]
    RuntimePublish,
    #[serde(rename = "runtime.manage")]
    RuntimeManage,
    #[serde(rename = "runtime.delete")]
    RuntimeDelete,
}
impl ServiceScope {
    pub const ALL: [Self; 13] = [
        Self::DefinitionsValidate,
        Self::DefinitionsManage,
        Self::RuntimeConnect,
        Self::RuntimeRead,
        Self::RuntimeObserve,
        Self::RuntimeAppUse,
        Self::RuntimeActivate,
        Self::RuntimeExecute,
        Self::RuntimeModify,
        Self::RuntimeControl,
        Self::RuntimePublish,
        Self::RuntimeManage,
        Self::RuntimeDelete,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DefinitionsValidate => "definitions.validate",
            Self::DefinitionsManage => "definitions.manage",
            Self::RuntimeConnect => "runtime.connect",
            Self::RuntimeRead => "runtime.read",
            Self::RuntimeObserve => "runtime.observe",
            Self::RuntimeAppUse => "runtime.app.use",
            Self::RuntimeActivate => "runtime.activate",
            Self::RuntimeExecute => "runtime.execute",
            Self::RuntimeModify => "runtime.modify",
            Self::RuntimeControl => "runtime.control",
            Self::RuntimePublish => "runtime.publish",
            Self::RuntimeManage => "runtime.manage",
            Self::RuntimeDelete => "runtime.delete",
        }
    }
}
impl std::str::FromStr for ServiceScope {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|scope| scope.as_str() == value)
            .ok_or(Error::InvalidCredentialParameters)
    }
}

pub struct IssueCredential<'a> {
    pub organization: &'a OrganizationId,
    pub principal: &'a PrincipalId,
    pub kind: PrincipalKind,
    pub scopes: &'a [ServiceScope],
    /// Service credentials expire within one day. Rotation issues a distinct key.
    pub lifetime: Duration,
}

/// Never serializes or prints the bearer value implicitly.
pub struct IssuedCredential {
    id: String,
    token: String,
}
impl IssuedCredential {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn expose_token(&self) -> &str {
        &self.token
    }
}
impl fmt::Debug for IssuedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedCredential")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Derived only from a valid database credential. Recheck for each request; this
/// value is not a reusable authorization permit for a later resource transaction.
#[derive(Debug)]
pub struct AuthenticatedPrincipal {
    organization: OrganizationId,
    principal: PrincipalId,
    kind: PrincipalKind,
}
impl AuthenticatedPrincipal {
    pub fn organization(&self) -> &OrganizationId {
        &self.organization
    }
    pub fn principal(&self) -> &PrincipalId {
        &self.principal
    }
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash(token: &str) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(b"agent-computer/service-credential-v1\0");
    digest.update(token.as_bytes());
    digest.finalize().to_vec()
}
pub(crate) fn token_id(token: &str) -> Result<&str> {
    let value = token.strip_prefix("acsk_").ok_or(Error::Unauthenticated)?;
    let (id, secret) = value.split_once('_').ok_or(Error::Unauthenticated)?;
    let lower_hex = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if id.len() != 32 || secret.len() != 64 || !lower_hex(id) || !lower_hex(secret) {
        return Err(Error::Unauthenticated);
    }
    Ok(id)
}

impl Store {
    /// A trusted worker rechecks the credential bound by a prior authenticated
    /// admission. This does not authenticate an untrusted credential-ID caller.
    pub(crate) async fn authorize_bound_runtime_in(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        org: &str,
        principal: &str,
        credential: &str,
        required: ServiceScope,
    ) -> Result<AuthenticatedPrincipal> {
        let row = sqlx::query("SELECT c.organization,c.principal,p.kind,c.scopes,(p.enabled AND NOT c.revoked AND c.expires_at>clock_timestamp()) AS active FROM service_credentials c JOIN principals p USING (organization,principal) WHERE c.credential_id=$1 AND c.organization=$2 AND c.principal=$3 FOR SHARE OF c,p")
            .bind(credential).bind(org).bind(principal).fetch_optional(&mut **tx).await?.ok_or(Error::Unauthenticated)?;
        if !row.try_get::<bool, _>("active")? {
            return Err(Error::Unauthenticated);
        }
        if !row
            .try_get::<Vec<String>, _>("scopes")?
            .iter()
            .any(|s| s == required.as_str())
        {
            return Err(Error::Forbidden);
        }
        identity_from_row(&row)
    }
    /// Local administrator operation; possession of a service token cannot call
    /// this API remotely. Existing disabled principals are never reactivated.
    pub async fn issue_credential(&self, request: IssueCredential<'_>) -> Result<IssuedCredential> {
        let seconds = request.lifetime.as_secs();
        if !(1..=86400).contains(&seconds)
            || request.lifetime.subsec_nanos() != 0
            || request.scopes.is_empty()
            || request.scopes.len() > ServiceScope::ALL.len()
        {
            return Err(Error::InvalidCredentialParameters);
        }
        let mut random = [0u8; 48];
        getrandom::fill(&mut random).map_err(|_| Error::EntropyUnavailable)?;
        let id = hex(&random[..16]);
        let token = format!("acsk_{id}_{}", hex(&random[16..]));
        let mut scopes: Vec<&str> = request.scopes.iter().map(|s| s.as_str()).collect();
        scopes.sort_unstable();
        scopes.dedup();
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, request.organization.as_str()).await?;
        sqlx::query("INSERT INTO principals (organization,principal,kind) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(request.organization.as_str()).bind(request.principal.as_str()).bind(request.kind.as_str()).execute(&mut *tx).await?;
        let principal = sqlx::query(
            "SELECT kind,enabled FROM principals WHERE organization=$1 AND principal=$2 FOR UPDATE",
        )
        .bind(request.organization.as_str())
        .bind(request.principal.as_str())
        .fetch_one(&mut *tx)
        .await?;
        if !principal.try_get::<bool, _>("enabled")?
            || principal.try_get::<String, _>("kind")? != request.kind.as_str()
        {
            return Err(Error::PrincipalConflict);
        }
        sqlx::query("INSERT INTO service_credentials (credential_id,organization,principal,secret_hash,scopes,expires_at) VALUES ($1,$2,$3,$4,$5,statement_timestamp()+make_interval(secs => $6))")
            .bind(&id).bind(request.organization.as_str()).bind(request.principal.as_str())
            .bind(hash(&token)).bind(scopes).bind(seconds as f64).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(IssuedCredential { id, token })
    }

    pub async fn authorize_service(
        &self,
        token: &str,
        required: ServiceScope,
    ) -> Result<AuthenticatedPrincipal> {
        let id = token_id(token)?;
        let row = sqlx::query("SELECT c.organization,c.principal,p.kind,c.secret_hash,c.scopes,(NOT c.revoked AND p.enabled AND c.expires_at > clock_timestamp()) AS active FROM service_credentials c JOIN principals p USING (organization,principal) WHERE c.credential_id=$1")
            .bind(id).fetch_optional(&self.pool).await?;
        decode_principal(row, token, required)
    }

    /// Caller takes the organization stream lock first. Row share locks serialize
    /// admission with credential revocation and principal disable until commit.
    pub(crate) async fn authorize_service_in(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        token: &str,
        required: ServiceScope,
    ) -> Result<AuthenticatedPrincipal> {
        let id = token_id(token)?;
        let row = sqlx::query("SELECT c.organization,c.principal,p.kind,c.secret_hash,c.scopes,(NOT c.revoked AND p.enabled AND c.expires_at > clock_timestamp()) AS active FROM service_credentials c JOIN principals p USING (organization,principal) WHERE c.credential_id=$1 FOR SHARE OF c,p")
            .bind(id).fetch_optional(&mut **tx).await?;
        decode_principal(row, token, required)
    }

    /// Revalidate the admission credential already bound by authenticated apply.
    /// Only trusted database workers use this path; no client supplies this ID.
    pub(crate) async fn authorize_operation_in(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        organization: &str,
        operation: &str,
    ) -> Result<AuthenticatedPrincipal> {
        let row = sqlx::query("SELECT c.organization,c.principal,p.kind,c.scopes,(NOT c.revoked AND p.enabled AND c.expires_at > clock_timestamp()) AS active FROM operations o JOIN service_credentials c ON c.credential_id=o.credential_id AND c.organization=o.organization AND c.principal=o.principal JOIN principals p ON p.organization=c.organization AND p.principal=c.principal WHERE o.organization=$1 AND o.operation_id=$2 FOR SHARE OF c,p")
            .bind(organization).bind(operation).fetch_optional(&mut **tx).await?.ok_or(Error::Unauthenticated)?;
        let scopes: Vec<String> = row.try_get("scopes")?;
        if !row.try_get::<bool, _>("active")? {
            return Err(Error::Unauthenticated);
        }
        if !scopes
            .iter()
            .any(|s| s == ServiceScope::DefinitionsManage.as_str())
        {
            return Err(Error::Forbidden);
        }
        identity_from_row(&row)
    }

    pub async fn revoke_credential(&self, organization: &OrganizationId, id: &str) -> Result<bool> {
        Ok(sqlx::query("UPDATE service_credentials SET revoked=TRUE WHERE organization=$1 AND credential_id=$2")
            .bind(organization.as_str()).bind(id).execute(&self.pool).await?.rows_affected() == 1)
    }

    pub async fn disable_principal(
        &self,
        organization: &OrganizationId,
        principal: &PrincipalId,
    ) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE principals SET enabled=FALSE WHERE organization=$1 AND principal=$2",
        )
        .bind(organization.as_str())
        .bind(principal.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }
}

fn decode_principal(
    row: Option<sqlx::postgres::PgRow>,
    token: &str,
    required: ServiceScope,
) -> Result<AuthenticatedPrincipal> {
    let Some(row) = row else {
        return Err(Error::Unauthenticated);
    };
    let expected: Vec<u8> = row.try_get("secret_hash")?;
    let matches: bool = hash(token).ct_eq(&expected).into();
    if !matches || !row.try_get::<bool, _>("active")? {
        return Err(Error::Unauthenticated);
    }
    let scopes: Vec<String> = row.try_get("scopes")?;
    if !scopes.iter().any(|s| s == required.as_str()) {
        return Err(Error::Forbidden);
    }
    identity_from_row(&row)
}

fn identity_from_row(row: &sqlx::postgres::PgRow) -> Result<AuthenticatedPrincipal> {
    let kind = match row.try_get::<String, _>("kind")?.as_str() {
        "human" => PrincipalKind::Human,
        "agent" => PrincipalKind::Agent,
        _ => return Err(Error::InvalidStoredData),
    };
    Ok(AuthenticatedPrincipal {
        organization: OrganizationId::new(row.try_get::<String, _>("organization")?)
            .map_err(|_| Error::InvalidStoredData)?,
        principal: PrincipalId::new(row.try_get::<String, _>("principal")?)
            .map_err(|_| Error::InvalidStoredData)?,
        kind,
    })
}
