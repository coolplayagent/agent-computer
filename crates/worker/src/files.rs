//! One bounded trusted file save under a connection-owned Candidate lease.
use agent_computer_storage::{MountedVolume, files::FileEdit};
use agent_computer_store::{
    Error, Store,
    runtime::{preparation::PreparationTarget, writers::*},
};
use serde::Deserialize;
use std::path::PathBuf;

pub fn read_request(path: &std::path::Path) -> agent_computer_store::Result<SaveRequest> {
    let bytes = agent_computer_storage::quota::read_private(path, 5 * 1024 * 1024)
        .map_err(|_| Error::InvalidRuntimeRequest)?;
    serde_json::from_slice(&bytes).map_err(|_| Error::InvalidRuntimeRequest)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub target: PreparationTarget,
    pub mount_root: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRequest {
    pub lease: WriterLeaseCommand,
    /// Stable effect ID; do not generate a new ID on an unknown outcome.
    pub dispatch_id: String,
    pub edit: FileEdit,
}
pub async fn save_once(
    store: &Store,
    token: &str,
    id: &str,
    input: SaveRequest,
    config: Configuration,
) -> agent_computer_store::Result<WriterLease> {
    input
        .edit
        .validate()
        .map_err(|_| Error::InvalidRuntimeRequest)?;
    if let Some(current) = store
        .candidate_file_edit_result(token, id, &input.lease, &input.dispatch_id, &input.edit)
        .await?
    {
        return Ok(current);
    }
    let (prepared, target) = store
        .candidate_writer_storage(token, id, &input.lease)
        .await?;
    if target != config.target {
        return Err(Error::ReferenceUnavailable);
    }
    let mount = MountedVolume::open(
        &config.mount_root,
        &target.volume_path,
        &target.filesystem_uuid,
        &target.pvc_uid,
        target.writer_uid,
        target.writer_gid,
    )
    .map_err(|_| Error::ReferenceUnavailable)?;
    let digest = input
        .edit
        .digest(&prepared)
        .map_err(|_| Error::InvalidRuntimeRequest)?;
    let permit = store
        .begin_candidate_writer_dispatch(
            token,
            id,
            &input.lease,
            WriterDispatch {
                dispatch_id: &input.dispatch_id,
                input_digest: &digest,
            },
        )
        .await?;
    // Await the actual blocking task. Cancelling this future must not release the
    // lease: Tokio cancellation does not prove a FUSE call or thread stopped.
    let closed = tokio::task::spawn_blocking(move || permit.edit_file(mount, &input.edit))
        .await
        .map_err(|_| Error::InvalidReconcileResult)??;
    store.finish_candidate_file_edit(&closed).await
}
