//! Exclusive ownership after verified draining; time always comes from the database.

use crate::identity::{check_generation, *};
use crate::{Error, Result};

pub const MAX_LEASE_MILLIS: u64 = 30_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LeaseScope {
    Gui(AppId),
    Modify(CandidateId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseState {
    Vacant,
    Held,
    Draining,
    Released,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseToken {
    organization_id: OrganizationId,
    lease_id: LeaseId,
    computer_id: ComputerId,
    computer_generation: Generation,
    epoch: LeaseEpoch,
    owner: ConnectionId,
}

impl LeaseToken {
    pub fn organization_id(&self) -> &OrganizationId {
        &self.organization_id
    }
    pub fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }
    pub fn computer_id(&self) -> &ComputerId {
        &self.computer_id
    }
    pub fn computer_generation(&self) -> Generation {
        self.computer_generation
    }
    pub fn epoch(&self) -> LeaseEpoch {
        self.epoch
    }
    pub fn owner(&self) -> &ConnectionId {
        &self.owner
    }
}

/// Trusted adapter assertion: all in-flight actions, pressed keys, and writers
/// for this exact token have drained. Never deserialize this from an API caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrainEvidence {
    pub token: LeaseToken,
    pub evidence_id: EvidenceId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lease {
    organization_id: OrganizationId,
    id: LeaseId,
    computer_id: ComputerId,
    computer_generation: Generation,
    scope: LeaseScope,
    state: LeaseState,
    epoch: LeaseEpoch,
    token: Option<LeaseToken>,
    expires_at_ms: u64,
    last_drain: Option<DrainEvidence>,
}

impl Lease {
    /// One authoritative lease row per computer generation and scope is required.
    pub fn new(
        organization_id: OrganizationId,
        id: LeaseId,
        computer_id: ComputerId,
        generation: Generation,
        scope: LeaseScope,
    ) -> Result<Self> {
        if generation == Generation::INITIAL {
            return Err(Error::StaleGeneration);
        }
        Ok(Self {
            organization_id,
            id,
            computer_id,
            computer_generation: generation,
            scope,
            state: LeaseState::Vacant,
            epoch: LeaseEpoch::INITIAL,
            token: None,
            expires_at_ms: 0,
            last_drain: None,
        })
    }

    pub fn organization_id(&self) -> &OrganizationId {
        &self.organization_id
    }

    pub fn state(&self) -> LeaseState {
        self.state
    }
    pub fn scope(&self) -> &LeaseScope {
        &self.scope
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    pub fn last_drain(&self) -> Option<&DrainEvidence> {
        self.last_drain.as_ref()
    }

    fn deadline(now_ms: u64, duration_ms: u64) -> Result<u64> {
        if duration_ms == 0 || duration_ms > MAX_LEASE_MILLIS {
            return Err(Error::InvalidDuration);
        }
        now_ms
            .checked_add(duration_ms)
            .ok_or(Error::CounterExhausted)
    }

    pub fn acquire(
        &mut self,
        owner: ConnectionId,
        current_generation: Generation,
        now_ms: u64,
        duration_ms: u64,
    ) -> Result<LeaseToken> {
        check_generation(self.computer_generation, current_generation)?;
        // Expiration is deliberately not a path to a new owner.
        if !matches!(self.state, LeaseState::Vacant | LeaseState::Released) {
            return Err(Error::LeaseBusy);
        }
        let deadline = Self::deadline(now_ms, duration_ms)?;
        let epoch = self.epoch.next()?;
        let token = LeaseToken {
            organization_id: self.organization_id.clone(),
            lease_id: self.id.clone(),
            computer_id: self.computer_id.clone(),
            computer_generation: self.computer_generation,
            epoch,
            owner,
        };
        self.epoch = epoch;
        self.token = Some(token.clone());
        self.expires_at_ms = deadline;
        self.state = LeaseState::Held;
        Ok(token)
    }

    fn matches_token(&self, token: &LeaseToken) -> Result<()> {
        if self.token.as_ref() == Some(token) {
            Ok(())
        } else {
            Err(Error::LeaseMismatch)
        }
    }

    /// The gateway must separately check current identity, grants and runtime readiness.
    pub fn authorize(
        &self,
        token: &LeaseToken,
        current_generation: Generation,
        now_ms: u64,
    ) -> Result<()> {
        check_generation(self.computer_generation, current_generation)?;
        self.matches_token(token)?;
        if self.state != LeaseState::Held {
            return Err(Error::LeaseBusy);
        }
        if now_ms >= self.expires_at_ms {
            return Err(Error::LeaseExpired);
        }
        Ok(())
    }

    pub fn renew(
        &mut self,
        token: &LeaseToken,
        current_generation: Generation,
        now_ms: u64,
        duration_ms: u64,
    ) -> Result<()> {
        self.authorize(token, current_generation, now_ms)?;
        self.expires_at_ms = Self::deadline(now_ms, duration_ms)?;
        Ok(())
    }

    pub fn begin_release(&mut self, token: &LeaseToken) -> Result<()> {
        self.matches_token(token)?;
        if self.state != LeaseState::Held {
            return Err(Error::InvalidTransition);
        }
        self.state = LeaseState::Draining;
        Ok(())
    }

    pub fn expire(&mut self, now_ms: u64) -> Result<()> {
        if self.state != LeaseState::Held || now_ms < self.expires_at_ms {
            return Err(Error::InvalidTransition);
        }
        self.state = LeaseState::Draining;
        Ok(())
    }

    pub fn confirm_drained(&mut self, evidence: DrainEvidence) -> Result<()> {
        self.matches_token(&evidence.token)?;
        if self.state != LeaseState::Draining {
            return Err(Error::InvalidTransition);
        }
        self.state = LeaseState::Released;
        self.last_drain = Some(evidence);
        Ok(())
    }
}
