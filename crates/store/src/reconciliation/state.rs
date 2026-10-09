use super::types::*;
use crate::{Error, Result, Store, plans};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};

pub(super) async fn check_deadline(tx: &mut Transaction<'_, Postgres>, until: i64) -> Result<()> {
    if plans::transactions::now(tx).await? >= until {
        return Err(Error::StaleReconcileLease);
    }
    Ok(())
}

pub(super) async fn authorize(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    operation: &str,
) -> Result<()> {
    let identity = Store::authorize_operation_in(tx, org, operation).await?;
    let plan_id: String = sqlx::query_scalar(
        "SELECT plan_id FROM operations WHERE organization=$1 AND operation_id=$2",
    )
    .bind(org)
    .bind(operation)
    .fetch_one(&mut **tx)
    .await?;
    let (plan, _) = plans::apply::load_plan(tx, &identity, &plan_id).await?;
    plans::access::authorize_plan(tx, &identity, &plan).await
}

pub(super) fn denial(error: &Error) -> Option<ReconcileReason> {
    match error {
        Error::Unauthenticated | Error::Forbidden => Some(ReconcileReason::AuthorizationRevoked),
        Error::ReferenceUnavailable | Error::RevisionConflict => {
            Some(ReconcileReason::ReferenceUnavailable)
        }
        _ => None,
    }
}

pub(super) fn decode(row: &PgRow) -> Result<IntentProgress> {
    let reason: Option<String> = row.try_get("reason_code")?;
    Ok(IntentProgress {
        step_id: row.try_get("step_id")?,
        resource_id: row.try_get("resource_id")?,
        revision: row.try_get("revision")?,
        state: serde_json::from_value(serde_json::Value::String(row.try_get("state")?))
            .map_err(|_| Error::InvalidStoredData)?,
        attempts: row.try_get("lease_epoch")?,
        dispatch_started: row.try_get("dispatch_started")?,
        reason: reason
            .map(|r| {
                serde_json::from_value(serde_json::Value::String(r))
                    .map_err(|_| Error::InvalidStoredData)
            })
            .transpose()?,
        available_at_ms: row.try_get("available_at_ms")?,
        event_sequence: row.try_get("event_sequence")?,
    })
}

pub(crate) async fn progress(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    operation: &str,
) -> Result<Vec<IntentProgress>> {
    sqlx::query("SELECT * FROM reconcile_intents WHERE organization=$1 AND operation_id=$2 ORDER BY ordinal")
        .bind(org).bind(operation).fetch_all(&mut **tx).await?.iter().map(decode).collect()
}

pub(super) async fn current(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    step: &str,
) -> Result<IntentProgress> {
    let row = sqlx::query("SELECT * FROM reconcile_intents WHERE organization=$1 AND step_id=$2")
        .bind(org)
        .bind(step)
        .fetch_one(&mut **tx)
        .await?;
    decode(&row)
}

pub(super) async fn lease_row(
    tx: &mut Transaction<'_, Postgres>,
    lease: &ReconcileLease,
) -> Result<PgRow> {
    let row=sqlx::query("SELECT *,lease_until_ms>floor(extract(epoch FROM clock_timestamp())*1000)::bigint AS live FROM reconcile_intents WHERE organization=$1 AND step_id=$2")
        .bind(&lease.task.organization).bind(&lease.task.step_id).fetch_optional(&mut **tx).await?.ok_or(Error::StaleReconcileLease)?;
    if row.try_get::<String, _>("state")? != "Running"
        || row.try_get::<i64, _>("lease_epoch")? != lease.epoch
        || row.try_get::<Option<String>, _>("lease_owner")?.as_deref() != Some(lease.owner.as_str())
        || !row.try_get::<Option<bool>, _>("live")?.unwrap_or(false)
    {
        return Err(Error::StaleReconcileLease);
    }
    Ok(row)
}

pub(super) async fn operation_state(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    operation: &str,
) -> Result<()> {
    sqlx::query("UPDATE operations SET state=(SELECT CASE WHEN bool_and(state='Succeeded') THEN 'Succeeded' WHEN bool_or(state='Failed') THEN 'Failed' WHEN bool_or(state='Blocked') THEN 'Blocked' WHEN bool_or(lease_epoch>0) THEN 'Running' ELSE 'Queued' END FROM reconcile_intents WHERE organization=$1 AND operation_id=$2) WHERE organization=$1 AND operation_id=$2")
        .bind(org).bind(operation).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn blocked(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    operation: &str,
    step: &str,
    seq: i64,
    reason: ReconcileReason,
) -> Result<IntentProgress> {
    let event = plans::transactions::emit(
        tx,
        org,
        seq,
        "reconciliation.blocked",
        serde_json::json!({"operation_id":operation,"step_id":step,"reason":reason}),
    )
    .await?;
    sqlx::query("UPDATE reconcile_intents SET state='Blocked',reason_code=$3,lease_owner=NULL,lease_until_ms=NULL,event_sequence=$4 WHERE organization=$1 AND step_id=$2")
        .bind(org).bind(step).bind(reason.as_str()).bind(event).execute(&mut **tx).await?;
    operation_state(tx, org, operation).await?;
    current(tx, org, step).await
}
