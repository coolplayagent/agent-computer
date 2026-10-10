//! Trusted single-execution controller. Completion requires live kernel and IO seals.
mod plan;
mod queue;
pub use queue::{QueueEvent, QueueOptions, QueueSummary, run_queue};
mod storage;
mod stream;
use agent_computer_core::identity::OrganizationId;
use agent_computer_kubernetes::{
    Client, DeleteOutcome, ExecutionEvent, PodObservation, PodPhase, StartupObservation,
    StartupSandboxPlan,
};
use agent_computer_store::{Error, Result, Store, runtime::writers::*};
pub use plan::compile_plan;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub outputs: agent_computer_objects::Configuration,
    pub output_spool: std::path::PathBuf,
    pub approved_supervisor_image: String,
    pub storage: agent_computer_kubernetes::volume::StorageClassBinding,
    pub candidate: crate::candidate::Configuration,
    pub node: agent_computer_node::Configuration,
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
    NodeBinding,
    Watchdog,
    WatchdogRegistration,
    Authorize,
    Run,
    RenewalAuthorize,
    RenewalWatchdog,
    RenewalAcknowledge,
    OutputStream,
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

/// Operator metadata plus bounded, private raw bytes collected into durable objects.
/// The raw observation is deliberately omitted from JSON/operator command output.
#[derive(Debug, Serialize)]
pub struct WorkResult {
    pub completion: Option<ExecutionCompletion>,
    pub io_fence: Option<agent_computer_fence::Evidence>,
    pub publication_revoked: bool,
    pub output: Option<ExecutionOutput>,
    pub output_unconfirmed: bool,
    pub execution: ExecutionRequest,
    pub interrupted_at: Option<Phase>,
    pub cleanup: Cleanup,
    pub node_error: Option<agent_computer_node::Error>,
    pub watchdog_journals: Option<agent_computer_node::JournalObservations>,
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
/// a trusted local node watchdog must be armed before the startup grant.
pub async fn execute_once(
    store: &Store,
    client: &Client,
    org: &OrganizationId,
    id: &str,
    expected_revision: i64,
    config: Configuration,
) -> Result<WorkResult> {
    let (output_client, output_spool) = output_resources(&config)?;
    let attempt = store
        .begin_candidate_execution_dispatch(org, id, expected_revision)
        .await?;
    execute_admitted(store, client, attempt, config, output_client, output_spool).await
}
fn output_resources(
    config: &Configuration,
) -> Result<(
    Arc<agent_computer_objects::Client>,
    agent_computer_objects::Spool,
)> {
    let client = agent_computer_objects::Client::new(&config.outputs)
        .map_err(|_| Error::ReferenceUnavailable)?;
    let spool = agent_computer_objects::Spool::open(&config.output_spool)
        .map_err(|_| Error::ReferenceUnavailable)?;
    Ok((Arc::new(client), spool))
}
async fn execute_admitted(
    store: &Store,
    client: &Client,
    mut attempt: ExecutionDispatchAttempt,
    config: Configuration,
    output_client: Arc<agent_computer_objects::Client>,
    output_spool: agent_computer_objects::Spool,
) -> Result<WorkResult> {
    let organization = OrganizationId::new(&attempt.intent().organization)
        .map_err(|_| Error::InvalidStoredData)?;
    let execution_id = attempt.intent().execution.execution_id.clone();
    let (org, id) = (&organization, execution_id.as_str());
    // Commitment itself can consume the last millisecond. Still pass through
    // authority lowering if the returned in-process budget has already expired.
    let budget = Duration::from_millis(attempt.remaining_budget_ms().unwrap_or_default().into());
    let hard_budget = Duration::from_millis(
        attempt
            .remaining_hard_budget_ms()
            .unwrap_or_default()
            .into(),
    );
    let mut phase = Phase::Preparing;
    let mut plan = None;
    let mut journal = None;
    let mut observed = None;
    let mut node_error = None;
    let mut fence = None;
    let mut node_guard = None;
    let mut registered = false;
    let mut publisher = None;
    let mut runtime_stopped = false;
    let run=tokio::time::timeout(hard_budget, async {
        let mut channel=tokio::time::timeout(budget, async {
        let inputs=store.candidate_execution_runtime_inputs(org,id).await?;
        let local=storage::open(&inputs,config.candidate).await?;
        storage::verify(local.clone()).await?;
        fence=Some(storage::fence(local.clone(),id.into()).await?);
        let fence=fence.as_ref().ok_or(Error::InvalidStoredData)?;
        plan=Some(plan::compile(&inputs,client,&config.storage,&config.approved_supervisor_image,Some((fence.reference().clone(),config.node.node.clone())))?);
        let plan=plan.as_ref().ok_or(Error::InvalidStoredData)?;
        phase=Phase::PodPlan;
        backend(client.probe_sandbox_storage(plan.pod_plan()).await)?;
        let permit=store.register_candidate_execution_pod(&attempt,client.namespace_uid(),&plan.pod_plan().manifest()).await?;
        journal=Some(permit.plan().clone());
        let registry=agent_computer_csi::Registry::open(std::path::Path::new(agent_computer_csi::server::REGISTRY)).map_err(|_|Error::ReferenceUnavailable)?;
        registry.register(fence,agent_computer_csi::PodBinding {namespace:client.namespace().into(),name:permit.plan().pod_name.clone(),node:config.node.node.name.clone()}).map_err(|_|Error::ReferenceUnavailable)?;
        registered=true;
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
        phase=Phase::NodeBinding;
        let runtime=backend(client.observe_runtime(plan.pod_plan(),channel.pod_uid(),&config.node.node).await)?;
        let anchor=agent_computer_watchdog::boottime_ms();
        let guard_deadline=anchor+u64::from(attempt.remaining_budget_ms()?.saturating_sub(2));
        let renewal=attempt.intent().hard_deadline_at_ms.map(|hard| agent_computer_watchdog::renewal::Policy {
            hard_deadline_boottime_ms:guard_deadline+(hard-attempt.intent().deadline_at_ms) as u64,
            authority_digest:attempt.intent().intent_digest.clone(),
        });
        let lease=agent_computer_node::ExecutionLease{deadline_boottime_ms:guard_deadline,renewal};
        let command=plan.pod_plan().manifest()["spec"]["containers"][0]["command"].clone();
        let execution=id.to_owned();
        let candidate=agent_computer_node::CandidateIdentity{data_inode:inputs.prepared.data_inode,volume_path:inputs.target.volume_path.clone()};
        phase=Phase::Watchdog;
        let live_fence=fence.clone();
        node_guard=Some(tokio::task::spawn_blocking(move || agent_computer_node::arm_fenced(&config.node,runtime,&execution,&command,&candidate,lease,&live_fence))
            .await.map_err(|_|Error::RuntimeAccessUnavailable)?.map_err(|error|{node_error=Some(error);Error::ReferenceUnavailable})?);
        let guard=node_guard.as_mut().ok_or(Error::InvalidStoredData)?;
        phase=Phase::WatchdogRegistration;
        store.register_candidate_execution_watchdog(&attempt,guard).await?;
        // Re-read API identity after host arming, before spending the grant.
        phase=Phase::NodeBinding;
        let actual=backend(client.observe_runtime(plan.pod_plan(),channel.pod_uid(),guard.evidence().runtime.identity.node()).await)?;
        if actual!=guard.evidence().runtime.identity {return Err(Error::RuntimeConflict)}
        phase=Phase::Authorize;
        let current=active(store,org,id).await?;
        let grant=store.authorize_guarded_candidate_execution_startup(org,id,current.revision,channel.pod_uid(),channel.challenge(),guard).await?;
        grant.remaining_budget_ms()?;
        attempt.remaining_budget_ms()?;
        guard.remaining_budget_ms().map_err(|_|Error::WriterLeaseInactive)?;
        let running=backend(channel.start(&grant.grant().grant).await)?;
        Ok::<_,Error>(running)
        }).await.map_err(|_|Error::WriterLeaseInactive)??;
        publisher=attempt.output_capture()?.map(|capture|stream::Publisher::new(store.clone(),capture,output_client.clone(),output_spool.clone()));
        phase=Phase::Run;
        loop {
            if publisher.as_ref().is_some_and(stream::Publisher::failed) {phase=Phase::OutputStream;return Err(Error::ExecutionOutputUnavailable);}
            let remaining=Duration::from_millis(attempt.remaining_budget_ms()?.into());
            tokio::select! {
                value=channel.next_event(), if publisher.as_ref().is_none_or(stream::Publisher::has_capacity) => match backend(value)? {
                    ExecutionEvent::Complete(observation) => break Ok(observation),
                    ExecutionEvent::Output(observation) => {
                        phase=Phase::OutputStream;
                        publisher.as_ref().ok_or(Error::InvalidReconcileResult)?.enqueue(observation)?;
                        phase=Phase::Run;
                    },
                    ExecutionEvent::Renewal(challenge) => {
                        if runtime_stopped {return Err(Error::WriterLeaseInactive);}
                        phase=Phase::RenewalAuthorize;
                        let guard=node_guard.as_mut().ok_or(Error::InvalidStoredData)?;
                        let permit=tokio::time::timeout(remaining,store.authorize_candidate_execution_renewal(&attempt,&challenge,guard)).await.map_err(|_|Error::WriterLeaseInactive)??;
                        let response=permit.grant().grant.clone();
                        let command=permit.grant().node_command.clone();
                        phase=Phase::RenewalWatchdog;
                        let mut guard=node_guard.take().ok_or(Error::InvalidStoredData)?;
                        let (guard,result)=tokio::task::spawn_blocking(move || { let result=guard.renew(&command); (guard,result) }).await.map_err(|_|Error::RuntimeAccessUnavailable)?;
                        node_guard=Some(guard);
                        result.map_err(|error|{node_error=Some(error);Error::ReferenceUnavailable})?;
                        phase=Phase::RenewalAcknowledge;
                        let remaining=Duration::from_millis(attempt.remaining_budget_ms()?.into());
                        tokio::time::timeout(remaining,store.acknowledge_candidate_execution_renewal(&mut attempt,permit,node_guard.as_mut().ok_or(Error::InvalidStoredData)?)).await.map_err(|_|Error::WriterLeaseInactive)??;
                        backend(channel.send_renewal(&response).await)?;
                        phase=Phase::Run;
                    },
                },
                _=tokio::time::sleep(remaining) => return Err(Error::WriterLeaseInactive),
                _=tokio::time::sleep(Duration::from_millis(200)) => {
                    let remaining=Duration::from_millis(attempt.remaining_budget_ms()?.into());
                    tokio::time::timeout(remaining,active(store,org,id)).await.map_err(|_|Error::WriterLeaseInactive)??;
                    if !runtime_stopped {
                        let guard=node_guard.as_mut().ok_or(Error::InvalidStoredData)?;
                        if guard.remaining_budget_ms().is_err() {
                            if !guard.process_termination_observed() {return Err(Error::WriterLeaseInactive);}
                            // The original domain is already empty, but a large
                            // report may still be buffered in attach transport.
                            fence.as_ref().ok_or(Error::InvalidStoredData)?.close();
                            runtime_stopped=true;
                        }
                    }
                },
            }
        }
    }).await;
    if let Some(fence) = &fence {
        fence.close();
    }
    let observation = run.ok().and_then(std::result::Result::ok);
    let interrupted_at = observation.is_none().then_some(phase);
    let publication_revoked = if registered {
        revoke(fence.as_ref().expect("registered live mount").instance()).await
    } else {
        false
    };
    // Kill/observe the original pinned process domain before API cleanup can
    // remove its paths. New IO is already closed; watchdog deadlines stay armed.
    let sealed = if let (Some(guard), Some(fence)) = (node_guard, fence.as_ref()) {
        let fence = fence.clone();
        match tokio::time::timeout(
            Duration::from_secs(12),
            tokio::task::spawn_blocking(move || guard.seal(&fence)),
        )
        .await
        {
            Ok(Ok(Ok(sealed))) => Some(sealed),
            Ok(Ok(Err(error))) => {
                node_error = Some(error);
                None
            }
            _ => {
                node_error = Some(agent_computer_node::Error::TerminationUnconfirmed);
                None
            }
        }
    } else {
        None
    };
    let stream_unconfirmed = if let Some(publisher) = publisher {
        publisher.finish().await.is_err()
    } else {
        false
    };
    let (output, output_unconfirmed) = if let Some(observation) = &observation {
        match tokio::time::timeout(
            Duration::from_secs(30),
            store.collect_candidate_execution_output(
                &attempt,
                observation,
                &output_client,
                &output_spool,
            ),
        )
        .await
        {
            Ok(Ok(output)) => (Some(output), false),
            _ => (None, true),
        }
    } else {
        (None, false)
    };
    let output_unconfirmed = output_unconfirmed || stream_unconfirmed;
    let completion = if let Some(sealed) = &sealed {
        tokio::time::timeout(Duration::from_secs(5), async {
            // The same immutable seal makes ambiguous commit responses safe to
            // retry. No process, grant, or side effect is dispatched again.
            for _ in 0..2 {
                if let Ok(receipt) = store.finish_candidate_execution(&attempt, sealed).await {
                    return Some(receipt);
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    let state = if completion.is_some() {
        tokio::time::timeout(
            Duration::from_secs(5),
            store.reconcile_candidate_execution(org, id),
        )
        .await
        .unwrap_or(Err(Error::RuntimeAccessUnavailable))
    } else {
        unknown(store, org, id).await
    };
    // Database unavailability must never suppress conditional external cleanup.
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
    let io_fence = if let Some(sealed) = &sealed {
        Some(sealed.evidence().io.clone())
    } else if let Some(fence) = fence {
        match tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                fence.seal().map(|sealed| sealed.evidence().clone())
            }),
        )
        .await
        {
            Ok(Ok(Ok(evidence))) => Some(evidence),
            _ => None,
        }
    } else {
        None
    };
    Ok(WorkResult {
        completion,
        io_fence,
        publication_revoked,
        output,
        output_unconfirmed,
        execution: state?,
        interrupted_at,
        cleanup,
        node_error,
        watchdog_journals: None,
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
    spool: &std::path::Path,
) -> Result<WorkResult> {
    let state = unknown(store, org, id).await?;
    let journal = store.candidate_execution_pod(org, id).await?;
    let mut publication_revoked = false;
    let cleanup = if let Some(journal) = journal {
        let inputs = store.candidate_execution_runtime_inputs(org, id).await?;
        let binding: serde_json::Value = serde_json::from_str(
            journal.manifest["metadata"]["annotations"]["agent-computer.io/binding"]
                .as_str()
                .ok_or(Error::InvalidStoredData)?,
        )
        .map_err(|_| Error::InvalidStoredData)?;
        let fence = if binding["workspace"]["fence"].is_null() {
            None
        } else {
            let mount: agent_computer_fence::MountReference =
                serde_json::from_value(binding["workspace"]["fence"]["mount"].clone())
                    .map_err(|_| Error::InvalidStoredData)?;
            let node = serde_json::from_value(binding["workspace"]["fence"]["node"].clone())
                .map_err(|_| Error::InvalidStoredData)?;
            Some((mount, node))
        };
        let plan = plan::compile(&inputs, client, storage, approved_image, fence.clone())?;
        if journal.namespace_uid != client.namespace_uid()
            || journal.manifest != plan.pod_plan().manifest()
        {
            return Err(Error::RuntimeConflict);
        }
        if let Some((mount, _)) = fence {
            publication_revoked = revoke(&mount.instance).await;
        }
        cleanup(store, client, org, id, &plan, &journal, None).await
    } else {
        Cleanup::NoPlan
    };
    // Complete authority lowering/conditional cleanup before local IO. A stuck
    // spool must not prevent those operations, nor manufacture a drain receipt.
    let (watchdog_journals, node_error) =
        if let Some(arm) = store.candidate_execution_watchdog(org, id).await? {
            let spool = spool.to_owned();
            match tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || {
                    agent_computer_node::observe_journals(&spool, &arm.evidence)
                }),
            )
            .await
            {
                Ok(Ok(Ok(value))) => (Some(value), None),
                Ok(Ok(Err(error))) => (None, Some(error)),
                _ => (None, Some(agent_computer_node::Error::Deadline)),
            }
        } else {
            (None, None)
        };
    Ok(WorkResult {
        completion: store.candidate_execution_completion(org, id).await?,
        io_fence: None,
        publication_revoked,
        output: None,
        output_unconfirmed: false,
        execution: state,
        interrupted_at: Some(Phase::Recovery),
        cleanup,
        node_error,
        watchdog_journals,
        observation: None,
    })
}
async fn revoke(instance: &str) -> bool {
    let instance = instance.to_owned();
    matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                agent_computer_csi::Registry::open(std::path::Path::new(
                    agent_computer_csi::server::REGISTRY,
                ))?
                .revoke(&instance)
            })
        )
        .await,
        Ok(Ok(Ok(())))
    )
}
