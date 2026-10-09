//! Durable start admission only. There is deliberately no external-effect dispatcher
//! here: input publication, storage preparation, leases and fencing remain required.
pub(super) mod graph;
mod types;
use super::*;
use crate::plans::types::{digest, random_id};
use agent_computer_core::identity::{ComputerId, IdempotencyKey};
use serde::{Deserialize, Serialize};
pub use types::*;

const START: &str = "runtime.start.v1";
const CANCEL: &str = "runtime.cancel-queued-start.v1";
const CANDIDATE_BYTES: i64 = 10 * 1024 * 1024 * 1024;

fn requirement(
    id: &str,
    permission: RuntimePermission,
    seconds: Option<u32>,
) -> RuntimeRequirement {
    RuntimeRequirement {
        kind: RuntimeKind::Computer,
        resource_id: id.into(),
        permission,
        runtime_seconds: seconds,
    }
}

async fn capacity(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    principal: &str,
    admission: &graph::Admission,
    seconds: u32,
) -> Result<()> {
    // Conservative platform ceilings, not caller-supplied capacity. All admissions
    // and releases hold the organization lock; Queued and Preparing both reserve.
    let row = sqlx::query("SELECT count(*)::bigint AS total, count(*) FILTER (WHERE principal=$2)::bigint AS actor, COALESCE(sum(cpu_millis),0)::bigint AS cpu, COALESCE(sum(memory_mib),0)::bigint AS memory, COALESCE(sum(storage_bytes),0)::bigint AS storage, COALESCE(sum(storage_bytes) FILTER (WHERE volume_id=$3),0)::bigint AS volume, COALESCE(sum(max_runtime_seconds) FILTER (WHERE principal=$2),0)::bigint AS seconds, count(*) FILTER (WHERE workspace_id=$4)::bigint AS workspace FROM runtime_start_requests WHERE organization=$1 AND state <> 'Cancelled'")
        .bind(org).bind(principal).bind(&admission.volume).bind(&admission.workspace).fetch_one(&mut **tx).await?;
    for (field, added, limit) in [
        ("total", 1, 64),
        ("actor", 1, 8),
        ("cpu", admission.cpu, 64_000),
        ("memory", admission.memory, 131_072),
        ("storage", CANDIDATE_BYTES, 1024 * 1024 * 1024 * 1024),
        ("volume", CANDIDATE_BYTES, admission.volume_quota),
        ("seconds", i64::from(seconds), 86_400),
        ("workspace", 1, 1),
    ] {
        if row
            .try_get::<i64, _>(field)?
            .checked_add(added)
            .is_none_or(|n| n > limit)
        {
            return Err(Error::RuntimeCapacityUnavailable);
        }
    }
    Ok(())
}

async fn state(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    computer: &str,
) -> Result<ComputerRuntime> {
    let row = sqlx::query("SELECT c.revision,c.generation,c.active_request,r.state FROM runtime_controls c LEFT JOIN runtime_start_requests r ON r.organization=c.organization AND r.request_id=c.active_request WHERE c.organization=$1 AND c.computer_id=$2")
        .bind(org).bind(computer).fetch_optional(&mut **tx).await?;
    let mut result = ComputerRuntime {
        computer_id: computer.into(),
        revision: 1,
        generation: 0,
        active_request: None,
        start_state: None,
        ready: false,
    };
    if let Some(row) = row {
        result.revision = row.try_get("revision")?;
        result.generation = row.try_get("generation")?;
        result.active_request = row.try_get("active_request")?;
        result.start_state = row
            .try_get::<Option<String>, _>("state")?
            .map(|s| {
                serde_json::from_value(serde_json::Value::String(s))
                    .map_err(|_| Error::InvalidStoredData)
            })
            .transpose()?;
    }
    Ok(result)
}

