//! Durable artifact publication. Sealed Candidates never reopen for mutation.
use agent_computer_core::identity::OrganizationId;
use agent_computer_objects::{
    Client, Spool,
    artifact::{CapturedArtifact, verify},
};
use agent_computer_store::{
    Error, Store, reconciliation::WorkerId, runtime::artifacts::ArtifactCommit,
};
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc, time::Duration};
#[derive(Deserialize)]
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
    let client = Arc::new(Client::new(&config.objects).map_err(|_| Error::ReferenceUnavailable)?);
    let spool = Spool::open(&config.spool).map_err(|_| Error::ReferenceUnavailable)?;
    let Some(lease) = store.claim_artifact(org, id, owner).await? else {
        return Ok(None);
    };
    let work = async {
        if lease.target() != &config.storage.target {
            return Err(Error::ReferenceUnavailable);
        }
        let bundle = if let Some(bundle) = lease.capture() {
            bundle.clone()
        } else {
            let mount = crate::files::open_mount(&config.storage)?;
            let prepared = lease.prepared().clone();
            let object_client = client.clone();
            let local = spool.clone();
            let organization = org.as_str().to_owned();
            let commit = id.to_owned();
            let capture = tokio::task::spawn_blocking(move || {
                CapturedArtifact::capture(
                    &object_client,
                    &local,
                    &mount,
                    &prepared,
                    &organization,
                    &commit,
                )
            })
            .await
            .map_err(|_| Error::InvalidReconcileResult)?
            .map_err(|_| Error::InvalidReconcileResult)?;
            store.record_artifact_capture(&lease, &capture).await?;
            capture.bundle().clone()
        };
        let verified = verify(&client, &bundle, Some(&spool))
            .await
            .map_err(|_| Error::InvalidReconcileResult)?;
        store.finish_artifact(&lease, &verified).await.map(Some)
    };
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            result=&mut work=>{
                if result.is_err() {let _=store.release_artifact_worker(&lease).await;}
                return result;
            },
            _=tokio::time::sleep(Duration::from_secs(10))=>store.renew_artifact(&lease).await?,
        }
    }
}
