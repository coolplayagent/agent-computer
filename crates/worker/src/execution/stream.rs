use super::*;
use agent_computer_kubernetes::OutputChunkObservation;
use tokio::{sync::mpsc, task::JoinHandle};

/// At most eight queued observations and one active publication. Dropping the
/// controller cancels async object/DB IO; durable pending bytes remain retryable.
pub(super) struct Publisher {
    sender: Option<mpsc::Sender<OutputChunkObservation>>,
    task: JoinHandle<Result<()>>,
}
impl Publisher {
    pub fn new(
        store: Store,
        capture: ExecutionOutputCapture,
        client: Arc<agent_computer_objects::Client>,
        spool: agent_computer_objects::Spool,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            while let Some(observation) = receiver.recv().await {
                store
                    .collect_candidate_execution_chunk(&capture, &observation, &client, &spool)
                    .await?;
            }
            Ok(())
        });
        Self {
            sender: Some(sender),
            task,
        }
    }
    pub fn has_capacity(&self) -> bool {
        self.sender.as_ref().is_some_and(|s| s.capacity() > 0)
    }
    pub fn enqueue(&self, observation: OutputChunkObservation) -> Result<()> {
        self.sender
            .as_ref()
            .ok_or(Error::ExecutionOutputUnavailable)?
            .try_send(observation)
            .map_err(|_| Error::ExecutionOutputUnavailable)
    }
    pub fn failed(&self) -> bool {
        self.task.is_finished()
    }
    pub async fn finish(mut self) -> Result<()> {
        self.sender.take();
        tokio::time::timeout(Duration::from_secs(30), &mut self.task)
            .await
            .map_err(|_| Error::ExecutionOutputUnavailable)?
            .map_err(|_| Error::ExecutionOutputUnavailable)?
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}
