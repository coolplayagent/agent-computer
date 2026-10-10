//! Publication of an original node's already sealed drain receipt. No Pod API,
//! process launch, output inference, lease renewal or authority reconstruction.
use super::*;

pub async fn recover_completion(
    store: &Store,
    org: &OrganizationId,
    id: &str,
    spool: &std::path::Path,
) -> Result<Option<ExecutionCompletion>> {
    if let Some(receipt) = store.candidate_execution_completion(org, id).await? {
        return Ok(Some(receipt));
    }
    let Some(arm) = store.candidate_execution_watchdog(org, id).await? else {
        return Ok(None);
    };
    let spool = spool.to_owned();
    // Join the blocking read even during shutdown. Detaching a stuck filesystem
    // operation would turn a bounded worker into unbounded background IO.
    let sealed = tokio::task::spawn_blocking(move || {
        agent_computer_node::read_recorded_seal(&spool, &arm.evidence)
    })
    .await
    .map_err(|_| Error::RuntimeAccessUnavailable)?
    .map_err(|_| Error::InvalidReconcileResult)?;
    let Some(sealed) = sealed else {
        return Ok(None);
    };
    store
        .recover_candidate_execution_completion(org, id, &sealed)
        .await
        .map(Some)
}
