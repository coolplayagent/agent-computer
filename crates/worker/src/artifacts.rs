//! Durable artifact publication. Sealed Candidates never reopen for mutation.
use agent_computer_core::identity::OrganizationId;
use agent_computer_objects::{
    Client, Spool,
    artifact::{CapturedArtifact, verify},
};
use agent_computer_store::{
    Error, Store, reconciliation::WorkerId, runtime::artifacts::ArtifactCommit,
};
use serde::{Deserialize, Serialize};
mod capture;
mod queue;
pub use queue::{QueueEvent, QueueOptions, QueueSummary, run_queue};
use std::{path::PathBuf, sync::Arc, time::Duration};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub storage: crate::files::Configuration,
    pub spool: PathBuf,
    pub objects: agent_computer_objects::Configuration,
}
pub async fn publish_once(
    store: &Store,
    org: &OrganizationId,
    id: &str,
    owner: &WorkerId,
    config: Configuration,
) -> agent_computer_store::Result<Option<ArtifactCommit>> {
    let checked = config.clone();
    let (client, spool) = tokio::task::spawn_blocking(move || resources(&checked))
        .await
        .map_err(|_| Error::ReferenceUnavailable)??;
    let Some(lease) = store.claim_artifact(org, id, owner).await? else {
        return Ok(None);
    };
    let result = publish_claimed(store, &lease, config, client, spool).await;
    if result.is_err() {
        let _ = store.release_artifact_worker(&lease).await;
    }
    result.map(Some)
}

fn resources(config: &Configuration) -> agent_computer_store::Result<(Arc<Client>, Spool)> {
    let client = Arc::new(Client::new(&config.objects).map_err(|_| Error::ReferenceUnavailable)?);
    let spool = Spool::open(&config.spool).map_err(|_| Error::ReferenceUnavailable)?;
    Ok((client, spool))
}

async fn publish_claimed(
    store: &Store,
    lease: &agent_computer_store::runtime::artifacts::ArtifactLease,
    config: Configuration,
    client: Arc<Client>,
    spool: Spool,
) -> agent_computer_store::Result<ArtifactCommit> {
    if lease.target() != &config.storage.target {
        return Err(Error::ReferenceUnavailable);
    }
    let captured = if lease.capture().is_none() {
        let prepared = lease.prepared().clone();
        let object_client = client.clone();
        let local = spool.clone();
        let organization = lease.organization().to_owned();
        let commit = lease.commit_id().to_owned();
        let task = tokio::task::spawn_blocking(move || {
            let mount = crate::files::open_mount(&config.storage)?;
            CapturedArtifact::capture(
                &object_client,
                &local,
                &mount,
                &prepared,
                &organization,
                &commit,
            )
            .map_err(|_| Error::InvalidReconcileResult)
        });
        Some(
            capture::joined(
                task,
                || store.renew_artifact(lease),
                Duration::from_secs(10),
            )
            .await?,
        )
    } else {
        None
    };
    let work = async {
        let bundle = if let Some(captured) = &captured {
            store.record_artifact_capture(lease, captured).await?;
            captured.bundle()
        } else {
            lease.capture().ok_or(Error::InvalidStoredData)?
        };
        let verified = verify(&client, bundle, Some(&spool))
            .await
            .map_err(|_| Error::InvalidReconcileResult)?;
        store.finish_artifact(lease, &verified).await
    };
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            result=&mut work=>return result,
            _=tokio::time::sleep(Duration::from_secs(10))=>store.renew_artifact(lease).await?,
        }
    }
}
