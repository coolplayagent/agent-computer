//! Trusted Browser controller API. Persisted records never reissue live permits.
pub mod control;
mod plan;
use super::*;
use agent_computer_browser::{AuthorizationRequest, Grant, Permission};
use agent_computer_kubernetes::BrowserPlan;
use agent_computer_node::{BrowserGuard, StoppedBrowser};
pub use plan::BrowserDeployment;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Non-cloneable original creation attempt. Do not repeat a POST after an
/// uncertain response, including when the durable commit response was lost.
pub struct BrowserDispatch {
    organization: String,
    instance_id: String,
    digest: String,
    plan: BrowserPlan,
    deadline: Instant,
}
impl BrowserDispatch {
    pub fn plan(&self) -> &BrowserPlan {
        &self.plan
    }
    pub fn organization(&self) -> &str {
        &self.organization
    }
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
    pub fn remaining_budget_ms(&self) -> Result<u32> {
        let n = self
            .deadline
            .saturating_duration_since(Instant::now())
            .as_millis();
        if n == 0 {
            return Err(Error::RuntimeAccessUnavailable);
        }
        u32::try_from(n).map_err(|_| Error::InvalidStoredData)
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct BrowserJournalEntry {
    pub kind: String,
    pub payload: Value,
    pub digest: String,
    pub recorded_at_ms: i64,
}
async fn journal(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    kind: &str,
) -> Result<Option<BrowserJournalEntry>> {
    let Some(r) = sqlx::query(
        "SELECT * FROM app_browser_journal WHERE organization=$1 AND instance_id=$2 AND kind=$3",
    )
    .bind(org)
    .bind(id)
    .bind(kind)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };
    let result = BrowserJournalEntry {
        kind: kind.into(),
        payload: r.try_get("payload")?,
        digest: r.try_get("digest")?,
        recorded_at_ms: r.try_get("recorded_at_ms")?,
    };
    if hash(org, id, kind, &result.payload, result.recorded_at_ms)? != result.digest {
        return Err(Error::InvalidStoredData);
    }
    Ok(Some(result))
}
fn hash(org: &str, id: &str, kind: &str, payload: &Value, now: i64) -> Result<String> {
    digest(
        "agent-computer/app-browser-journal-v1",
        &(org, id, kind, payload, now),
    )
}
async fn require(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    kind: &str,
) -> Result<BrowserJournalEntry> {
    journal(tx, org, id, kind)
        .await?
        .ok_or(Error::RuntimeAccessUnavailable)
}
async fn insert(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    kind: &str,
    payload: Value,
) -> Result<BrowserJournalEntry> {
    if journal(tx, org, id, kind).await?.is_some() {
        return Err(Error::DispatchAlreadyStarted);
    }
    let now = transactions::now(tx).await?;
    let hash = hash(org, id, kind, &payload, now)?;
    sqlx::query("INSERT INTO app_browser_journal (organization,instance_id,kind,payload,digest,recorded_at_ms) VALUES ($1,$2,$3,$4,$5,$6)").bind(org).bind(id).bind(kind).bind(&payload).bind(&hash).bind(now).execute(&mut **tx).await?;
    Ok(BrowserJournalEntry {
        kind: kind.into(),
        payload,
        digest: hash,
        recorded_at_ms: now,
    })
}
async fn bound(tx: &mut Transaction<'_, Postgres>, attempt: &BrowserDispatch) -> Result<PgRow> {
    attempt.remaining_budget_ms()?;
    let row = row(tx, &attempt.organization, &attempt.instance_id).await?;
    if !matches!(
        row.try_get::<String, _>("state")?.as_str(),
        "Dispatching" | "Running"
    ) {
        return Err(Error::RuntimeConflict);
    }
    if require(tx, &attempt.organization, &attempt.instance_id, "dispatch")
        .await?
        .digest
        != attempt.digest
    {
        return Err(Error::RuntimeConflict);
    }
    authority::bound(tx, &attempt.organization, &row).await?;
    Ok(row)
}
async fn live(
    tx: &mut Transaction<'_, Postgres>,
    attempt: &BrowserDispatch,
    guard: &mut BrowserGuard,
) -> Result<u32> {
    bound(tx, attempt).await?;
    let arm = require(tx, &attempt.organization, &attempt.instance_id, "arm").await?;
    let evidence = serde_json::to_value(guard.evidence()).map_err(|_| Error::InvalidStoredData)?;
    let now = transactions::now(tx).await?;
    let expiry = arm.payload["expires_at_ms"]
        .as_i64()
        .ok_or(Error::InvalidStoredData)?;
    if arm.payload["evidence"] != evidence || guard.renewal_evidence().is_some() || now >= expiry {
        return Err(Error::RuntimeAccessUnavailable);
    }
    Ok(guard
        .remaining_budget_ms()
        .map_err(|_| Error::RuntimeAccessUnavailable)?
        .min(attempt.remaining_budget_ms()?)
        .min(u32::try_from(expiry - now).map_err(|_| Error::InvalidStoredData)?))
}
async fn draining(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<()> {
    sqlx::query("UPDATE app_instances SET state='Draining',revision=revision+1 WHERE organization=$1 AND instance_id=$2 AND state IN ('Dispatching','Running')").bind(org).bind(id).execute(&mut **tx).await?;
    Ok(())
}
impl Store {
    /// Compile from immutable admission and commit one dispatch before any Pod
    /// mutation. Neither journal reads nor retries can recreate this handle.
    pub async fn begin_browser_app(
        &self,
        org: &OrganizationId,
        id: &str,
        revision: i64,
        deployment: &BrowserDeployment,
    ) -> Result<BrowserDispatch> {
        valid(id)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let row = row(&mut tx, org.as_str(), id).await?;
        authority::reconcile(&mut tx, org.as_str(), &row, seq).await?;
        let current = view(&mut tx, org.as_str(), id).await?;
        if current.state != AppState::Queued {
            tx.commit().await?;
            return Err(Error::RuntimeConflict);
        }
        if current.revision != revision {
            return Err(Error::RevisionConflict);
        }
        authority::bound(&mut tx, org.as_str(), &row).await?;
        let start = Instant::now();
        let remaining = u32::try_from(current.expires_at_ms - transactions::now(&mut tx).await?)
            .map_err(|_| Error::RuntimeAccessUnavailable)?;
        let plan = plan::compile(
            org.as_str(),
            &current,
            &row.try_get("binding")?,
            deployment,
            remaining,
        )?;
        let entry=insert(&mut tx,org.as_str(),id,"dispatch",json!({"binding_digest":current.binding_digest,"namespace_uid":deployment.namespace_uid,"manifest":plan.pod_plan().manifest(),"expires_at_ms":current.expires_at_ms})).await?;
        sqlx::query("UPDATE app_instances SET state='Dispatching',revision=revision+1 WHERE organization=$1 AND instance_id=$2").bind(org.as_str()).bind(id).execute(&mut *tx).await?;
        transactions::emit(
            &mut tx,
            org.as_str(),
            seq,
            "app.browser_dispatch_planned",
            json!({"instance_id":id,"digest":entry.digest,"pod_created_confirmed":false}),
        )
        .await?;
        authority::bound(&mut tx, org.as_str(), &row).await?;
        let attempt = BrowserDispatch {
            organization: org.as_str().into(),
            instance_id: id.into(),
            digest: entry.digest,
            plan,
            deadline: start + Duration::from_millis(u64::from(remaining)),
        };
        attempt.remaining_budget_ms()?;
        tx.commit().await?;
        Ok(attempt)
    }
    /// Trusted adapter readback only, including after expiry. This records the
    /// original exact manifest's UID, without reauthorizing creation or launch.
    pub async fn record_browser_pod(&self, attempt: &BrowserDispatch, uid: &str) -> Result<()> {
        valid(uid)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, &attempt.organization).await?;
        if require(
            &mut tx,
            &attempt.organization,
            &attempt.instance_id,
            "dispatch",
        )
        .await?
        .digest
            != attempt.digest
        {
            return Err(Error::RuntimeConflict);
        }
        let value = json!({"pod_uid":uid});
        if let Some(old) =
            journal(&mut tx, &attempt.organization, &attempt.instance_id, "pod").await?
        {
            if old.payload != value {
                return Err(Error::RuntimeConflict);
            }
        } else {
            insert(
                &mut tx,
                &attempt.organization,
                &attempt.instance_id,
                "pod",
                value,
            )
            .await?;
            transactions::emit(
                &mut tx,
                &attempt.organization,
                seq,
                "app.browser_pod_observed",
                json!({"instance_id":attempt.instance_id,"pod_uid":uid}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn register_browser_guard(
        &self,
        attempt: &BrowserDispatch,
        guard: &mut BrowserGuard,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, &attempt.organization).await?;
        bound(&mut tx, attempt).await?;
        let dispatch = require(
            &mut tx,
            &attempt.organization,
            &attempt.instance_id,
            "dispatch",
        )
        .await?;
        let pod = require(&mut tx, &attempt.organization, &attempt.instance_id, "pod").await?;
        let runtime = &guard.evidence().runtime;
        let identity = &runtime.identity;
        if runtime.binding != *attempt.plan.binding()
            || identity.pod_uid()
                != pod.payload["pod_uid"]
                    .as_str()
                    .ok_or(Error::InvalidStoredData)?
            || identity.namespace_uid()
                != dispatch.payload["namespace_uid"]
                    .as_str()
                    .ok_or(Error::InvalidStoredData)?
            || identity.namespace() != attempt.plan.pod_plan().namespace()
            || identity.pod_name() != attempt.plan.pod_plan().pod_name()
            || identity.node() != attempt.plan.node()
            || guard.evidence().armed.request.execution_id != attempt.instance_id
            || guard.evidence().armed.request.renewal.is_some()
            || guard.renewal_evidence().is_some()
        {
            return Err(Error::RuntimeConflict);
        }
        let remaining = guard
            .remaining_budget_ms()
            .map_err(|_| Error::RuntimeAccessUnavailable)?
            .min(attempt.remaining_budget_ms()?);
        let now = transactions::now(&mut tx).await?;
        let evidence =
            serde_json::to_value(guard.evidence()).map_err(|_| Error::InvalidStoredData)?;
        insert(
            &mut tx,
            &attempt.organization,
            &attempt.instance_id,
            "arm",
            json!({"evidence":evidence,"expires_at_ms":now+i64::from(remaining)}),
        )
        .await?;
        transactions::emit(
            &mut tx,
            &attempt.organization,
            seq,
            "app.browser_watchdog_armed",
            json!({"instance_id":attempt.instance_id,"pod_uid":pod.payload["pod_uid"]}),
        )
        .await?;
        live(&mut tx, attempt, guard).await?;
        tx.commit().await?;
        Ok(())
    }
    /// Original service credential and all admission grants are checked again.
    /// This API grants launch only; it never grants observation or GUI control.
    pub async fn authorize_browser_launch(
        &self,
        attempt: &BrowserDispatch,
        guard: &mut BrowserGuard,
        request: &AuthorizationRequest,
    ) -> Result<Grant> {
        if request.scope != attempt.plan.binding().scope
            || request.actor != attempt.plan.binding().actor
            || request.permission != Permission::Launch
        {
            return Err(Error::RuntimeAccessUnavailable);
        }
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, &attempt.organization).await?;
        live(&mut tx, attempt, guard).await?;
        insert(
            &mut tx,
            &attempt.organization,
            &attempt.instance_id,
            "launch",
            json!({"request":request}),
        )
        .await?;
        transactions::emit(
            &mut tx,
            &attempt.organization,
            seq,
            "app.browser_launch_requested",
            json!({"instance_id":attempt.instance_id,"launch_confirmed":false}),
        )
        .await?;
        let remaining = live(&mut tx, attempt, guard).await?;
        let before = Instant::now();
        tx.commit().await?;
        // Time spent committing cannot extend the original lease.
        let remaining = remaining
            .saturating_sub(u32::try_from(before.elapsed().as_millis()).unwrap_or(u32::MAX));
        let remaining = remaining.min(attempt.remaining_budget_ms()?).min(
            guard
                .remaining_budget_ms()
                .map_err(|_| Error::RuntimeAccessUnavailable)?,
        );
        if remaining == 0 {
            return Err(Error::RuntimeAccessUnavailable);
        }
        Ok(Grant {
            scope: request.scope.clone(),
            actor: request.actor.clone(),
            permission: Permission::Launch,
            control_epoch: 0,
            valid_for_ms: remaining,
        })
    }
    /// A launch response is an untrusted driver observation; the original live
    /// guard and current authority must still be present to mark it Running.
    pub async fn record_browser_running(
        &self,
        attempt: &BrowserDispatch,
        guard: &mut BrowserGuard,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, &attempt.organization).await?;
        live(&mut tx, attempt, guard).await?;
        require(
            &mut tx,
            &attempt.organization,
            &attempt.instance_id,
            "launch",
        )
        .await?;
        insert(
            &mut tx,
            &attempt.organization,
            &attempt.instance_id,
            "running",
            json!({"driver_launch_observed":true,"ready":false}),
        )
        .await?;
        sqlx::query("UPDATE app_instances SET state='Running',revision=revision+1 WHERE organization=$1 AND instance_id=$2 AND state='Dispatching'").bind(&attempt.organization).bind(&attempt.instance_id).execute(&mut *tx).await?;
        transactions::emit(
            &mut tx,
            &attempt.organization,
            seq,
            "app.browser_running",
            json!({"instance_id":attempt.instance_id,"ready":false}),
        )
        .await?;
        live(&mut tx, attempt, guard).await?;
        tx.commit().await?;
        Ok(())
    }
    /// Polling current authority cannot extend the fixed node watchdog deadline.
    pub async fn check_browser_app(
        &self,
        attempt: &BrowserDispatch,
        guard: &mut BrowserGuard,
    ) -> Result<u32> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, &attempt.organization).await?;
        let remaining = live(&mut tx, attempt, guard).await?;
        tx.commit().await?;
        Ok(remaining)
    }
    /// Uncertain start/failed guard/controller shutdown: retain the reservation.
    pub async fn drain_browser_app(&self, org: &OrganizationId, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let r = row(&mut tx, org.as_str(), id).await?;
        let state = r.try_get::<String, _>("state")?;
        if !matches!(state.as_str(), "Dispatching" | "Running" | "Draining") {
            return Err(Error::RuntimeConflict);
        }
        if matches!(state.as_str(), "Dispatching" | "Running") {
            draining(&mut tx, org.as_str(), id).await?;
            transactions::emit(
                &mut tx,
                org.as_str(),
                seq,
                "app.browser_draining",
                json!({"instance_id":id,"process_stop_confirmed":false}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// Accept only the original node adapter's live termination handle. Persisted
    /// JSON, Pod absence and elapsed deadlines cannot call this API or release a profile.
    pub async fn record_browser_stop(
        &self,
        org: &OrganizationId,
        id: &str,
        stopped: &StoppedBrowser,
    ) -> Result<()> {
        let value =
            serde_json::to_value(stopped.evidence()).map_err(|_| Error::InvalidStoredData)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let arm = require(&mut tx, org.as_str(), id, "arm").await?;
        if value["arm"] != arm.payload["evidence"] {
            return Err(Error::RuntimeConflict);
        }
        if let Some(old) = journal(&mut tx, org.as_str(), id, "stop").await? {
            if old.payload != value {
                return Err(Error::RuntimeConflict);
            }
        } else {
            insert(&mut tx, org.as_str(), id, "stop", value).await?;
            draining(&mut tx, org.as_str(), id).await?;
            transactions::emit(&mut tx,org.as_str(),seq,"app.browser_process_stopped",json!({"instance_id":id,"profile_checkpoint_confirmed":false,"reservation_released":false})).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// Operator-only historical evidence; no creation/startup/GUI authority.
    pub async fn browser_app_journal(
        &self,
        org: &OrganizationId,
        id: &str,
    ) -> Result<Vec<BrowserJournalEntry>> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org.as_str()).await?;
        row(&mut tx, org.as_str(), id).await?;
        let mut result = Vec::new();
        for kind in ["dispatch", "pod", "arm", "launch", "running", "stop"] {
            if let Some(entry) = journal(&mut tx, org.as_str(), id, kind).await? {
                result.push(entry);
            }
        }
        tx.commit().await?;
        Ok(result)
    }
}

pub(super) async fn check_deadline(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
) -> Result<()> {
    if let Some(arm) = journal(tx, org, id, "arm").await? {
        let expiry = arm.payload["expires_at_ms"]
            .as_i64()
            .ok_or(Error::InvalidStoredData)?;
        if transactions::now(tx).await? >= expiry {
            return Err(Error::RuntimeAccessUnavailable);
        }
    }
    Ok(())
}
