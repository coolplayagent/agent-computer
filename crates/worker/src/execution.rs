//! Trusted single-execution controller. Local reports never release writer leases.
mod plan;
mod storage;
use agent_computer_core::identity::OrganizationId;
use agent_computer_kubernetes::{
    Client, DeleteOutcome, PodObservation, PodPhase, StartupObservation, StartupSandboxPlan,
};
use agent_computer_store::{Error, Result, Store, runtime::writers::*};
pub use plan::compile_plan;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub approved_supervisor_image: String,
    pub storage: agent_computer_kubernetes::volume::StorageClassBinding,
    pub candidate: crate::candidate::Configuration,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    PodPlan,
    PodCreate,
    PodStart,
    Attach,
    StorageCheck,
    Authorize,
    Run,
    Recovery,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    NoPlan,
    ApiAbsent,
    DeleteRequested,
    Unconfirmed,
}

/// Operator metadata plus bounded, private raw bytes for a future collector.
/// The raw observation is deliberately omitted from JSON/operator command output.
#[derive(Debug, Serialize)]
pub struct WorkResult {
    pub execution: ExecutionRequest,
    pub interrupted_at: Option<Phase>,
    pub cleanup: Cleanup,
    #[serde(skip)]
    pub observation: Option<StartupObservation>,
}

async fn active(store: &Store, org: &OrganizationId, id: &str) -> Result<ExecutionRequest> {
    let current = store.reconcile_candidate_execution(org, id).await?;
    if current.state != ExecutionState::Dispatching {
        return Err(Error::WriterLeaseInactive);
    }
    Ok(current)
}
async fn unknown(store: &Store, org: &OrganizationId, id: &str) -> Result<ExecutionRequest> {
    tokio::time::timeout(Duration::from_secs(5), unknown_inner(store, org, id))
        .await
        .map_err(|_| Error::RuntimeAccessUnavailable)?
}
async fn unknown_inner(store: &Store, org: &OrganizationId, id: &str) -> Result<ExecutionRequest> {
    // The only concurrent revision change is cancellation/authority loss. Retry
    // this monotone database transition, never an external create or grant.
    for _ in 0..3 {
        let current = store.reconcile_candidate_execution(org, id).await?;
        match store
            .mark_candidate_execution_unknown(org, id, current.revision)
            .await
        {
            Err(Error::RuntimeConflict) => continue,
            result => return result,
        }
    }
    Err(Error::RuntimeConflict)
}
fn backend<T>(result: agent_computer_kubernetes::Result<T>) -> Result<T> {
    result.map_err(|_| Error::ReferenceUnavailable)
}

