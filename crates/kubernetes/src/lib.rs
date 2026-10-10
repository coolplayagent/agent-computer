//! Constrained Kubernetes transport for explicitly admitted, ephemeral sandbox instances.
//!
//! This library does not authorize users, allocate generations, prepare storage, or prove
//! physical fencing. Its caller must persist admission and instance identity before dispatch.
//! Creating a declaration alone is never a reason to call `create`.
#![forbid(unsafe_code)]

mod attach;
mod candidate;
mod client;
mod node;
pub use node::{NodeIdentity, PodRuntimeIdentity};
mod plan;
mod startup_plan;
mod verify;
pub mod volume;

pub use attach::{
    ExecutionChannel, ExecutionEvent, OutputChunkObservation, StartupChannel, StartupObservation,
};
pub use candidate::CandidateMount;
pub use client::{Client, DeleteOutcome, Deployment, PodObservation, PodPhase};
pub use plan::{EphemeralSandboxPlan, InstanceIdentity};
pub use startup_plan::StartupSandboxPlan;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidConfiguration,
    UnsupportedSandbox,
    UnsupportedVolume,
    InvalidIdentity,
    InvalidCommand,
    Transport,
    ResponseLimit,
    InvalidResponse,
    AccessDenied,
    ApiRejected,
    PreconditionFailed,
    IdentityMismatch,
    ExistingObject,
    /// A mutation may have happened. Observe the persisted identity; never blindly redispatch.
    MutationUnconfirmed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately exclude upstream bodies, endpoint URLs and credentials.
        write!(f, "Kubernetes adapter: {self:?}")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests;
