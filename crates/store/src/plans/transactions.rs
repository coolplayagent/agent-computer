use crate::{
    Error, Result, Store,
    auth::{AuthenticatedPrincipal, ServiceScope},
};
use agent_computer_core::identity::IdempotencyKey;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};

pub(super) async fn begin<'a>(
    store: &'a Store,
    token: &str,
) -> Result<(Transaction<'a, Postgres>, AuthenticatedPrincipal, i64)> {
    let initial = store
        .authorize_service(token, ServiceScope::DefinitionsManage)
        .await?;
    let mut tx = store.pool.begin().await?;
    let seq = Store::lock_stream(&mut tx, initial.organization().as_str()).await?;
    let identity =
        Store::authorize_service_in(&mut tx, token, ServiceScope::DefinitionsManage).await?;
    Ok((tx, identity, seq))
}
pub(crate) async fn retry<T: DeserializeOwned>(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    operation: &str,
    key: &IdempotencyKey,
    input: &str,
) -> Result<Option<T>> {
    let row=sqlx::query("SELECT input_digest,response,retired FROM request_records WHERE organization=$1 AND principal=$2 AND operation=$3 AND request_key=$4").bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(operation).bind(key.as_str()).fetch_optional(&mut **tx).await?;
    let Some(row) = row else { return Ok(None) };
    if row.try_get::<bool, _>("retired")? {
        return Err(Error::IdempotencyGone);
    }
    if row.try_get::<Vec<u8>, _>("input_digest")? != input_hash(input) {
        return Err(Error::IdempotencyConflict);
    }
    serde_json::from_value(row.try_get("response")?)
        .map(Some)
        .map_err(|_| Error::InvalidStoredData)
}
fn input_hash(input: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(input).to_vec()
}
pub(crate) async fn save_receipt(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    operation: &str,
    key: &IdempotencyKey,
    input: &str,
    response: &impl Serialize,
) -> Result<()> {
    sqlx::query("INSERT INTO request_records (organization,principal,operation,request_key,input_digest,response) VALUES ($1,$2,$3,$4,$5,$6)").bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(operation).bind(key.as_str()).bind(input_hash(input)).bind(serde_json::to_value(response).map_err(|_|Error::InvalidStoredData)?).execute(&mut **tx).await?;
    Ok(())
}
pub(crate) async fn emit(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    previous: i64,
    kind: &str,
    payload: Value,
) -> Result<i64> {
    let sequence = previous.checked_add(1).ok_or(Error::CounterExhausted)?;
    sqlx::query("UPDATE organization_streams SET last_sequence=$2 WHERE organization=$1")
        .bind(org)
        .bind(sequence)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO events (organization,sequence,kind,payload) VALUES ($1,$2,$3,$4)")
        .bind(org)
        .bind(sequence)
        .bind(kind)
        .bind(payload)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO outbox (organization,sequence) VALUES ($1,$2)")
        .bind(org)
        .bind(sequence)
        .execute(&mut **tx)
        .await?;
    Ok(sequence)
}
pub(crate) async fn now(tx: &mut Transaction<'_, Postgres>) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint")
            .fetch_one(&mut **tx)
            .await?,
    )
}
