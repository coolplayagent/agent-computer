//! Continuous Artifact publication and checkpoint-stop work.
use super::*;
pub use crate::queue_options::QueueOptions;
use std::{collections::HashMap, future::Future, time::Duration};
use tokio::{
    task::{Id, JoinSet},
    time::{Instant, MissedTickBehavior},
};

#[derive(Default, Debug, Serialize)]
pub struct QueueSummary {
    pub scheduled: u64,
    pub committed: u64,
    pub stopped: u64,
    pub busy: u64,
    pub unconfirmed: u64,
    pub poll_failures: u64,
}
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum QueueEvent {
    Ready {
        concurrency: usize,
        poll_interval_ms: u128,
    },
    Scheduled {
        commit_id: String,
    },
    Finished {
        commit_id: String,
        outcome: Option<Box<ArtifactCommit>>,
    },
    Unconfirmed {
        commit_id: Option<String>,
    },
    PollFailed,
    Stopping {
        active: usize,
    },
}

/// Rotate eligible commits so revoked authority or failed storage does not
/// monopolize discovery. Busy leases and terminal commits are excluded.
pub async fn run_queue(
    store: Store,
    org: OrganizationId,
    owner: WorkerId,
    config: Configuration,
    options: QueueOptions,
    shutdown: impl Future<Output = ()>,
    mut report: impl FnMut(QueueEvent),
) -> agent_computer_store::Result<QueueSummary> {
    let checked = config.clone();
    tokio::task::spawn_blocking(move || {
        resources(&checked)?;
        crate::files::open_mount(&checked.storage)?;
        Ok::<_, Error>(())
    })
    .await
    .map_err(|_| Error::ReferenceUnavailable)??;
    let mut jobs = JoinSet::<agent_computer_store::Result<Option<ArtifactCommit>>>::new();
    let mut active = HashMap::<Id, String>::new();
    let mut cooldown = HashMap::<String, Instant>::new();
    let mut last = String::new();
    let mut interval = tokio::time::interval(options.poll_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tokio::pin!(shutdown);
    let mut stopping = false;
    let mut summary = QueueSummary::default();
    report(QueueEvent::Ready {
        concurrency: options.concurrency,
        poll_interval_ms: options.poll_interval.as_millis(),
    });
    loop {
        if stopping && jobs.is_empty() {
            return Ok(summary);
        }
        tokio::select! {
            biased;
            _ = &mut shutdown, if !stopping => {
                stopping=true;
                report(QueueEvent::Stopping { active:jobs.len() });
            },
            Some(result) = jobs.join_next_with_id(), if !jobs.is_empty() => {
                let (task, result) = match result {
                    Ok((task,result)) => (task, Some(result)),
                    Err(error) => (error.id(), None),
                };
                let commit_id = active.remove(&task);
                match result {
                    Some(Ok(outcome)) => {
                        if let Some(commit)=&outcome {
                            summary.committed=summary.committed.saturating_add(1);
                            if commit.stop_receipt.is_some() { summary.stopped=summary.stopped.saturating_add(1); }
                        } else { summary.busy=summary.busy.saturating_add(1); }
                        if let Some(commit_id)=commit_id {
                            if outcome.is_none() { cooldown.insert(commit_id.clone(),Instant::now()+Duration::from_secs(5)); }
                            report(QueueEvent::Finished { commit_id, outcome:outcome.map(Box::new) });
                        }
                    },
                    _ => {
                        summary.unconfirmed=summary.unconfirmed.saturating_add(1);
                        if let Some(id) = &commit_id { cooldown.insert(id.clone(),Instant::now()+Duration::from_secs(5)); }
                        report(QueueEvent::Unconfirmed { commit_id });
                    },
                }
            },
            _ = interval.tick(), if !stopping && jobs.len()<options.concurrency => {
                // Discovery is read-only, so shutdown can cancel it without
                // abandoning an ambiguously committed publication claim.
                let polled = tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        stopping=true;
                        report(QueueEvent::Stopping { active:jobs.len() });
                        continue;
                    },
                    result = tokio::time::timeout(Duration::from_secs(5),store.artifact_queue(&org,&config.storage.target)) => result,
                };
                let queue = match polled {
                    Ok(Ok(queue)) => queue,
                    _ => {
                        summary.poll_failures=summary.poll_failures.saturating_add(1);
                        report(QueueEvent::PollFailed);
                        continue;
                    },
                };
                cooldown.retain(|id,until| *until>Instant::now() && (queue.contains(id) || active.values().any(|v|v==id)));
                let eligible = |id: &&String| !active.values().any(|v|v==*id) && !cooldown.contains_key(*id);
                let id = queue.iter().filter(eligible).find(|id|*id>&last).or_else(||queue.iter().find(eligible)).cloned();
                if let Some(id) = id {
                    last=id.clone();
                    let (store,org,owner,config,request)=(store.clone(),org.clone(),owner.clone(),config.clone(),id.clone());
                    let task=jobs.spawn(async move { publish_once(&store,&org,&request,&owner,config).await });
                    active.insert(task.id(),id.clone());
                    summary.scheduled=summary.scheduled.saturating_add(1);
                    report(QueueEvent::Scheduled { commit_id:id });
                }
            },
        }
    }
}
