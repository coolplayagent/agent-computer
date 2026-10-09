//! Trusted local JuiceFS worker for one durably admitted Candidate.
use agent_computer_core::identity::OrganizationId;
use agent_computer_storage::{
    MountedVolume, ObjectCache,
    quota::{JuiceFsConfig, JuiceFsQuota},
};
use agent_computer_store::{
    Error, Store,
    reconciliation::{ClaimMode, WorkerId},
    runtime::preparation::*,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub target: PreparationTarget,
    pub mount_root: PathBuf,
    pub object_cache: PathBuf,
    pub quota: JuiceFsConfig,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkResult {
    Busy,
    Prepared,
    StorageUnknown,
}

pub async fn prepare_once(
    store: &Store,
    org: &OrganizationId,
    request_id: &str,
    owner: &WorkerId,
    config: Configuration,
) -> agent_computer_store::Result<WorkResult> {
    // Validate the actual local mount before consuming a durable dispatch permit.
    let target = &config.target;
    let mount = MountedVolume::open(
        &config.mount_root,
        &target.volume_path,
        &target.filesystem_uuid,
        &target.pvc_uid,
        target.writer_uid,
        target.writer_gid,
    )
    .map_err(|_| Error::ReferenceUnavailable)?;
    let cache = ObjectCache::open(&config.object_cache).map_err(|_| Error::ReferenceUnavailable)?;
    let quota = JuiceFsQuota::new(config.quota).map_err(|_| Error::ReferenceUnavailable)?;
    let lease = match store
        .claim_candidate_preparation(org, request_id, owner, target)
        .await?
    {
        PreparationClaim::Busy => return Ok(WorkResult::Busy),
        PreparationClaim::Prepared(_) => return Ok(WorkResult::Prepared),
        PreparationClaim::Claimed(lease) => *lease,
    };
    let permit = if lease.mode() == ClaimMode::Execute {
        Some(store.begin_candidate_preparation(&lease).await?)
    } else {
        None
    };
    let task = lease.clone();
    let result = tokio::task::spawn_blocking(move || {
        if let Some(permit) = permit {
            mount
                .prepare(permit.lease().request(), &cache, &quota)
                .map(Some)
        } else {
            mount.observe_prepared(task.request(), &quota)
        }
    })
    .await
    .map_err(|_| Error::InvalidReconcileResult)?;
    match result {
        Ok(Some(receipt)) => {
            store.finish_candidate_preparation(&lease, &receipt).await?;
            Ok(WorkResult::Prepared)
        }
        Ok(None) | Err(_) => {
            store.defer_candidate_preparation(&lease).await?;
            Ok(WorkResult::StorageUnknown)
        }
    }
}
