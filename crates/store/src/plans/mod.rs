//! Authorized desired-state planning and atomic publication of reconcile intents.
//! No Kubernetes, storage backend, browser or process work runs in these transactions.
mod access;
mod apply;
mod build;
mod references;
mod transactions;
mod types;
pub use types::*;
