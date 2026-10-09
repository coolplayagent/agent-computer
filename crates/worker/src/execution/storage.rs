use agent_computer_storage::{MountedVolume, PrepareRequest, Prepared, quota::JuiceFsQuota};
use agent_computer_store::{Error, Result, runtime::writers::ExecutionRuntimeInputs};
use std::sync::Arc;

pub(super) struct LocalStorage {
    mount: MountedVolume,
    quota: JuiceFsQuota,
    request: PrepareRequest,
    prepared: Prepared,
}
pub(super) async fn open(
    inputs: &ExecutionRuntimeInputs,
    config: crate::candidate::Configuration,
) -> Result<Arc<LocalStorage>> {
    if config.target != inputs.target {
        return Err(Error::ReferenceUnavailable);
    }
    let request = inputs.preparation.clone();
    let prepared = inputs.prepared.clone();
    tokio::task::spawn_blocking(move || {
        let t = config.target;
        let mount = MountedVolume::open(
            &config.mount_root,
            &t.volume_path,
            &t.filesystem_uuid,
            &t.pvc_uid,
            t.writer_uid,
            t.writer_gid,
        )
        .map_err(|_| Error::ReferenceUnavailable)?;
        let quota = JuiceFsQuota::new(config.quota).map_err(|_| Error::ReferenceUnavailable)?;
        Ok(Arc::new(LocalStorage {
            mount,
            quota,
            request,
            prepared,
        }))
    })
    .await
    .map_err(|_| Error::ReferenceUnavailable)?
}
pub(super) async fn verify(local: Arc<LocalStorage>) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        if local
            .mount
            .observe_prepared(&local.request, &local.quota)
            .map_err(|_| Error::ReferenceUnavailable)?
            .as_ref()
            != Some(&local.prepared)
        {
            return Err(Error::ReferenceUnavailable);
        }
        Ok(())
    })
    .await
    .map_err(|_| Error::ReferenceUnavailable)?
}