impl Store {
    pub async fn computer_runtime(&self, token: &str, computer: &str) -> Result<ComputerRuntime> {
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        authorize_in(
            &mut tx,
            token,
            &[requirement(computer, RuntimePermission::Read, None)],
        )
        .await?;
        let result = state(&mut tx, identity.organization().as_str(), computer).await?;
        Store::authorize_service_in(&mut tx, token, ServiceScope::RuntimeRead).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn admit_computer_start(
        &self,
        token: &str,
        key: &IdempotencyKey,
        computer: &str,
        request: &StartRequest,
    ) -> Result<StartReceipt> {
        if request.expected_revision < 1
            || request.expected_spec_revision < 1
            || !(1..=86400).contains(&request.max_runtime_seconds)
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeActivate).await?;
        authorize_in(
            &mut tx,
            token,
            &[requirement(
                computer,
                RuntimePermission::Activate,
                Some(request.max_runtime_seconds),
            )],
        )
        .await?;
        let org = identity.organization().as_str();
        let input = digest("agent-computer/start-input-v1", &(computer, request))?;
        if let Some(receipt) =
            transactions::retry::<StartReceipt>(&mut tx, &identity, START, key, &input).await?
        {
            graph::reauthorize(&mut tx, token, org, &receipt.request_id).await?;
            Store::authorize_service_in(&mut tx, token, ServiceScope::RuntimeActivate).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        let current = state(&mut tx, org, computer).await?;
        if current.revision != request.expected_revision || current.active_request.is_some() {
            return Err(Error::RuntimeConflict);
        }
        let admission = graph::capture(&mut tx, org, computer, request).await?;
        authorize_in(&mut tx, token, &admission.snapshot.requirements).await?;
        let input_revision = super::inputs::current(&mut tx, org, &admission.workspace).await?;
        let input_digest: String = sqlx::query_scalar("SELECT digest FROM workspace_input_versions WHERE organization=$1 AND workspace_id=$2 AND revision=$3")
            .bind(org).bind(&admission.workspace).bind(input_revision).fetch_one(&mut *tx).await?;
        capacity(
            &mut tx,
            org,
            identity.principal().as_str(),
            &admission,
            request.max_runtime_seconds,
        )
        .await?;
        let receipt = StartReceipt {
            request_id: random_id("start")?,
            computer_id: computer.into(),
            control_revision: current
                .revision
                .checked_add(1)
                .ok_or(Error::CounterExhausted)?,
            spec_revision: request.expected_spec_revision,
            generation: current
                .generation
                .checked_add(1)
                .ok_or(Error::CounterExhausted)?,
            candidate_id: random_id("candidate")?,
            snapshot_digest: digest("agent-computer/start-snapshot-v1", &admission.snapshot)?,
            input_revision: Some(input_revision),
            input_manifest_digest: Some(input_digest),
            state: StartState::Queued,
            reason: "awaiting_runtime_preparation".into(),
            cpu_millis: admission.cpu,
            memory_mib: admission.memory,
            storage_bytes: CANDIDATE_BYTES,
            max_runtime_seconds: request.max_runtime_seconds,
            queue_deadline_at_ms: transactions::now(&mut tx)
                .await?
                .checked_add(900_000)
                .ok_or(Error::CounterExhausted)?,
            event_sequence: seq.checked_add(1).ok_or(Error::CounterExhausted)?,
        };
        sqlx::query("INSERT INTO runtime_controls (organization,computer_id) VALUES ($1,$2) ON CONFLICT DO NOTHING")
            .bind(org).bind(computer).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO runtime_start_requests (organization,request_id,computer_id,principal,credential_id,generation,candidate_id,workspace_id,volume_id,cpu_millis,memory_mib,storage_bytes,max_runtime_seconds,queue_deadline_at_ms,snapshot,snapshot_digest,receipt) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)")
            .bind(org).bind(&receipt.request_id).bind(computer).bind(identity.principal().as_str()).bind(crate::auth::token_id(token)?)
            .bind(receipt.generation).bind(&receipt.candidate_id).bind(&admission.workspace).bind(&admission.volume)
            .bind(admission.cpu).bind(admission.memory).bind(CANDIDATE_BYTES).bind(request.max_runtime_seconds as i32).bind(receipt.queue_deadline_at_ms)
            .bind(serde_json::to_value(&admission.snapshot).map_err(|_|Error::InvalidStoredData)?).bind(&receipt.snapshot_digest)
            .bind(serde_json::to_value(&receipt).map_err(|_|Error::InvalidStoredData)?).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO runtime_start_inputs (organization,request_id,workspace_id,revision) VALUES ($1,$2,$3,$4)")
            .bind(org).bind(&receipt.request_id).bind(&admission.workspace).bind(input_revision).execute(&mut *tx).await?;
        sqlx::query("UPDATE runtime_controls SET revision=$3,generation=$4,active_request=$5 WHERE organization=$1 AND computer_id=$2")
            .bind(org).bind(computer).bind(receipt.control_revision).bind(receipt.generation).bind(&receipt.request_id).execute(&mut *tx).await?;
        transactions::emit(&mut tx, org, seq, "computer.start_queued", serde_json::json!({"computer_id":computer,"request_id":receipt.request_id,"generation":receipt.generation,"revision":receipt.control_revision,"ready":false})).await?;
        transactions::save_receipt(&mut tx, &identity, START, key, &input, &receipt).await?;
        Store::authorize_service_in(&mut tx, token, ServiceScope::RuntimeActivate).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    /// Releases only an undispatched admission. This is not runtime stop/recover.
    pub async fn cancel_queued_computer_start(
        &self,
        token: &str,
        key: &IdempotencyKey,
        computer: &str,
        request: &CancelQueuedStart,
    ) -> Result<ComputerRuntime> {
        if request.expected_revision < 1 || ComputerId::new(&request.request_id).is_err() {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeManage).await?;
        authorize_in(
            &mut tx,
            token,
            &[requirement(computer, RuntimePermission::Manage, None)],
        )
        .await?;
        let org = identity.organization().as_str();
        let input = digest("agent-computer/cancel-start-input-v1", &(computer, request))?;
        if let Some(receipt) =
            transactions::retry::<ComputerRuntime>(&mut tx, &identity, CANCEL, key, &input).await?
        {
            Store::authorize_service_in(&mut tx, token, ServiceScope::RuntimeManage).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        let current = state(&mut tx, org, computer).await?;
        if current.revision != request.expected_revision
            || current.active_request.as_deref() != Some(&request.request_id)
            || current.start_state != Some(StartState::Queued)
        {
            return Err(Error::RuntimeConflict);
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(Error::CounterExhausted)?;
        let changed = sqlx::query("UPDATE runtime_start_requests SET state='Cancelled' WHERE organization=$1 AND computer_id=$2 AND request_id=$3 AND state='Queued'")
            .bind(org).bind(computer).bind(&request.request_id).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(Error::RuntimeConflict);
        }
        sqlx::query("UPDATE runtime_controls SET revision=$3,active_request=NULL WHERE organization=$1 AND computer_id=$2")
            .bind(org).bind(computer).bind(revision).execute(&mut *tx).await?;
        let result = state(&mut tx, org, computer).await?;
        transactions::emit(&mut tx, org, seq, "computer.start_cancelled", serde_json::json!({"computer_id":computer,"request_id":request.request_id,"generation":current.generation,"revision":revision,"dispatch_started":false})).await?;
        transactions::save_receipt(&mut tx, &identity, CANCEL, key, &input, &result).await?;
        Store::authorize_service_in(&mut tx, token, ServiceScope::RuntimeManage).await?;
        tx.commit().await?;
        Ok(result)
    }
}