/// Consume one queued admission. Identical/concurrent calls fail at the dispatch
/// journal and do not stop an active controller. Recovery is a separate operation.
/// A live controller polls authority and deletes conditionally on every outcome;
/// controller/PID 1 suspension still needs an independent external watchdog.
pub async fn execute_once(
    store: &Store,
    client: &Client,
    org: &OrganizationId,
    id: &str,
    expected_revision: i64,
    config: Configuration,
) -> Result<WorkResult> {
    let attempt = store
        .begin_candidate_execution_dispatch(org, id, expected_revision)
        .await?;
    // Commitment itself can consume the last millisecond. Still pass through
    // authority lowering if the returned in-process budget has already expired.
    let budget = Duration::from_millis(attempt.remaining_budget_ms().unwrap_or_default().into());
    let mut phase = Phase::Preparing;
    let mut plan = None;
    let mut journal = None;
    let mut observed = None;
    let run=tokio::time::timeout(budget, async {
        let inputs=store.candidate_execution_runtime_inputs(org,id).await?;
        plan=Some(compile_plan(&inputs,client,&config.storage,&config.approved_supervisor_image)?);
        let plan=plan.as_ref().ok_or(Error::InvalidStoredData)?;
        let local=storage::open(&inputs,config.candidate).await?;
        storage::verify(local.clone()).await?;
        phase=Phase::PodPlan;
        backend(client.probe_sandbox_storage(plan.pod_plan()).await)?;
        let permit=store.register_candidate_execution_pod(&attempt,client.namespace_uid(),&plan.pod_plan().manifest()).await?;
        journal=Some(permit.plan().clone());
        active(store,org,id).await?;
        permit.remaining_budget_ms()?;
        phase=Phase::PodCreate;
        // Exactly one call; even timeout/lost responses enter cleanup/recovery.
        observed=Some(backend(client.create(plan.pod_plan()).await)?);
        let first=observed.as_ref().ok_or(Error::InvalidStoredData)?;
        store.record_candidate_execution_pod(org,id,&permit.plan().plan_digest,first.uid()).await?;
        phase=Phase::PodStart;
        loop {
            active(store,org,id).await?;
            let current=observed.as_ref().ok_or(Error::InvalidStoredData)?;
            match current.phase() {
                PodPhase::Running => break,
                PodPhase::Pending => {},
                _ => return Err(Error::RuntimeConflict),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            observed=backend(client.observe(plan.pod_plan(),Some(current.uid())).await)?;
        }
        phase=Phase::Attach;
        let channel=backend(client.attach_startup(plan,observed.as_ref().ok_or(Error::InvalidStoredData)?).await)?;
        phase=Phase::StorageCheck;
        storage::verify(local).await?;
        phase=Phase::Authorize;
        let current=active(store,org,id).await?;
        let grant=store.authorize_candidate_execution_startup(org,id,current.revision,channel.pod_uid(),channel.challenge()).await?;
        grant.remaining_budget_ms()?;
        attempt.remaining_budget_ms()?;
        phase=Phase::Run;
        let run=channel.run(&grant.grant().grant);
        tokio::pin!(run);
        loop {
            tokio::select! {
                value=&mut run => break backend(value),
                _=tokio::time::sleep(Duration::from_millis(200)) => { active(store,org,id).await?; },
            }
        }
    }).await;
    let observation = run.ok().and_then(std::result::Result::ok);
    let interrupted_at = observation.is_none().then_some(phase);
    // Try to lower database authority first, but a database outage must not skip
    // best-effort conditional deletion of an already planned/observed instance.
    let state = unknown(store, org, id).await;
    let cleanup = match (&plan, &journal) {
        (Some(plan), Some(journal)) => {
            cleanup(
                store,
                client,
                org,
                id,
                plan,
                journal,
                observed.as_ref().map(PodObservation::uid),
            )
            .await
        }
        _ => Cleanup::NoPlan,
    };
    Ok(WorkResult {
        execution: state?,
        interrupted_at,
        cleanup,
        observation,
    })
}

async fn cleanup(
    store: &Store,
    client: &Client,
    org: &OrganizationId,
    id: &str,
    plan: &StartupSandboxPlan,
    journal: &ExecutionPodPlan,
    known_uid: Option<&str>,
) -> Cleanup {
    tokio::time::timeout(
        Duration::from_secs(15),
        cleanup_inner(store, client, org, id, plan, journal, known_uid),
    )
    .await
    .unwrap_or(Cleanup::Unconfirmed)
}
async fn cleanup_inner(
    store: &Store,
    client: &Client,
    org: &OrganizationId,
    id: &str,
    plan: &StartupSandboxPlan,
    journal: &ExecutionPodPlan,
    known_uid: Option<&str>,
) -> Cleanup {
    match client
        .observe(plan.pod_plan(), known_uid.or(journal.pod_uid.as_deref()))
        .await
    {
        Ok(None) => Cleanup::ApiAbsent,
        Ok(Some(observed)) => {
            // A failed observation write cannot suppress cleanup. It grants no
            // authority; exact UID/resourceVersion preconditions remain required.
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                store.record_candidate_execution_pod(org, id, &journal.plan_digest, observed.uid()),
            )
            .await;
            match client.delete(plan.pod_plan(), &observed).await {
                Ok(DeleteOutcome::Requested) => Cleanup::DeleteRequested,
                Ok(DeleteOutcome::AlreadyAbsent) => Cleanup::ApiAbsent,
                Err(_) => Cleanup::Unconfirmed,
            }
        }
        Err(_) => Cleanup::Unconfirmed,
    }
}

/// Explicitly stop/reconcile the original instance. This operation may interrupt
/// an existing controller by lowering authority. It never creates, attaches,
/// reconstructs a grant, or certifies physical drainage, even when no Pod exists.
pub async fn recover_once(
    store: &Store,
    client: &Client,
    org: &OrganizationId,
    id: &str,
    storage: &agent_computer_kubernetes::volume::StorageClassBinding,
    approved_image: &str,
) -> Result<WorkResult> {
    let state = unknown(store, org, id).await?;
    let journal = store.candidate_execution_pod(org, id).await?;
    let cleanup = if let Some(journal) = journal {
        let inputs = store.candidate_execution_runtime_inputs(org, id).await?;
        let plan = compile_plan(&inputs, client, storage, approved_image)?;
        if journal.namespace_uid != client.namespace_uid()
            || journal.manifest != plan.pod_plan().manifest()
        {
            return Err(Error::RuntimeConflict);
        }
        cleanup(store, client, org, id, &plan, &journal, None).await
    } else {
        Cleanup::NoPlan
    };
    Ok(WorkResult {
        execution: state,
        interrupted_at: Some(Phase::Recovery),
        cleanup,
        observation: None,
    })
}
