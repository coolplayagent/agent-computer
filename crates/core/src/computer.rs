//! Lifecycle decisions. Callers must authorize and persist each mutation atomically.

use crate::identity::{check_generation, check_revision, *};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesiredState {
    Running,
    Stopped,
    Deleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservedState {
    Stopped,
    Starting,
    Ready,
    Draining,
    Error,
    RecoveryBlocked,
    Deleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopMode {
    Normal,
    Idle,
    Force,
}

/// Snapshot of activity evaluated under the same transaction as the stop decision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Activity {
    pub human_or_presentation: bool,
    pub execution: bool,
    pub lease: bool,
    pub keep_running: bool,
}

/// Reference to verified runtime termination/fencing, issued by a trusted adapter.
/// This value alone does not verify a process, node, or external side effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeFence {
    pub computer_id: ComputerId,
    pub generation: Generation,
    pub evidence_id: EvidenceId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckpointOutcome {
    Committed(CheckpointId),
    /// Explicit loss report; only accepted for an authorized force stop.
    Discarded(EvidenceId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Computer {
    organization_id: OrganizationId,
    id: ComputerId,
    workspace_id: WorkspaceId,
    revision: Revision,
    generation: Generation,
    desired: DesiredState,
    observed: ObservedState,
    stop_mode: Option<StopMode>,
    last_fence: Option<RuntimeFence>,
    checkpoint: Option<CheckpointId>,
    last_stop: Option<CheckpointOutcome>,
}

impl Computer {
    pub fn new(organization_id: OrganizationId, id: ComputerId, workspace_id: WorkspaceId) -> Self {
        Self {
            organization_id,
            id,
            workspace_id,
            revision: Revision::INITIAL,
            generation: Generation::INITIAL,
            desired: DesiredState::Stopped,
            observed: ObservedState::Stopped,
            stop_mode: None,
            last_fence: None,
            checkpoint: None,
            last_stop: None,
        }
    }

    pub fn organization_id(&self) -> &OrganizationId {
        &self.organization_id
    }

    pub fn id(&self) -> &ComputerId {
        &self.id
    }
    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }
    pub fn revision(&self) -> Revision {
        self.revision
    }
    pub fn generation(&self) -> Generation {
        self.generation
    }
    pub fn desired(&self) -> DesiredState {
        self.desired
    }
    pub fn observed(&self) -> ObservedState {
        self.observed
    }
    pub fn checkpoint(&self) -> Option<&CheckpointId> {
        self.checkpoint.as_ref()
    }
    pub fn last_stop(&self) -> Option<&CheckpointOutcome> {
        self.last_stop.as_ref()
    }
    pub fn last_fence(&self) -> Option<&RuntimeFence> {
        self.last_fence.as_ref()
    }

    pub fn start(&mut self, expected: Revision) -> Result<Generation> {
        check_revision(self.revision, expected)?;
        if matches!(
            self.observed,
            ObservedState::Starting | ObservedState::Ready
        ) {
            return Ok(self.generation);
        }
        if self.observed != ObservedState::Stopped {
            return Err(Error::InvalidTransition);
        }
        self.begin_generation()
    }

    fn begin_generation(&mut self) -> Result<Generation> {
        let revision = self.revision.next()?;
        let generation = self.generation.next()?;
        self.revision = revision;
        self.generation = generation;
        self.desired = DesiredState::Running;
        self.observed = ObservedState::Starting;
        self.stop_mode = None;
        Ok(generation)
    }

    pub fn ready(&mut self, expected: Revision, generation: Generation) -> Result<()> {
        check_revision(self.revision, expected)?;
        check_generation(self.generation, generation)?;
        if self.observed != ObservedState::Starting {
            return Err(Error::InvalidTransition);
        }
        self.revision = self.revision.next()?;
        self.observed = ObservedState::Ready;
        Ok(())
    }

    pub fn stop(&mut self, expected: Revision, mode: StopMode, activity: Activity) -> Result<()> {
        check_revision(self.revision, expected)?;
        if self.observed == ObservedState::Stopped {
            return Ok(());
        }
        if matches!(
            self.observed,
            ObservedState::Deleted | ObservedState::Draining
        ) {
            return Err(Error::InvalidTransition);
        }
        let blocked = match mode {
            StopMode::Normal => activity.human_or_presentation,
            StopMode::Idle => {
                activity.human_or_presentation
                    || activity.execution
                    || activity.lease
                    || activity.keep_running
            }
            StopMode::Force => false,
        };
        if blocked {
            return Err(Error::ActiveUse);
        }
        self.revision = self.revision.next()?;
        self.desired = DesiredState::Stopped;
        self.observed = ObservedState::Draining;
        self.stop_mode = Some(mode);
        Ok(())
    }

    fn validate_fence(&self, fence: &RuntimeFence) -> Result<()> {
        if self.id != fence.computer_id {
            return Err(Error::EvidenceMismatch);
        }
        check_generation(self.generation, fence.generation)
    }

    pub fn complete_stop(
        &mut self,
        expected: Revision,
        fence: RuntimeFence,
        checkpoint: CheckpointOutcome,
    ) -> Result<()> {
        check_revision(self.revision, expected)?;
        self.validate_fence(&fence)?;
        if self.observed != ObservedState::Draining {
            return Err(Error::InvalidTransition);
        }
        if matches!(checkpoint, CheckpointOutcome::Discarded(_))
            && self.stop_mode != Some(StopMode::Force)
        {
            return Err(Error::CheckpointRequired);
        }
        let revision = self.revision.next()?;
        if let CheckpointOutcome::Committed(id) = &checkpoint {
            self.checkpoint = Some(id.clone());
        }
        self.last_stop = Some(checkpoint);
        self.last_fence = Some(fence);
        self.revision = revision;
        self.observed = ObservedState::Stopped;
        self.stop_mode = None;
        Ok(())
    }

    /// A lost runtime is blocked until a matching fence has been independently verified.
    pub fn runtime_failed(
        &mut self,
        expected: Revision,
        generation: Generation,
        isolation_unknown: bool,
    ) -> Result<()> {
        check_revision(self.revision, expected)?;
        check_generation(self.generation, generation)?;
        if !matches!(
            self.observed,
            ObservedState::Starting | ObservedState::Ready | ObservedState::Draining
        ) {
            return Err(Error::InvalidTransition);
        }
        self.revision = self.revision.next()?;
        self.observed = if isolation_unknown {
            ObservedState::RecoveryBlocked
        } else {
            ObservedState::Error
        };
        Ok(())
    }

    /// The adapter must also validate authorization and checkpoint compatibility.
    pub fn recover(&mut self, expected: Revision, fence: RuntimeFence) -> Result<Generation> {
        check_revision(self.revision, expected)?;
        self.validate_fence(&fence)?;
        if !matches!(
            self.observed,
            ObservedState::Error | ObservedState::RecoveryBlocked
        ) {
            return Err(Error::InvalidTransition);
        }
        let generation = self.begin_generation()?;
        self.last_fence = Some(fence);
        Ok(generation)
    }

    /// Tombstone only this logical Computer; persistent workspace identity is retained.
    pub fn delete_stopped(&mut self, expected: Revision) -> Result<()> {
        check_revision(self.revision, expected)?;
        if self.observed != ObservedState::Stopped {
            return Err(Error::InvalidTransition);
        }
        self.revision = self.revision.next()?;
        self.desired = DesiredState::Deleted;
        self.observed = ObservedState::Deleted;
        Ok(())
    }
}
