//! Node-local continuous dispatch. Database claims, never transport retries, pick work.
use super::*;
use std::{collections::VecDeque, future::Future};
use tokio::{task::JoinSet, time::MissedTickBehavior};

pub use crate::queue_options::QueueOptions;

#[derive(Default, Debug, Serialize)]
pub struct QueueSummary {
    pub claimed: u64,
    pub cancelled_before_dispatch: u64,
    pub finished: u64,
    pub unconfirmed: u64,
    pub poll_failures: u64,
    pub completions_recovered: u64,
    pub completion_recovery_failures: u64,
}
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum QueueEvent {
    Ready {
        concurrency: usize,
        poll_interval_ms: u128,
    },
    Claimed {
        execution_id: String,
    },
    Cancelled {
        execution: Box<ExecutionRequest>,
    },
    Finished {
        result: Box<WorkResult>,
    },
    CompletionRecovered {
        receipt: ExecutionCompletion,
    },
    CompletionRecoveryFailed {
        execution_id: String,
    },
    Unconfirmed,
    PollFailed,
    Stopping {
        active: usize,
    },
}

/// Run one explicitly configured organization and qualified storage target.
/// Shutdown stops new claims and joins all admitted jobs. An in-progress claim
/// is allowed to finish; dropping an ambiguous committed claim is never a retry.
/// Persistent dispatches are never selected for execution after restart. A
/// separate bounded lane publishes only existing node drain receipts.
pub async fn run_queue(
    store: Store,
    client: Arc<Client>,
    org: OrganizationId,
    config: Configuration,
    options: QueueOptions,
    shutdown: impl Future<Output = ()>,
    mut report: impl FnMut(QueueEvent),
) -> Result<QueueSummary> {
    if config.candidate.target.namespace_uid != client.namespace_uid() {
        return Err(Error::ReferenceUnavailable);
    }
    let checked = config.clone();
    let (output_client, output_spool) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            agent_computer_node::validate_local_configuration(&checked.node)
                .map_err(|_| Error::ReferenceUnavailable)?;
            storage::validate_configuration(&checked.candidate)?;
            output_resources(&checked)
        }),
    )
    .await
    .map_err(|_| Error::ReferenceUnavailable)?
    .map_err(|_| Error::ReferenceUnavailable)??;
    enum Job {
        Dispatch(Box<WorkResult>),
        Recovery {
            execution_id: String,
            receipt: Result<Option<ExecutionCompletion>>,
        },
    }
    let mut jobs = JoinSet::new();
    let mut recovery_ids = VecDeque::new();
    let mut recovery_cursor: Option<String> = None;
    let mut prefer_recovery = true;
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
                stopping = true;
                report(QueueEvent::Stopping { active: jobs.len() });
            },
            Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                match result {
                    Ok(Ok(Job::Recovery { execution_id, receipt })) => {
                        match receipt {
                            Ok(Some(receipt)) => {
                                summary.completions_recovered = summary.completions_recovered.saturating_add(1);
                                report(QueueEvent::CompletionRecovered { receipt });
                            },
                            Ok(None) => {},
                            Err(_) => {
                                summary.completion_recovery_failures = summary.completion_recovery_failures.saturating_add(1);
                                report(QueueEvent::CompletionRecoveryFailed { execution_id });
                            },
                        }
                    },
                    Ok(Ok(Job::Dispatch(result))) => {
                        summary.finished = summary.finished.saturating_add(1);
                        report(QueueEvent::Finished { result });
                    },
                    _ => {
                        summary.unconfirmed = summary.unconfirmed.saturating_add(1);
                        report(QueueEvent::Unconfirmed);
                    },
                }
            },
            _ = interval.tick(), if !stopping && jobs.len() < options.concurrency => {
                if prefer_recovery {
                    if recovery_ids.is_empty() {
                        match tokio::time::timeout(Duration::from_secs(5), store.candidate_execution_completion_queue(&org, &config.candidate.target, &config.node.node, recovery_cursor.as_deref())).await {
                            Ok(Ok(ids)) => {
                                if ids.is_empty() { recovery_cursor = None; }
                                recovery_ids = ids.into();
                            },
                            _ => {
                                summary.poll_failures = summary.poll_failures.saturating_add(1);
                                report(QueueEvent::PollFailed);
                            },
                        }
                    }
                    if let Some(id) = recovery_ids.pop_front() {
                        recovery_cursor = Some(id.clone());
                        let (store, org, spool) = (store.clone(), org.clone(), config.node.spool.clone());
                        jobs.spawn(async move {
                            let receipt = recover_completion(&store, &org, &id, &spool).await;
                            Ok(Job::Recovery { execution_id: id, receipt })
                        });
                        prefer_recovery = false;
                        continue;
                    }
                }
                prefer_recovery = true;
                // The original admission budget includes lock/commit latency.
                // Timeout may be an ambiguous commit: later polls select Queued
                // only, never replay the lost dispatch or extend its deadline.
                match tokio::time::timeout(Duration::from_secs(5), store.claim_queued_candidate_execution(&org, &config.candidate.target)).await {
                    Ok(Ok(QueuedDispatch::Idle)) => {},
                    Ok(Ok(QueuedDispatch::Cancelled(execution))) => {
                        summary.cancelled_before_dispatch = summary.cancelled_before_dispatch.saturating_add(1);
                        report(QueueEvent::Cancelled { execution });
                    },
                    Ok(Ok(QueuedDispatch::Claimed(attempt))) => {
                        let id = attempt.intent().execution.execution_id.clone();
                        let (store, client, config, outputs, spool) = (store.clone(), client.clone(), config.clone(), output_client.clone(), output_spool.clone());
                        jobs.spawn(async move { execute_admitted(&store, &client, *attempt, config, outputs, spool).await.map(Box::new).map(Job::Dispatch) });
                        summary.claimed = summary.claimed.saturating_add(1);
                        report(QueueEvent::Claimed { execution_id: id });
                    },
                    _ => {
                        summary.poll_failures = summary.poll_failures.saturating_add(1);
                        report(QueueEvent::PollFailed);
                    },
                }
            },
        }
    }
}
