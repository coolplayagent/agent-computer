//! Authenticated GUI admission and controller-owned serial dispatch. History is
//! never a replacement for the original attach session or live node guard.
mod api;
mod checks;
mod types;
mod worker;
use super::*;
use agent_computer_browser::{ActionIntent, ActionReceipt, Callback, Decision, Outcome};
use checks::*;
use types::Caller;
pub use types::{
    AcquireControl, BrowserCommand, BrowserCommandView, BrowserControl, BrowserDrain, BrowserWork,
    ReleaseControl, SubmitBrowserCommand,
};

async fn control(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<Option<PgRow>> {
    Ok(
        sqlx::query("SELECT * FROM browser_controls WHERE organization=$1 AND instance_id=$2")
            .bind(org)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?,
    )
}
fn control_view(r: &PgRow) -> Result<BrowserControl> {
    Ok(BrowserControl {
        instance_id: r.try_get("instance_id")?,
        control_epoch: r.try_get("epoch")?,
        state: r.try_get("state")?,
        owner_principal: r.try_get("principal")?,
        owner_connection: r.try_get("session_id")?,
        expires_at_ms: r.try_get("expires_at_ms")?,
    })
}
fn caller(r: &PgRow) -> Result<Caller> {
    Ok(Caller {
        principal: r.try_get("principal")?,
        credential: r.try_get("credential_id")?,
        session: r.try_get("session_id")?,
    })
}
fn authenticated(identity: &AuthenticatedPrincipal, token: &str, session: &str) -> Result<Caller> {
    valid(session)?;
    Ok(Caller {
        principal: identity.principal().as_str().into(),
        credential: crate::auth::token_id(token)?.into(),
        session: session.into(),
    })
}
async fn request(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<PgRow> {
    valid(id)?;
    sqlx::query("SELECT * FROM browser_requests WHERE organization=$1 AND request_id=$2")
        .bind(org)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::RuntimeAccessUnavailable)
}
fn command(r: &PgRow) -> Result<BrowserCommand> {
    serde_json::from_value(r.try_get("command")?).map_err(|_| Error::InvalidStoredData)
}
fn request_view(r: &PgRow) -> Result<BrowserCommandView> {
    Ok(BrowserCommandView {
        request_id: r.try_get("request_id")?,
        instance_id: r.try_get("instance_id")?,
        control_epoch: r.try_get("epoch")?,
        state: r.try_get("state")?,
        result: r.try_get("result")?,
        result_valid_until_ms: r.try_get("result_valid_until_ms")?,
        created_at_ms: r.try_get("created_at_ms")?,
        expires_at_ms: r.try_get("expires_at_ms")?,
    })
}
async fn epoch(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<i64> {
    Ok(control(tx, org, id)
        .await?
        .map(|r| r.try_get("epoch"))
        .transpose()?
        .unwrap_or(0))
}
