use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId, Revision};
use agent_computer_definitions::ValidatedDefinition;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Registry revision, independent of expectedRevision fields inside a ComputerSet.
#[derive(Clone, Copy, Debug)]
pub enum Precondition {
    Create,
    Match(Revision),
}

pub struct RecordDeclaration<'a> {
    pub organization: &'a OrganizationId,
    pub principal: &'a PrincipalId,
    pub key: &'a IdempotencyKey,
    pub precondition: Precondition,
    pub definition: &'a ValidatedDefinition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    pub name: String,
    pub revision: i64,
    pub digest: String,
    pub event_sequence: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclarationVersion {
    pub name: String,
    pub revision: i64,
    pub digest: String,
    pub canonical: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub sequence: i64,
    pub kind: String,
    /// Metadata only: never copies the declaration body into the event stream.
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub declarations: Vec<DeclarationVersion>,
    pub watermark: i64,
}

#[derive(Clone, Debug)]
pub struct EventPage {
    pub events: Vec<Event>,
    /// Continue after this value. A limited page never jumps to the watermark.
    pub next_cursor: i64,
    pub watermark: i64,
}

#[derive(Debug)]
pub enum Error {
    StaleReconcileLease,
    DispatchAlreadyStarted,
    InvalidReconcileResult,
    OperationNotBlocked,
    PreconditionRequired,
    ReferenceUnavailable,
    PlanNotFound,
    PlanDigestMismatch,
    PlanExpired,
    PlanTooLarge,
    UnsupportedChange,
    Unauthenticated,
    Forbidden,
    InvalidCredentialParameters,
    PrincipalConflict,
    EntropyUnavailable,
    SchemaNotReady,
    RevisionConflict,
    IdempotencyConflict,
    IdempotencyGone,
    InvalidPrecondition,
    InvalidCursor,
    CursorExpired,
    InvalidPageSize,
    UnpublishedEvents,
    CounterExhausted,
    InvalidStoredData,
    Database(sqlx::Error),
    Migration(sqlx::migrate::MigrateError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Do not expose SQL parameters, connection strings or stored input.
        f.write_str(match self {
            Self::StaleReconcileLease => "reconciliation lease is expired or no longer owned",
            Self::DispatchAlreadyStarted => {
                "dispatch may already have occurred; observe the stable effect identity"
            }
            Self::InvalidReconcileResult => "invalid reconciliation result or effect binding",
            Self::OperationNotBlocked => "operation is not blocked",
            Self::PreconditionRequired => "expected revision is required for existing definitions",
            Self::ReferenceUnavailable => {
                "reference is missing, inaccessible, disabled or incompatible"
            }
            Self::PlanNotFound => "plan or operation is unavailable",
            Self::PlanDigestMismatch => "plan digest does not match",
            Self::PlanExpired => "plan expired; create a new plan",
            Self::PlanTooLarge => "plan exceeds its size limit",
            Self::UnsupportedChange => "this definition update requires a separate data migration",
            Self::Unauthenticated => "invalid or inactive credential",
            Self::Forbidden => "credential does not grant the required scope",
            Self::InvalidCredentialParameters => "invalid credential parameters",
            Self::PrincipalConflict => "principal is disabled or has a different kind",
            Self::EntropyUnavailable => "secure random source unavailable",
            Self::SchemaNotReady => "database schema is not ready",
            Self::RevisionConflict => "declaration revision conflict",
            Self::IdempotencyConflict => "idempotency key has different intent",
            Self::IdempotencyGone => "idempotency key is retired",
            Self::InvalidPrecondition => "invalid declaration revision precondition",
            Self::InvalidCursor => "invalid event cursor",
            Self::CursorExpired => "event cursor expired; obtain a new snapshot",
            Self::InvalidPageSize => "page size must be between 1 and 1000",
            Self::UnpublishedEvents => "unacknowledged outbox events prevent retention",
            Self::CounterExhausted => "persistent counter exhausted",
            Self::InvalidStoredData => "invalid persistent declaration data",
            Self::Database(_) => "database operation failed; write outcome may be unknown",
            Self::Migration(_) => "database migration failed",
        })
    }
}

impl std::error::Error for Error {}
impl From<sqlx::Error> for Error {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}
impl From<sqlx::migrate::MigrateError> for Error {
    fn from(value: sqlx::migrate::MigrateError) -> Self {
        Self::Migration(value)
    }
}
pub type Result<T> = std::result::Result<T, Error>;
