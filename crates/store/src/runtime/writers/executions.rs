//! Durable bounded execution admission and dispatch journal.
mod dispatch;
use super::*;
use crate::plans::DefinitionKind;
pub use dispatch::{
    ExecutionCompletion, ExecutionDispatchAttempt, ExecutionDispatchIntent, ExecutionOutput,
    ExecutionOutputDownload, ExecutionPodAttempt, ExecutionPodPlan, ExecutionRenewalAttempt,
    ExecutionRenewalGrant, ExecutionRuntimeInputs, ExecutionStartupAttempt, ExecutionStartupGrant,
    ExecutionWatchdogArm, OutputState, OutputStream, QueuedDispatch,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCommand {
    pub argv: Vec<String>,
    pub cwd: String,
    pub timeout_seconds: u32,
    pub term_grace_ms: u32,
    pub output_limit_bytes: usize,
}
impl ExecutionCommand {
    fn validate(&self) -> Result<()> {
        // Share the actual supervisor's command constraints; no process is run.
        agent_computer_sandbox::Request {
            execution_id: "validation".into(),
            generation: 1,
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            timeout_seconds: self.timeout_seconds,
            lease_budget_ms: 1,
            term_grace_ms: self.term_grace_ms,
            output_limit_bytes: self.output_limit_bytes,
        }
        .validate()
        .map_err(|_| Error::InvalidRuntimeRequest)
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLifetime {
    #[default]
    Background,
    Connection,
}

fn historical_lifetime() -> ExecutionLifetime {
    ExecutionLifetime::Connection
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitExecution {
    pub lease_id: String,
    pub lease: WriterLeaseCommand,
    pub sandbox_id: String,
    #[serde(default)]
    pub lifetime: ExecutionLifetime,
    /// New admissions default to worker renewal. Omission remains absent from
    /// canonical input bytes, so retries of historical fixed records still match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renewable: Option<bool>,
    pub command: ExecutionCommand,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelExecution {
    pub expected_revision: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExecutionState {
    Queued,
    Cancelled,
    Dispatching,
    CancelRequested,
    Unknown,
    Succeeded,
    Failed,
}

/// Metadata only. Command bytes and storage paths are not exposed in this view.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionRequest {
    pub execution_id: String,
    #[serde(default = "historical_lifetime")]
    pub lifetime: ExecutionLifetime,
    #[serde(default)]
    pub renewable: bool,
    pub computer_id: String,
    pub generation: i64,
    pub candidate_id: String,
    pub sandbox_id: String,
    pub sandbox_revision: i64,
    pub lease_id: String,
    pub epoch: i64,
    pub revision: i64,
    pub state: ExecutionState,
    pub reason: String,
    pub input_digest: String,
    pub binding_digest: String,
    pub created_at_ms: i64,
    pub queue_deadline_at_ms: i64,
    pub dispatch_started: bool,
}

pub(super) async fn reserved(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    lease: &str,
    epoch: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_requests WHERE organization=$1 AND lease_id=$2 AND epoch=$3 AND state='Queued')")
        .bind(org).bind(lease).bind(epoch).fetch_one(&mut **tx).await?)
}
pub(super) async fn cancel_reserved(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    lease: &str,
    epoch: i64,
    reason: &str,
    seq: i64,
) -> Result<i64> {
    let id:Option<String>=sqlx::query_scalar("UPDATE execution_requests SET state='Cancelled',revision=revision+1,reason=$4 WHERE organization=$1 AND lease_id=$2 AND epoch=$3 AND state='Queued' RETURNING execution_id")
        .bind(org).bind(lease).bind(epoch).bind(reason).fetch_optional(&mut **tx).await?;
    if let Some(id) = id {
        transactions::emit(tx,org,seq,"execution.status_changed",serde_json::json!({"execution_id":id,"state":"Cancelled","reason":reason,"dispatch_started":false})).await
    } else {
        Ok(seq)
    }
}

async fn row(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<PgRow> {
    valid_id(id)?;
    sqlx::query("SELECT * FROM execution_requests WHERE organization=$1 AND execution_id=$2")
        .bind(org)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::RuntimeAccessUnavailable)
}
fn view(row: &PgRow) -> Result<ExecutionRequest> {
    let binding: serde_json::Value = row.try_get("binding")?;
    if digest("agent-computer/execution-binding-v1", &binding)?
        != row.try_get::<String, _>("binding_digest")?
    {
        return Err(Error::InvalidStoredData);
    }
    let string = |field: &str| {
        binding[field]
            .as_str()
            .map(String::from)
            .ok_or(Error::InvalidStoredData)
    };
    Ok(ExecutionRequest {
        execution_id: row.try_get("execution_id")?,
        lifetime: serde_json::from_value::<SubmitExecution>(row.try_get("input")?)
            .map_err(|_| Error::InvalidStoredData)?
            .lifetime,
        renewable: binding.get("execution_lease").is_some(),
        computer_id: string("computer_id")?,
        generation: binding["generation"]
            .as_i64()
            .ok_or(Error::InvalidStoredData)?,
        candidate_id: string("candidate_id")?,
        sandbox_id: string("sandbox_id")?,
        sandbox_revision: binding["sandbox_revision"]
            .as_i64()
            .ok_or(Error::InvalidStoredData)?,
        lease_id: row.try_get("lease_id")?,
        epoch: row.try_get("epoch")?,
        revision: row.try_get("revision")?,
        state: serde_json::from_value(row.try_get::<String, _>("state")?.into())
            .map_err(|_| Error::InvalidStoredData)?,
        reason: row.try_get("reason")?,
        input_digest: row.try_get("input_digest")?,
        binding_digest: row.try_get("binding_digest")?,
        created_at_ms: row.try_get("created_at_ms")?,
        queue_deadline_at_ms: row.try_get("queue_deadline_at_ms")?,
        dispatch_started: row.try_get::<String, _>("reason")? == "completed_cancelled"
            || matches!(
                row.try_get::<String, _>("state")?.as_str(),
                "Dispatching" | "CancelRequested" | "Unknown" | "Succeeded" | "Failed"
            ),
    })
}
async fn own_execution(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    identity: &AuthenticatedPrincipal,
    id: &str,
) -> Result<PgRow> {
    let row = row(tx, identity.organization().as_str(), id).await?;
    authority::owner(
        tx,
        token,
        identity,
        &row.try_get::<String, _>("session_id")?,
    )
    .await?;
    Ok(row)
}
async fn reconcile(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    row: &PgRow,
    seq: i64,
) -> Result<()> {
    let state: String = row.try_get("state")?;
    if matches!(
        state.as_str(),
        "Cancelled" | "Unknown" | "Succeeded" | "Failed"
    ) {
        return Ok(());
    }
    let lease: String = row.try_get("lease_id")?;
    let epoch = row.try_get("epoch")?;
    let owner = authority::row(tx, org, &lease).await?;
    let now = transactions::now(tx).await?;
    let effective_deadline = if state == "Queued" {
        row.try_get("queue_deadline_at_ms")?
    } else {
        sqlx::query_scalar::<_, Option<i64>>("SELECT execution_effective_deadline($1,$2)")
            .bind(org)
            .bind(row.try_get::<String, _>("execution_id")?)
            .fetch_one(&mut **tx)
            .await?
            .ok_or(Error::InvalidStoredData)?
    };
    let authorized = if state == "CancelRequested" {
        authority::cancellation_active(tx, org, &owner).await?
    } else {
        authority::active(tx, org, &owner).await?
    };
    if owner.try_get::<i64, _>("epoch")? != epoch
        || !(owner.try_get::<String, _>("state")? == "Held"
            || (state == "CancelRequested" && owner.try_get::<String, _>("state")? == "Draining"))
        || now >= effective_deadline
        || now >= owner.try_get::<i64, _>("expires_at_ms")?
        || !authorized
    {
        if state == "Queued" {
            cancel_reserved(tx, org, &lease, epoch, "writer_unavailable", seq).await?;
        } else {
            dispatch::uncertain(tx, org, row, "writer_unavailable", seq).await?;
        }
    }
    Ok(())
}

impl Store {
    pub async fn submit_candidate_execution(
        &self,
        token: &str,
        key: &IdempotencyKey,
        computer: &str,
        input: &SubmitExecution,
    ) -> Result<ExecutionRequest> {
        for id in [computer, &input.lease_id, &input.sandbox_id] {
            valid_id(id)?;
        }
        input.command.validate()?;
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let org = identity.organization().as_str();
        let hash = digest(
            "agent-computer/execution-submit-v1",
            &(computer, input, crate::auth::token_id(token)?),
        )?;
        let op = "runtime.execution-submit.v1";
        if let Some(id) = transactions::retry::<String>(&mut tx, &identity, op, key, &hash).await? {
            let record = own_execution(&mut tx, token, &identity, &id).await?;
            reconcile(&mut tx, org, &record, seq).await?;
            let result = view(&row(&mut tx, org, &id).await?)?;
            Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeConnect).await?;
            tx.commit().await?;
            return Ok(result);
        }
        let (owner, _, prepared) = live(&mut tx, token, &identity, &input.lease_id).await?;
        authority::command(&owner, &input.lease)?;
        if owner.try_get::<String, _>("computer_id")? != computer {
            return Err(Error::RuntimeAccessUnavailable);
        }
        if owner.try_get::<bool, _>("dispatched")? {
            return Err(Error::DispatchAlreadyStarted);
        }
        let used: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_requests WHERE organization=$1 AND lease_id=$2 AND epoch=$3)").bind(org).bind(&input.lease_id).bind(input.lease.epoch).fetch_one(&mut *tx).await?;
        if used {
            return Err(Error::WriterLeaseBusy);
        }
        let request: String = owner.try_get("request_id")?;
        let start = sqlx::query("SELECT r.snapshot,r.max_runtime_seconds,i.revision,p.binding FROM runtime_start_requests r JOIN runtime_start_inputs i USING(organization,request_id) JOIN candidate_preparations p USING(organization,request_id) WHERE r.organization=$1 AND r.request_id=$2")
            .bind(org).bind(&request).fetch_one(&mut *tx).await?;
        if input.command.timeout_seconds > start.try_get::<i32, _>("max_runtime_seconds")? as u32 {
            return Err(Error::RuntimeBudgetExceeded);
        }
        let snapshot: super::super::start::graph::Snapshot =
            serde_json::from_value(start.try_get("snapshot")?)
                .map_err(|_| Error::InvalidStoredData)?;
        let root = snapshot.resource(DefinitionKind::Computer, computer)?;
        if !root.spec["sandboxRefs"]
            .as_array()
            .is_some_and(|v| v.contains(&serde_json::json!(format!("id:{}", input.sandbox_id))))
        {
            return Err(Error::ReferenceUnavailable);
        }
        let sandbox = snapshot.resource(DefinitionKind::Sandbox, &input.sandbox_id)?;
        if sandbox.spec["runtimeClass"] != "gvisor"
            || snapshot.resources.iter().any(|r| {
                r.reference.kind == DefinitionKind::App
                    && r.spec["sandboxRef"] == format!("id:{}", input.sandbox_id)
            })
        {
            return Err(Error::ReferenceUnavailable);
        }
        let mut binding = serde_json::json!({"computer_id":computer,"generation":input.lease.generation,"candidate_id":owner.try_get::<String,_>("candidate_id")?,"sandbox_id":input.sandbox_id,"sandbox_revision":sandbox.reference.revision,"sandbox":sandbox,"prepared":prepared,"storage_target":start.try_get::<serde_json::Value,_>("binding")?,"input_revision":start.try_get::<i64,_>("revision")?});
        if input.renewable.unwrap_or(true) {
            let max_budget_ms = (u64::from(input.command.timeout_seconds) * 1000 + 30_000)
                .min(start.try_get::<i32, _>("max_runtime_seconds")? as u64 * 1000)
                .min(u64::from(
                    agent_computer_sandbox::renewal::MAX_EXECUTION_BUDGET_MS,
                ));
            binding["execution_lease"] =
                serde_json::json!({"version":1,"max_budget_ms":max_budget_ms});
        }
        let binding_hash = digest("agent-computer/execution-binding-v1", &binding)?;
        let id = random_id("exec")?;
        let now = transactions::now(&mut tx).await?;
        let until: i64 = owner.try_get("expires_at_ms")?;
        sqlx::query("INSERT INTO execution_requests (organization,execution_id,lease_id,epoch,session_id,input,binding,input_digest,binding_digest,created_at_ms,queue_deadline_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(org).bind(&id).bind(&input.lease_id).bind(input.lease.epoch).bind(&input.lease.connection_session_id)
            .bind(serde_json::to_value(input).map_err(|_|Error::InvalidRuntimeRequest)?).bind(binding).bind(&hash).bind(&binding_hash).bind(now).bind(until).execute(&mut *tx).await?;
        sqlx::query("UPDATE candidate_writer_leases SET revision=revision+1 WHERE organization=$1 AND lease_id=$2").bind(org).bind(&input.lease_id).execute(&mut *tx).await?;
        transactions::emit(&mut tx,org,seq,"execution.status_changed",serde_json::json!({"execution_id":id,"computer_id":computer,"generation":input.lease.generation,"state":"Queued","lifetime":input.lifetime,"dispatch_started":false,"input_digest":hash,"binding_digest":binding_hash})).await?;
        transactions::save_receipt(&mut tx, &identity, op, key, &hash, &id).await?;
        live(&mut tx, token, &identity, &input.lease_id).await?;
        if transactions::now(&mut tx).await? >= until {
            return Err(Error::WriterLeaseInactive);
        }
        let result = view(&row(&mut tx, org, &id).await?)?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn candidate_execution(&self, token: &str, id: &str) -> Result<ExecutionRequest> {
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let record = own_execution(&mut tx, token, &identity, id).await?;
        reconcile(&mut tx, identity.organization().as_str(), &record, seq).await?;
        let result = view(&row(&mut tx, identity.organization().as_str(), id).await?)?;
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeConnect).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn cancel_candidate_execution(
        &self,
        token: &str,
        key: &IdempotencyKey,
        id: &str,
        input: &CancelExecution,
    ) -> Result<ExecutionRequest> {
        if input.expected_revision < 1 {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let org = identity.organization().as_str();
        let record = own_execution(&mut tx, token, &identity, id).await?;
        let hash = digest(
            "agent-computer/execution-cancel-v1",
            &(id, input, crate::auth::token_id(token)?),
        )?;
        let op = "runtime.execution-cancel.v1";
        if transactions::retry::<String>(&mut tx, &identity, op, key, &hash)
            .await?
            .is_none()
        {
            if record.try_get::<i64, _>("revision")? != input.expected_revision {
                return Err(Error::RuntimeConflict);
            }
            match record.try_get::<String, _>("state")?.as_str() {
                "Queued" => {
                    cancel_reserved(
                        &mut tx,
                        org,
                        &record.try_get::<String, _>("lease_id")?,
                        record.try_get("epoch")?,
                        "user_requested",
                        seq,
                    )
                    .await?;
                }
                "Dispatching" => {
                    sqlx::query("UPDATE execution_requests SET state='CancelRequested',reason='user_requested',revision=revision+1 WHERE organization=$1 AND execution_id=$2")
                        .bind(org).bind(id).execute(&mut *tx).await?;
                    let seq = transactions::emit(&mut tx, org, seq, "execution.status_changed", serde_json::json!({"execution_id":id,"state":"CancelRequested","reason":"user_requested","dispatch_started":true})).await?;
                    dispatch::stop_writer(&mut tx, org, &record, seq).await?;
                }
                // CancelRequested/Unknown already require external stopping;
                // recording a retry receipt does not claim it has happened.
                "Cancelled" | "CancelRequested" | "Unknown" | "Succeeded" | "Failed" => {}
                _ => return Err(Error::InvalidStoredData),
            }
            transactions::save_receipt(&mut tx, &identity, op, key, &hash, &id).await?;
        }
        let result = view(&row(&mut tx, org, id).await?)?;
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeConnect).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Trusted maintenance can lower authority even after credential revocation.
    pub async fn reconcile_candidate_execution(
        &self,
        org: &OrganizationId,
        id: &str,
    ) -> Result<ExecutionRequest> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        reconcile(&mut tx, org.as_str(), &record, seq).await?;
        let result = view(&row(&mut tx, org.as_str(), id).await?)?;
        tx.commit().await?;
        Ok(result)
    }
}
