//! One durable App dispatch with private launch authorization and physical stop.
//! GUI requests use a serial durable queue; profile checkpoint/release is separate.
use agent_computer_browser::{Callback, Decision, Operation, Permission, Policy};
use agent_computer_core::identity::OrganizationId;
use agent_computer_kubernetes::{Client, DeleteOutcome, PodPhase};
use agent_computer_node::{BrowserGuard, arm_browser};
use agent_computer_store::{
    Error, Result, Store,
    runtime::apps::browser::{BrowserDeployment, BrowserDispatch, control::BrowserWork},
};
use agent_computer_watchdog::boottime_ms;
use serde::{Deserialize, Serialize};
use std::{future::Future, time::Duration};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub approved_image: String,
    pub viewport: agent_computer_browser::Viewport,
    pub node: agent_computer_node::Configuration,
}
#[derive(Debug, Serialize)]
pub struct WorkResult {
    pub instance_id: String,
    pub pod_uid: Option<String>,
    pub launch_observed: bool,
    pub process_stop_recorded: bool,
    pub drain_recorded: bool,
    pub api_cleanup_confirmed: bool,
    pub interrupted_at: Option<Phase>,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Create,
    Pod,
    Attach,
    Arm,
    Register,
    Launch,
    Running,
    Authority,
}
struct LaunchPolicy<'a> {
    store: &'a Store,
    attempt: &'a BrowserDispatch,
    guard: &'a mut BrowserGuard,
}
impl Policy for LaunchPolicy<'_> {
    async fn answer(&mut self, callback: &Callback) -> agent_computer_browser::Result<Decision> {
        let Callback::Authorize(request) = callback else {
            return Ok(Decision::Deny);
        };
        if request.permission != Permission::Launch {
            return Ok(Decision::Deny);
        }
        match bounded(
            self.store
                .authorize_browser_launch(self.attempt, self.guard, request),
        )
        .await
        {
            Ok(grant) => Ok(Decision::Grant(grant)),
            Err(_) => Ok(Decision::Deny),
        }
    }
}
struct CommandPolicy<'a> {
    store: &'a Store,
    attempt: &'a BrowserDispatch,
    guard: &'a mut BrowserGuard,
    work: &'a BrowserWork,
}
impl Policy for CommandPolicy<'_> {
    async fn answer(&mut self, callback: &Callback) -> agent_computer_browser::Result<Decision> {
        bounded(
            self.store
                .browser_callback(self.attempt, self.guard, self.work, callback),
        )
        .await
        .map_err(|_| agent_computer_browser::Error::Denied)
    }
}
struct NoCallbacks;
impl Policy for NoCallbacks {
    async fn answer(&mut self, _: &Callback) -> agent_computer_browser::Result<Decision> {
        Ok(Decision::Deny)
    }
}
async fn bounded<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .map_err(|_| Error::RuntimeAccessUnavailable)?
}
/// Explicit single attempt; no queue adoption and no automatic replay. Shutdown
/// drains the original handle. Abort/crash leaves fixed watchdogs and reservation
/// retention, not an inferred successful stop. First controller slice is at most
/// 30 seconds after node arm; no renewal is implied by a longer admission budget.
pub async fn execute_once(
    store: &Store,
    client: &Client,
    org: &OrganizationId,
    id: &str,
    expected_revision: i64,
    config: Configuration,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<WorkResult> {
    let deployment = BrowserDeployment {
        namespace: client.namespace().into(),
        namespace_uid: client.namespace_uid().into(),
        approved_image: config.approved_image.clone(),
        node: config.node.node.clone(),
        viewport: config.viewport,
    };
    client
        .probe()
        .await
        .map_err(|_| Error::ReferenceUnavailable)?;
    let attempt = bounded(store.begin_browser_app(org, id, expected_revision, &deployment)).await?;
    let mut result = WorkResult {
        instance_id: id.into(),
        pod_uid: None,
        launch_observed: false,
        process_stop_recorded: false,
        drain_recorded: false,
        api_cleanup_confirmed: false,
        interrupted_at: None,
    };
    let mut guard = None;
    let mut phase = Phase::Create;
    // The body always returns ownership of the original guard to cleanup. A
    // canceled network/database future cannot cause another Pod or launch POST.
    let work = async {
        attempt.remaining_budget_ms()?;
        let created = client
            .create(attempt.plan().pod_plan())
            .await
            .map_err(|_| Error::ReferenceUnavailable)?;
        result.pod_uid = Some(created.uid().into());
        bounded(store.record_browser_pod(&attempt, created.uid())).await?;
        phase = Phase::Pod;
        loop {
            attempt.remaining_budget_ms()?;
            let observed = client
                .observe(attempt.plan().pod_plan(), Some(created.uid()))
                .await
                .map_err(|_| Error::ReferenceUnavailable)?
                .ok_or(Error::ReferenceUnavailable)?;
            if matches!(observed.phase(), PodPhase::Succeeded | PodPhase::Failed) {
                return Err(Error::RuntimeAccessUnavailable);
            }
            if observed.phase() == PodPhase::Running
                && client
                    .observe_browser_runtime(attempt.plan(), created.uid())
                    .await
                    .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        phase = Phase::Attach;
        let mut channel = client
            .attach_browser(attempt.plan(), &created)
            .await
            .map_err(|_| Error::ReferenceUnavailable)?;
        phase = Phase::Arm;
        let deadline = boottime_ms() + u64::from(attempt.remaining_budget_ms()?.min(30_000));
        guard = Some(
            arm_browser(&config.node, channel.runtime().clone(), deadline)
                .map_err(|_| Error::RuntimeAccessUnavailable)?,
        );
        let active = guard.as_mut().ok_or(Error::RuntimeAccessUnavailable)?;
        phase = Phase::Register;
        bounded(store.register_browser_guard(&attempt, active)).await?;
        phase = Phase::Launch;
        let launch = channel
            .session_mut()
            .call(
                Operation::Launch,
                &mut LaunchPolicy {
                    store,
                    attempt: &attempt,
                    guard: active,
                },
            )
            .await
            .map_err(|_| Error::RuntimeAccessUnavailable)?;
        if !launch.ok() {
            return Err(Error::RuntimeAccessUnavailable);
        }
        result.launch_observed = true;
        phase = Phase::Running;
        bounded(store.record_browser_running(&attempt, active)).await?;
        phase = Phase::Authority;
        loop {
            let remaining = bounded(store.check_browser_app(&attempt, active)).await?;
            if remaining <= 1000 {
                break;
            }
            if let Some(drain) = bounded(store.next_browser_drain(&attempt, active)).await? {
                let response = channel
                    .session_mut()
                    .call(
                        Operation::Drain {
                            control_epoch: drain.control_epoch(),
                        },
                        &mut NoCallbacks,
                    )
                    .await
                    .map_err(|_| Error::RuntimeAccessUnavailable)?;
                bounded(store.finish_browser_drain(&attempt, active, drain, &response)).await?;
            }
            if let Some(work) = bounded(store.next_browser_work(&attempt, active)).await? {
                let response = channel
                    .session_mut()
                    .call(
                        work.operation(),
                        &mut CommandPolicy {
                            store,
                            attempt: &attempt,
                            guard: active,
                            work: &work,
                        },
                    )
                    .await;
                let safe = bounded(store.finish_browser_work(
                    &attempt,
                    active,
                    work,
                    response.as_ref().ok(),
                ))
                .await?;
                if !safe || response.is_err() || channel.session_mut().control_blocked() {
                    return Err(Error::RuntimeAccessUnavailable);
                }
            } else {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        // Keep the attach channel alive through the guarded lifetime. EOF is not
        // physical completion; only stop_browser below can record that proof.
        drop(channel);
        Ok(())
    };
    let remaining = Duration::from_millis(u64::from(attempt.remaining_budget_ms()?));
    let outcome = tokio::select! {
        value=tokio::time::timeout(remaining,work)=>value.unwrap_or(Err(Error::RuntimeAccessUnavailable)),
        ()=shutdown=>Err(Error::RuntimeAccessUnavailable),
    };
    if outcome.is_err() {
        result.interrupted_at = Some(phase);
    }
    result.drain_recorded = bounded(store.drain_browser_app(org, id)).await.is_ok();
    if let Some(guard) = guard
        && let Ok(stopped) = guard.stop_browser()
    {
        result.process_stop_recorded = bounded(store.record_browser_stop(org, id, &stopped))
            .await
            .is_ok();
        // Preserve StoppedBrowser until after publication; JSON from a node spool
        // is only historical evidence and cannot release this reservation.
        drop(stopped);
    }
    // A lost create reply cannot authorize name-based deletion of a replacement.
    if let Some(uid) = result.pod_uid.as_deref() {
        result.api_cleanup_confirmed =
            match client.observe(attempt.plan().pod_plan(), Some(uid)).await {
                Ok(None) => true,
                Ok(Some(observed)) => matches!(
                    client.delete(attempt.plan().pod_plan(), &observed).await,
                    Ok(DeleteOutcome::AlreadyAbsent)
                ),
                Err(_) => false,
            };
    }
    Ok(result)
}
