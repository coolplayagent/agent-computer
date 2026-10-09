//! Environment domain contracts, independent of transports and runtime drivers.
#![forbid(unsafe_code)]

pub mod computer;
pub mod execution;
pub mod idempotency;
pub mod identity;
pub mod lease;

use std::fmt;

pub const API_VERSION: &str = "agent-computer/v1alpha1";
pub const VERSION: &str = "0.1.0";

/// Domain failures contain no caller-supplied text or credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidIdentifier,
    InvalidDigest,
    InvalidDuration,
    CounterExhausted,
    RevisionConflict,
    StaleGeneration,
    InvalidTransition,
    ActiveUse,
    EvidenceMismatch,
    CheckpointRequired,
    LeaseBusy,
    LeaseExpired,
    LeaseMismatch,
    IdempotencyScopeMismatch,
    IdempotencyConflict,
    IdempotencyGone,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
