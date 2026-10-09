//! Trusted Candidate materialization on a qualified, mounted JuiceFS volume.
//! A prepared directory is not a writer grant, lease, or physical fencing proof.
#![forbid(unsafe_code)]
#[cfg(not(target_os = "linux"))]
compile_error!("Candidate storage currently requires Linux openat2 and renameat2");

mod directory;
mod materialize;
mod model;
pub mod quota;

pub use materialize::{MountedVolume, ObjectCache, ObjectSource};
pub use model::{Entry, Manifest, PrepareRequest, Prepared};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRequest,
    InputMismatch,
    IdentityConflict,
    InvalidFilesystemObject,
    UnsupportedBackend,
    QuotaUnavailable,
    Io,
}
pub type Result<T> = std::result::Result<T, Error>;
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidRequest => "invalid preparation request",
            Self::InputMismatch => "input integrity verification failed",
            Self::IdentityConflict => "candidate identity conflicts with retained storage",
            Self::InvalidFilesystemObject => "unexpected filesystem object",
            Self::UnsupportedBackend => "qualified JuiceFS storage is required",
            Self::QuotaUnavailable => "directory quota was not confirmed",
            Self::Io => "storage operation was not confirmed",
        })
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}
impl From<rustix::io::Errno> for Error {
    fn from(_: rustix::io::Errno) -> Self {
        Self::Io
    }
}

#[cfg(test)]
mod tests;
