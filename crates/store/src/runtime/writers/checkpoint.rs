//! Lower writer authority for an already authorized, durable normal stop.
use super::*;

pub(in crate::runtime) async fn request_checkpoint_drain(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    request: &str,
    mut seq: i64,
) -> Result<i64> {
    let pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_start_requests r JOIN artifact_commits a USING(organization,request_id) WHERE r.organization=$1 AND r.request_id=$2 AND r.state='Draining' AND a.state='Draining' AND a.cancel_running AND a.stop_after_commit)")
        .bind(org).bind(request).fetch_one(&mut **tx).await?;
    if !pending {
        return Err(Error::RuntimeConflict);
    }
    let executions = sqlx::query("SELECT e.execution_id,e.lease_id,e.epoch,e.state FROM execution_requests e JOIN candidate_writer_leases l USING(organization,lease_id) WHERE l.organization=$1 AND l.request_id=$2 AND e.state IN ('Queued','Dispatching') ORDER BY e.execution_id")
        .bind(org).bind(request).fetch_all(&mut **tx).await?;
    for execution in executions {
        if execution.try_get::<String, _>("state")? == "Queued" {
            seq = executions::cancel_reserved(
                tx,
                org,
                &execution.try_get::<String, _>("lease_id")?,
                execution.try_get("epoch")?,
                "user_requested",
                seq,
            )
            .await?;
        } else {
            let id: String = execution.try_get("execution_id")?;
            sqlx::query("UPDATE execution_requests SET state='CancelRequested',reason='user_requested',revision=revision+1 WHERE organization=$1 AND execution_id=$2")
                .bind(org).bind(&id).execute(&mut **tx).await?;
            seq = transactions::emit(tx, org, seq, "execution.status_changed", serde_json::json!({"execution_id":id,"state":"CancelRequested","reason":"user_requested","trigger":"checkpoint_stop","dispatch_started":true})).await?;
        }
    }
    let leases: Vec<String> = sqlx::query_scalar("SELECT lease_id FROM candidate_writer_leases WHERE organization=$1 AND request_id=$2 AND state<>'Released' ORDER BY lease_id")
        .bind(org).bind(request).fetch_all(&mut **tx).await?;
    for lease in leases {
        // Reuse only existing proofs. Expiry, a cancellation request or a Pod
        // deletion cannot manufacture a writer drain or resolve Unknown.
        drain(tx, org, &lease, seq).await?;
        seq = Store::lock_stream(tx, org).await?;
    }
    Ok(seq)
}
