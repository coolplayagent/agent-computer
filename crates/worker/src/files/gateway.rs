//! Bounded file IO jobs survive an HTTP timeout. The slot stays owned until the
//! blocking IO and result acceptance finish; cancellation is never drain proof.
use super::*;
use agent_computer_store::runtime::files::ReadFileRequest;
use std::{future::Future, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub use agent_computer_storage::files::FileRead;
pub const FILE_IO_SLOTS: usize = 4;
#[derive(Clone)]
pub struct FileGateway {
    volumes: Arc<Vec<Configuration>>,
    slots: Arc<Semaphore>,
}
pub struct FileSlot(OwnedSemaphorePermit);
impl FileSlot {
    /// Start an owned job. Dropping the waiting caller does not abort it or free
    /// its slot while a blocking filesystem operation may still be running.
    pub async fn run<T: Send + 'static>(
        self,
        job: impl Future<Output = agent_computer_store::Result<T>> + Send + 'static,
    ) -> agent_computer_store::Result<T> {
        tokio::spawn(async move {
            let _slot = self.0;
            job.await
        })
        .await
        .map_err(|_| Error::InvalidReconcileResult)?
    }
}
impl FileGateway {
    pub fn new(volumes: Vec<Configuration>) -> agent_computer_store::Result<Self> {
        if volumes.is_empty()
            || volumes.len() > 16
            || volumes.iter().any(|v| !v.mount_root.is_absolute())
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        let mut ids = std::collections::BTreeSet::new();
        if volumes.iter().any(|v| !ids.insert(&v.target.volume_id)) {
            return Err(Error::InvalidRuntimeRequest);
        }
        Ok(Self {
            volumes: Arc::new(volumes),
            slots: Arc::new(Semaphore::new(FILE_IO_SLOTS)),
        })
    }
    pub fn admit(&self) -> agent_computer_store::Result<FileSlot> {
        self.slots
            .clone()
            .try_acquire_owned()
            .map(FileSlot)
            .map_err(|_| Error::RuntimeCapacityUnavailable)
    }
    fn config(&self, target: &PreparationTarget) -> agent_computer_store::Result<Configuration> {
        self.volumes
            .iter()
            .find(|v| &v.target == target)
            .cloned()
            .ok_or(Error::ReferenceUnavailable)
    }
    pub async fn read(
        &self,
        store: &Store,
        token: &str,
        workspace: &str,
        input: &ReadFileRequest,
    ) -> agent_computer_store::Result<FileRead> {
        let admitted = store.candidate_file_read(token, workspace, input).await?;
        let config = self.config(&admitted.target)?;
        let prepared = admitted.prepared.clone();
        let path = input.path.clone();
        let result = tokio::task::spawn_blocking(move || {
            open_mount(&config)?
                .read_file(&prepared, &path)
                .map_err(|_| Error::FileUnavailable)?
                .ok_or(Error::FileUnavailable)
        })
        .await
        .map_err(|_| Error::InvalidReconcileResult)?;
        // Recheck even on an IO error, so failure responses cannot disclose
        // current path existence after authority has disappeared.
        let current = store.candidate_file_read(token, workspace, input).await?;
        if current != admitted {
            return Err(Error::RuntimeConflict);
        }
        result
    }
    pub async fn save(
        &self,
        store: &Store,
        token: &str,
        id: &str,
        input: SaveRequest,
    ) -> agent_computer_store::Result<WriterLease> {
        if let Some(result) = store
            .candidate_file_edit_result(token, id, &input.lease, &input.dispatch_id, &input.edit)
            .await?
        {
            return Ok(result);
        }
        let (_, target) = store
            .candidate_writer_storage(token, id, &input.lease)
            .await?;
        save_once(store, token, id, input, self.config(&target)?).await
    }
}
