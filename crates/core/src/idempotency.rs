//! Per-organization, principal and operation idempotency records. Storage must
//! enforce a unique scope/key and commit reservation with the execution and Outbox.

use crate::identity::*;
use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestIdentity {
    pub organization: OrganizationId,
    pub principal: PrincipalId,
    pub operation: OperationKind,
    pub key: IdempotencyKey,
    /// Digest of all canonical intent fields, including target and generation.
    pub input: InputDigest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotencyRecord {
    request: RequestIdentity,
    execution: ExecutionId,
    retired: bool,
}

impl IdempotencyRecord {
    pub fn new(request: RequestIdentity, execution: ExecutionId) -> Self {
        Self {
            request,
            execution,
            retired: false,
        }
    }

    pub fn retry(&self, request: &RequestIdentity) -> Result<&ExecutionId> {
        if self.request.organization != request.organization
            || self.request.principal != request.principal
            || self.request.operation != request.operation
            || self.request.key != request.key
        {
            return Err(Error::IdempotencyScopeMismatch);
        }
        if self.retired {
            return Err(Error::IdempotencyGone);
        }
        if self.request.input != request.input {
            return Err(Error::IdempotencyConflict);
        }
        Ok(&self.execution)
    }

    /// Keep the key tombstone so an expired request can never silently execute again.
    pub fn retire(&mut self) {
        self.retired = true;
    }
}
