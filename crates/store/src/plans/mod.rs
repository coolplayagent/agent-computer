//! Authorized desired-state planning and atomic publication of reconcile intents.
//! No Kubernetes, storage backend, browser or process work runs in these transactions.
pub(crate) mod access;
pub(crate) mod apply;
mod build;
mod references;
pub(crate) mod transactions;
pub(crate) mod types;
pub use types::*;
