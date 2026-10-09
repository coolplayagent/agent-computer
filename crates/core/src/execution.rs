//! Execution facts are distinct from business acceptance. No operation is replayed here.

use crate::identity::{check_generation, check_revision, *};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
}

impl Outcome {
    fn status(self) -> ExecutionStatus {
        match self {
            Self::Succeeded => ExecutionStatus::Succeeded,
            Self::Failed => ExecutionStatus::Failed,
            Self::Cancelled => ExecutionStatus::Cancelled,
        }
    }
}

/// A reference to durable output/receipt or an explicit reconciliation record.
/// Trusted adapters verify its persistence, provenance and actual outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Receipt {
    pub outcome: Outcome,
    pub evidence_id: EvidenceId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Execution {
    organization_id: OrganizationId,
    id: ExecutionId,
    computer_id: ComputerId,
    generation: Generation,
    input_digest: InputDigest,
    revision: Revision,
    status: ExecutionStatus,
    cancel_requested: bool,
    receipt: Option<Receipt>,
    unknown_evidence: Option<EvidenceId>,
    resolution: Option<Receipt>,
}

impl Execution {
    pub fn new(
        organization_id: OrganizationId,
        id: ExecutionId,
        computer_id: ComputerId,
        generation: Generation,
        input_digest: InputDigest,
    ) -> Result<Self> {
        if generation == Generation::INITIAL {
            return Err(Error::StaleGeneration);
        }
        Ok(Self {
            organization_id,
            id,
            computer_id,
            generation,
            input_digest,
            revision: Revision::INITIAL,
            status: ExecutionStatus::Queued,
            cancel_requested: false,
            receipt: None,
            unknown_evidence: None,
            resolution: None,
        })
    }

    pub fn organization_id(&self) -> &OrganizationId {
        &self.organization_id
    }

    pub fn id(&self) -> &ExecutionId {
        &self.id
    }
    pub fn computer_id(&self) -> &ComputerId {
        &self.computer_id
    }
    pub fn generation(&self) -> Generation {
        self.generation
    }
    pub fn input_digest(&self) -> InputDigest {
        self.input_digest
    }
    pub fn revision(&self) -> Revision {
        self.revision
    }
    pub fn status(&self) -> ExecutionStatus {
        self.status
    }
    pub fn cancel_requested(&self) -> bool {
        self.cancel_requested
    }
    pub fn receipt(&self) -> Option<&Receipt> {
        self.receipt.as_ref()
    }
    pub fn unknown_evidence(&self) -> Option<&EvidenceId> {
        self.unknown_evidence.as_ref()
    }
    pub fn resolution(&self) -> Option<&Receipt> {
        self.resolution.as_ref()
    }

    /// Persist Running/dispatch intent before the worker performs any side effect.
    pub fn dispatch(&mut self, expected: Revision, current_generation: Generation) -> Result<()> {
        check_revision(self.revision, expected)?;
        check_generation(self.generation, current_generation)?;
        if self.status != ExecutionStatus::Queued {
            return Err(Error::InvalidTransition);
        }
        self.revision = self.revision.next()?;
        self.status = ExecutionStatus::Running;
        Ok(())
    }

    /// Only a queued operation is known to have no dispatched process to stop.
    pub fn request_cancel(&mut self, expected: Revision) -> Result<()> {
        check_revision(self.revision, expected)?;
        if !matches!(
            self.status,
            ExecutionStatus::Queued | ExecutionStatus::Running | ExecutionStatus::Unknown
        ) || self.resolution.is_some()
        {
            return Err(Error::InvalidTransition);
        }
        if self.cancel_requested {
            return Ok(());
        }
        self.revision = self.revision.next()?;
        self.cancel_requested = true;
        if self.status == ExecutionStatus::Queued {
            self.status = ExecutionStatus::Cancelled;
        }
        Ok(())
    }

    pub fn finish(
        &mut self,
        expected: Revision,
        current_generation: Generation,
        receipt: Receipt,
    ) -> Result<()> {
        check_revision(self.revision, expected)?;
        check_generation(self.generation, current_generation)?;
        if self.status != ExecutionStatus::Running {
            return Err(Error::InvalidTransition);
        }
        self.revision = self.revision.next()?;
        self.status = receipt.outcome.status();
        self.receipt = Some(receipt);
        Ok(())
    }

    /// Called by the authority when dispatch may have occurred but no durable receipt exists.
    pub fn mark_unknown(&mut self, expected: Revision, evidence: EvidenceId) -> Result<()> {
        check_revision(self.revision, expected)?;
        if self.status != ExecutionStatus::Running {
            return Err(Error::InvalidTransition);
        }
        self.revision = self.revision.next()?;
        self.status = ExecutionStatus::Unknown;
        self.unknown_evidence = Some(evidence);
        Ok(())
    }

    /// A separately authorized reconciliation can resolve an old generation;
    /// worker callbacks must use finish instead. The original Unknown fact remains.
    pub fn reconcile(&mut self, expected: Revision, resolution: Receipt) -> Result<()> {
        check_revision(self.revision, expected)?;
        if self.status != ExecutionStatus::Unknown || self.resolution.is_some() {
            return Err(Error::InvalidTransition);
        }
        self.revision = self.revision.next()?;
        self.resolution = Some(resolution);
        Ok(())
    }
}
