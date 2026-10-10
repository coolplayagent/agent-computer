//! Single-use PID-namespace init for a credential-free, isolated execution container.
//!
//! This is a local supervisor, not an authorization service or a physical fencing
//! authority. Its report must never release a durable writer lease by itself.
#![forbid(unsafe_code)]

mod output;
pub mod renewal;
mod request;
mod startup;
mod supervisor;

pub use output::Output;
pub use request::{MAX_REQUEST_BYTES, Request};
pub use startup::{
    Bootstrap, STARTUP_PROTOCOL, STARTUP_WAIT_MS, StartupChallenge, StartupGrant, StartupHello,
    StartupReport, startup, startup_attached,
};
pub use supervisor::{Outcome, Report, run};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRequest,
    IsolationRequired,
    Setup,
    StartupExpired,
    StartupCancelled,
    LeaseExpired,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sandbox supervisor: {self:?}")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
